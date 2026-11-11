//! The usage insights report (spec: observability, Usage insights report):
//! what a profile did over a window, from the usage ledger and the session
//! store, without any model call. Every session counts towards the source
//! its lineage's root started from (a surface such as `cli`, or a
//! messaging platform such as `telegram`), so a subagent of a Telegram
//! conversation is Telegram usage, and a source filter keeps whole
//! lineages.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_core::text::Catalog;
use pmt_store::{Activity, Store, Tokens, UsageFilter};
use serde::Serialize;

/// What the report covers.
#[derive(Debug, Clone)]
pub struct Window {
    /// Its start.
    pub since: Timestamp,
    /// Its length in days (the heading).
    pub days: u32,
    /// Only lineages started from this source.
    pub source: Option<String>,
    /// The profile's time zone (days and the activity matrix).
    pub zone: TimeZone,
}

/// One group of a breakdown.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Group {
    /// The group (`openai/gpt-x`, `openai`, `telegram`).
    pub key: String,
    /// Calls.
    pub calls: u64,
    /// Tokens of every direction.
    pub tokens: u64,
    /// Priced cost.
    pub cost_usd: f64,
    /// Calls without a price.
    pub unpriced_calls: u64,
}

/// A tool's calls.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolUse {
    /// The tool.
    pub tool: String,
    /// Results.
    pub calls: u64,
    /// Share that did their job.
    pub success_rate: f64,
}

/// A root lineage's spend.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RootUse {
    /// The root session.
    pub root_session_id: String,
    /// Priced cost of the lineage.
    pub cost_usd: f64,
    /// Tokens of the lineage.
    pub tokens: u64,
}

/// The overview.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Overview {
    /// Sessions with activity in the window.
    pub sessions: u64,
    /// Messages the person wrote.
    pub user_messages: u64,
    /// Turns that called a model.
    pub turns: u64,
    /// Tool results.
    pub tool_calls: u64,
    /// Days with any activity.
    pub active_days: u64,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cache-read tokens.
    pub cache_read_tokens: u64,
    /// Cache-write tokens.
    pub cache_write_tokens: u64,
    /// Reasoning tokens.
    pub reasoning_tokens: u64,
}

/// The report.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Insights {
    /// Its length in days.
    pub days: u32,
    /// The source filter.
    pub source: Option<String>,
    /// Sources with activity in the window.
    pub sources: Vec<String>,
    /// The overview.
    pub overview: Overview,
    /// Priced cost.
    pub cost_usd: f64,
    /// Calls without a price.
    pub unpriced_calls: u64,
    /// Calls by `<provider>/<model>`.
    pub by_model: Vec<Group>,
    /// Calls by provider.
    pub by_provider: Vec<Group>,
    /// Calls by source.
    pub by_source: Vec<Group>,
    /// The 10 tools called most, with their success rate.
    pub top_tools: Vec<ToolUse>,
    /// Skill loads by skill.
    pub skills: BTreeMap<String, u64>,
    /// Turns by weekday (Monday first) and hour, in the profile's zone.
    pub activity: Vec<Vec<u64>>,
    /// The 5 costliest root lineages.
    pub top_roots_by_cost: Vec<RootUse>,
    /// The 5 root lineages that used the most tokens.
    pub top_roots_by_tokens: Vec<RootUse>,
}

fn total(tokens: &Tokens) -> u64 {
    tokens.input + tokens.output + tokens.cache_read + tokens.cache_write
}

#[derive(Default)]
struct Acc {
    overview: Overview,
    cost_usd: f64,
    unpriced_calls: u64,
    by_model: HashMap<String, Group>,
    by_provider: HashMap<String, Group>,
    by_source: HashMap<String, Group>,
    roots: HashMap<String, RootUse>,
    turns: HashMap<(SessionId, String), Timestamp>,
    days: BTreeSet<jiff::civil::Date>,
    tools: HashMap<String, (u64, u64)>,
    skills: BTreeMap<String, u64>,
}

