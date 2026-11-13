//! The cost and usage view's data (spec: web-dashboard, Cost and usage view
//! (upgrade)): the usage ledger rows a filter selects, attributed to their
//! root session lineage (children and continuations under their root),
//! with daily totals, the prompt-cache hit ratio and the spend against the
//! profile's daily and monthly budgets; the same rows as CSV.

use std::collections::{BTreeMap, HashMap};

use jiff::{Timestamp, ToSpan as _, Zoned};
use pmt_core::id::SessionId;
use pmt_store::{UsageEvent, UsageFilter};
use serde_json::{Value, json};

use super::profile::Profile;
use crate::http::{Reply, Req, bytes, error, from_error, json_reply};

/// What a request selects beyond the ledger's time range.
#[derive(Debug, Default)]
struct Narrow {
    model: Option<String>,
    provider: Option<String>,
    source: Option<String>,
}

/// One selected ledger row with its root and the session's source.
struct Row {
    event: UsageEvent,
    root: SessionId,
}

#[derive(Debug, Default, Clone, Copy)]
struct Sum {
    cost: f64,
    input: u64,
    output: u64,
    cache_read: u64,
    calls: u64,
}

impl Sum {
    fn add(&mut self, event: &UsageEvent) {
        self.cost += event.cost_usd.unwrap_or(0.0);
        self.input += event.tokens.input;
        self.output += event.tokens.output;
        self.cache_read += event.tokens.cache_read;
        self.calls += 1;
    }

    fn json(self) -> Value {
        let prompt = self.input + self.cache_read;
        #[allow(clippy::cast_precision_loss)] // token counts are far below 2^52
        let ratio = if prompt == 0 {
            0.0
        } else {
            self.cache_read as f64 / prompt as f64
        };
        json!({"cost_usd": round(self.cost), "input_tokens": self.input,
               "output_tokens": self.output, "cache_read_tokens": self.cache_read,
               "calls": self.calls, "cache_hit_ratio": ratio})
    }
}

fn round(usd: f64) -> f64 {
    (usd * 1_000_000.0).round() / 1_000_000.0
}

/// `YYYY-MM-DD` at midnight in `zone`, or an RFC 3339 time.
fn time(text: &str, zone: &jiff::tz::TimeZone) -> Option<Timestamp> {
    text.parse::<Timestamp>().ok().or_else(|| {
        let date: jiff::civil::Date = text.parse().ok()?;
        date.to_zoned(zone.clone()).ok().map(|z| z.timestamp())
    })
}

fn filter(req: &Req, zone: &jiff::tz::TimeZone) -> Result<(UsageFilter, Narrow), Reply> {
    let bound = |name: &str| match req.param(name) {
        None => Ok(None),
        Some(text) => time(&text, zone).map(Some).ok_or_else(|| {
            error(
                400,
                "invalid_date",
                &format!("{name}: '{text}' is not a date such as 2026-11-01"),
            )
        }),
    };
    let until = bound("to")?.map(|t| t + 24.hours());
    let ledger = UsageFilter {
        since: bound("from")?,
        until,
        ..UsageFilter::default()
    };
    let narrow = Narrow {
        model: req.param("model"),
        provider: req.param("provider"),
        source: req.param("source"),
    };
    Ok((ledger, narrow))
}

fn rows(
    at: &Profile<'_>,
    ledger: &UsageFilter,
    narrow: &Narrow,
) -> Result<Vec<Row>, pmt_core::Error> {
    let mut sources: HashMap<SessionId, String> = HashMap::new();
    let mut selected = Vec::new();
    at.store.for_each_usage(ledger, |event, root| {
        let source = sources
            .entry(event.session_id)
            .or_insert_with(|| {
                at.store
                    .session(event.session_id)
                    .map_or_else(|_| String::new(), |s| s.source.as_str().to_owned())
            })
            .clone();
        let keep = narrow.model.as_ref().is_none_or(|m| *m == event.model)
            && narrow
                .provider
                .as_ref()
                .is_none_or(|p| *p == event.provider)
            && narrow.source.as_ref().is_none_or(|s| *s == source);
        if keep {
            selected.push(Row { event, root });
        }
        Ok(())
    })?;
    Ok(selected)
}

/// Each root lineage with its own calls and every child session's.
fn lineages(at: &Profile<'_>, rows: &[Row]) -> Vec<Value> {
    let mut by_root: BTreeMap<SessionId, BTreeMap<SessionId, Sum>> = BTreeMap::new();
    for row in rows {
        by_root
            .entry(row.root)
            .or_default()
            .entry(row.event.session_id)
            .or_default()
            .add(&row.event);
    }
    let title = |id: SessionId| at.store.session(id).ok().and_then(|s| s.title);
    by_root
        .into_iter()
        .map(|(root, sessions)| {
            let mut total = Sum::default();
            for sum in sessions.values() {
                total.cost += sum.cost;
                total.input += sum.input;
                total.output += sum.output;
                total.cache_read += sum.cache_read;
                total.calls += sum.calls;
            }
            let own = sessions.get(&root).copied().unwrap_or_default();
            let children: Vec<Value> = sessions
                .iter()
                .filter(|(id, _)| **id != root)
                .map(|(id, sum)| json!({"session_id": id.to_string(), "title": title(*id), "usage": sum.json()}))
                .collect();
            json!({"root_session_id": root.to_string(), "title": title(root),
                   "total": total.json(), "own": own.json(), "children": children})
        })
        .collect()
}