fn add(groups: &mut HashMap<String, Group>, key: &str, tokens: u64, cost: Option<f64>) {
    let group = groups.entry(key.to_owned()).or_insert_with(|| Group {
        key: key.to_owned(),
        ..Group::default()
    });
    group.calls += 1;
    group.tokens += tokens;
    match cost {
        Some(cost) => group.cost_usd += cost,
        None => group.unpriced_calls += 1,
    }
}

/// The insights of `store` over `window`.
///
/// # Errors
///
/// `usage.unknown_source` (2) naming the sources present when the filter
/// matches none of them; store errors.
pub fn insights(store: &Store, window: &Window) -> Result<Insights, Error> {
    let labeled = store.labeled_sessions(window.since)?;
    let sources: Vec<String> = labeled
        .iter()
        .map(|s| s.source.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if let Some(source) = &window.source
        && !sources.contains(source)
    {
        let present = if sources.is_empty() {
            "none".to_owned()
        } else {
            sources.join(", ")
        };
        return Err(Error::new(
            &codes::USAGE_UNKNOWN_SOURCE,
            format!("no activity from source '{source}' in the window; sources present: {present}"),
        ));
    }
    let wanted = |label: &str| window.source.as_deref().is_none_or(|s| s == label);
    let label_of: HashMap<SessionId, String> = labeled
        .iter()
        .filter(|s| wanted(&s.source))
        .map(|s| (s.id, s.source.clone()))
        .collect();
    let mut acc = Acc::default();
    acc.overview.sessions = label_of.len() as u64;
    ledger(store, window, &label_of, &mut acc)?;
    activity(store, window, &label_of, &mut acc)?;
    Ok(finish(acc, window, sources))
}

fn ledger(
    store: &Store,
    window: &Window,
    label_of: &HashMap<SessionId, String>,
    acc: &mut Acc,
) -> Result<(), Error> {
    let filter = UsageFilter {
        since: Some(window.since),
        ..UsageFilter::default()
    };
    store.for_each_usage(&filter, |row, root| {
        let Some(source) = label_of.get(&row.session_id) else {
            return Ok(());
        };
        let tokens = total(&row.tokens);
        let o = &mut acc.overview;
        o.input_tokens += row.tokens.input;
        o.output_tokens += row.tokens.output;
        o.cache_read_tokens += row.tokens.cache_read;
        o.cache_write_tokens += row.tokens.cache_write;
        o.reasoning_tokens += row.tokens.reasoning;
        match row.cost_usd {
            Some(cost) => acc.cost_usd += cost,
            None => acc.unpriced_calls += 1,
        }
        add(
            &mut acc.by_model,
            &format!("{}/{}", row.provider, row.model),
            tokens,
            row.cost_usd,
        );
        add(&mut acc.by_provider, &row.provider, tokens, row.cost_usd);
        add(&mut acc.by_source, source, tokens, row.cost_usd);
        let root_id = root.to_string();
        let lineage = acc.roots.entry(root_id.clone()).or_insert(RootUse {
            root_session_id: root_id,
            cost_usd: 0.0,
            tokens: 0,
        });
        lineage.cost_usd += row.cost_usd.unwrap_or(0.0);
        lineage.tokens += tokens;
        if let Some(turn) = &row.turn_id {
            let first = acc
                .turns
                .entry((row.session_id, turn.clone()))
                .or_insert(row.ts);
            *first = (*first).min(row.ts);
        }
        acc.days.insert(row.ts.to_zoned(window.zone.clone()).date());
        Ok(())
    })
}

fn activity(
    store: &Store,
    window: &Window,
    label_of: &HashMap<SessionId, String>,
    acc: &mut Acc,
) -> Result<(), Error> {
    for row in store.activity_since(window.since)? {
        if !label_of.contains_key(&row.session_id) {
            continue;
        }
        match row.activity {
            Activity::UserMessage => {
                acc.overview.user_messages += 1;
                acc.days.insert(row.at.to_zoned(window.zone.clone()).date());
            }
            Activity::ToolResult { tool, failed } => {
                acc.overview.tool_calls += 1;
                let entry = acc.tools.entry(tool).or_default();
                entry.0 += 1;
                entry.1 += u64::from(!failed);
            }
            Activity::SkillLoad { skill } => *acc.skills.entry(skill).or_default() += 1,
        }
    }
    Ok(())
}

fn sorted(groups: HashMap<String, Group>) -> Vec<Group> {
    let mut groups: Vec<Group> = groups.into_values().collect();
    groups.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.key.cmp(&b.key)));
    groups
}

#[allow(clippy::cast_precision_loss)] // counts far below 2^52
fn finish(acc: Acc, window: &Window, sources: Vec<String>) -> Insights {
    let mut overview = acc.overview;
    overview.turns = acc.turns.len() as u64;
    overview.active_days = acc.days.len() as u64;
    let mut matrix = vec![vec![0u64; 24]; 7];
    for at in acc.turns.values() {
        let local = at.to_zoned(window.zone.clone());
        let day = usize::from(local.weekday().to_monday_zero_offset().unsigned_abs());
        let hour = usize::from(local.hour().unsigned_abs());
        matrix[day.min(6)][hour.min(23)] += 1;
    }
    let mut tools: Vec<ToolUse> = acc
        .tools
        .into_iter()
        .map(|(tool, (calls, ok))| ToolUse {
            tool,
            calls,
            success_rate: if calls == 0 {
                0.0
            } else {
                ok as f64 / calls as f64
            },
        })
        .collect();
    tools.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.tool.cmp(&b.tool)));
    tools.truncate(10);
    let roots: Vec<RootUse> = acc.roots.into_values().collect();
    let mut by_cost = roots.clone();
    by_cost.sort_by(|a, b| {
        b.cost_usd
            .total_cmp(&a.cost_usd)
            .then_with(|| a.root_session_id.cmp(&b.root_session_id))
    });
    by_cost.truncate(5);
    let mut by_tokens = roots;
    by_tokens.sort_by(|a, b| {
        b.tokens
            .cmp(&a.tokens)
            .then_with(|| a.root_session_id.cmp(&b.root_session_id))
    });
    by_tokens.truncate(5);
    Insights {
        days: window.days,
        source: window.source.clone(),
        sources,
        overview,
        cost_usd: acc.cost_usd,
        unpriced_calls: acc.unpriced_calls,
        by_model: sorted(acc.by_model),
        by_provider: sorted(acc.by_provider),
        by_source: sorted(acc.by_source),
        top_tools: tools,
        skills: acc.skills,
        activity: matrix,
        top_roots_by_cost: by_cost,
        top_roots_by_tokens: by_tokens,
    }
}