fn daily(rows: &[Row], zone: &jiff::tz::TimeZone) -> Vec<Value> {
    let mut days: BTreeMap<String, Sum> = BTreeMap::new();
    for row in rows {
        let day = row.event.ts.to_zoned(zone.clone()).date().to_string();
        days.entry(day).or_default().add(&row.event);
    }
    days.into_iter()
        .map(|(date, sum)| json!({"date": date, "usage": sum.json()}))
        .collect()
}

/// Spend this day and month against `budgets.daily_usd` and `monthly_usd`.
fn budgets(at: &Profile<'_>, zone: &jiff::tz::TimeZone) -> Result<Vec<Value>, pmt_core::Error> {
    let config = &at.scope.config().budgets;
    let now = Zoned::now().with_time_zone(zone.clone());
    let day = now.start_of_day().map_err(|e| {
        pmt_core::Error::new(&pmt_core::error::codes::CONFIG_TIMEZONE, e.to_string())
    })?;
    let month = now
        .first_of_month()
        .and_then(|z| z.start_of_day())
        .map_err(|e| {
            pmt_core::Error::new(&pmt_core::error::codes::CONFIG_TIMEZONE, e.to_string())
        })?;
    let mut out = Vec::new();
    for (scope, limit, since) in [
        ("daily_usd", config.daily_usd, day),
        ("monthly_usd", config.monthly_usd, month),
    ] {
        if limit <= 0.0 {
            continue;
        }
        let spent = at
            .store
            .usage_sum(&UsageFilter {
                since: Some(since.timestamp()),
                ..UsageFilter::default()
            })?
            .cost_usd;
        out.push(
            json!({"scope": scope, "limit": limit, "spent": round(spent),
                        "percent": round(spent / limit * 100.0)}),
        );
    }
    Ok(out)
}

fn zone(at: &Profile<'_>) -> jiff::tz::TimeZone {
    pmt_obs::time::profile_zone(&at.scope.config().cron).unwrap_or(jiff::tz::TimeZone::UTC)
}

/// `GET usage[?from=&to=&model=&provider=&source=]`.
pub(crate) fn report(at: &Profile<'_>, req: &Req) -> Reply {
    let zone = zone(at);
    let (ledger, narrow) = match filter(req, &zone) {
        Ok(selection) => selection,
        Err(reply) => return reply,
    };
    let selected = match rows(at, &ledger, &narrow) {
        Ok(rows) => rows,
        Err(err) => return from_error(&err),
    };
    let mut total = Sum::default();
    for row in &selected {
        total.add(&row.event);
    }
    let budgets = match budgets(at, &zone) {
        Ok(budgets) => budgets,
        Err(err) => return from_error(&err),
    };
    json_reply(
        200,
        &json!({"total": total.json(), "lineages": lineages(at, &selected),
                "daily": daily(&selected, &zone), "budgets": budgets}),
    )
}

/// One CSV field, quoted when it needs to be.
fn field(text: &str) -> String {
    if text.contains([',', '"', '\n']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_owned()
    }
}

/// `GET usage.csv`: the filtered rows.
pub(crate) fn csv(at: &Profile<'_>, req: &Req) -> Reply {
    let zone = zone(at);
    let (ledger, narrow) = match filter(req, &zone) {
        Ok(selection) => selection,
        Err(reply) => return reply,
    };
    let selected = match rows(at, &ledger, &narrow) {
        Ok(rows) => rows,
        Err(err) => return from_error(&err),
    };
    let mut out = String::from(
        "ts,session_id,root_session_id,provider,model,purpose,input_tokens,output_tokens,cache_read_tokens,cost_usd\n",
    );
    for Row { event, root } in &selected {
        let cost = event.cost_usd.map_or_else(String::new, |c| c.to_string());
        let line = [
            event.ts.to_string(),
            event.session_id.to_string(),
            root.to_string(),
            field(&event.provider),
            field(&event.model),
            field(&event.purpose),
            event.tokens.input.to_string(),
            event.tokens.output.to_string(),
            event.tokens.cache_read.to_string(),
            cost,
        ];
        out.push_str(&line.join(","));
        out.push('\n');
    }
    let mut reply = bytes(200, "text/csv; charset=utf-8", out);
    reply.headers_mut().insert(
        hyper::header::CONTENT_DISPOSITION,
        hyper::header::HeaderValue::from_static("attachment; filename=\"usage.csv\""),
    );
    reply
}