/// The report as text, worded with `text` (the CLI passes English,
/// `/insights` the profile's language).
#[must_use]
pub fn render(text: &dyn Catalog, insights: &Insights) -> String {
    let mut out = String::new();
    let days = insights.days;
    let heading = match insights.source.as_deref() {
        Some(source) => text.text(
            "slash.insights.heading_source",
            &[("days", &days), ("source", &source)],
        ),
        None => text.text("slash.insights.heading", &[("days", &days)]),
    };
    let _ = writeln!(out, "{heading}");
    let o = &insights.overview;
    let overview = text.text(
        "slash.insights.overview",
        &[
            ("sessions", &o.sessions),
            ("messages", &o.user_messages),
            ("turns", &o.turns),
            ("calls", &o.tool_calls),
            ("days", &o.active_days),
        ],
    );
    let tokens = text.text(
        "slash.insights.tokens",
        &[
            ("input", &o.input_tokens),
            ("output", &o.output_tokens),
            ("cache_read", &o.cache_read_tokens),
            ("cache_write", &o.cache_write_tokens),
            ("reasoning", &o.reasoning_tokens),
        ],
    );
    let _ = writeln!(out, "{overview}\n{tokens}");
    let cost = if insights.unpriced_calls > 0 && insights.cost_usd == 0.0 {
        "—".to_owned()
    } else {
        format!("${:.4}", insights.cost_usd)
    };
    let cost = text.text(
        "slash.insights.cost",
        &[("cost", &cost), ("unpriced", &insights.unpriced_calls)],
    );
    let _ = writeln!(out, "{cost}");
    groups(
        text,
        &mut out,
        "slash.insights.by_model",
        &insights.by_model,
    );
    groups(
        text,
        &mut out,
        "slash.insights.by_provider",
        &insights.by_provider,
    );
    groups(
        text,
        &mut out,
        "slash.insights.by_source",
        &insights.by_source,
    );
    if !insights.top_tools.is_empty() {
        let _ = writeln!(out, "{}", text.text("slash.insights.top_tools", &[]));
        for t in &insights.top_tools {
            let rate = format!("{:>5.1}", t.success_rate * 100.0);
            let calls = format!("{:>6}", t.calls);
            let row = text.text(
                "slash.insights.tool_row",
                &[("calls", &calls), ("rate", &rate)],
            );
            let _ = writeln!(out, "  {:<24}{row}", t.tool);
        }
    }
    if !insights.skills.is_empty() {
        let _ = writeln!(out, "{}", text.text("slash.insights.skills", &[]));
        for (skill, loads) in &insights.skills {
            let _ = writeln!(out, "  {skill:<24}{loads:>6}");
        }
    }
    activity_matrix(text, &mut out, &insights.activity);
    roots(
        text,
        &mut out,
        "slash.insights.top_by_cost",
        &insights.top_roots_by_cost,
    );
    roots(
        text,
        &mut out,
        "slash.insights.top_by_tokens",
        &insights.top_roots_by_tokens,
    );
    out
}

fn groups(text: &dyn Catalog, out: &mut String, title: &str, groups: &[Group]) {
    if groups.is_empty() {
        return;
    }
    let _ = writeln!(out, "{}", text.text(title, &[]));
    for g in groups {
        let cost = if g.calls == g.unpriced_calls {
            "—".to_owned()
        } else {
            format!("${:.4}", g.cost_usd)
        };
        let calls = format!("{:>6}", g.calls);
        let tokens = format!("{:>10}", g.tokens);
        let row = text.text(
            "slash.insights.group_row",
            &[("calls", &calls), ("tokens", &tokens), ("cost", &cost)],
        );
        let _ = writeln!(out, "  {:<32}{row}", g.key);
    }
}

fn roots(text: &dyn Catalog, out: &mut String, title: &str, roots: &[RootUse]) {
    if roots.is_empty() {
        return;
    }
    let _ = writeln!(out, "{}", text.text(title, &[]));
    for r in roots {
        let cost = format!("${:.4}", r.cost_usd);
        let row = text.text(
            "slash.insights.root_row",
            &[("cost", &cost), ("tokens", &r.tokens)],
        );
        let _ = writeln!(out, "  {}  {row}", r.root_session_id);
    }
}

fn activity_matrix(catalog: &dyn Catalog, text: &mut String, matrix: &[Vec<u64>]) {
    if matrix.iter().flatten().all(|n| *n == 0) {
        return;
    }
    let _ = write!(
        text,
        "{}\n     ",
        catalog.text("slash.insights.activity", &[])
    );
    for hour in 0..24 {
        let _ = write!(text, "{hour:>3}");
    }
    text.push('\n');
    let seen: HashSet<usize> = (0..7).collect();
    // Seven space-separated names, Monday first, three columns wide.
    let names = catalog.text("slash.insights.weekdays", &[]);
    for (day, name) in names.split_whitespace().take(7).enumerate() {
        if !seen.contains(&day) {
            continue;
        }
        let _ = write!(text, "  {name:<3}");
        for count in &matrix[day] {
            let _ = write!(text, "{count:>3}");
        }
        text.push('\n');
    }
}
