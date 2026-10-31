//! `prometheus usage` (spec: observability, Usage command, Totals keyed by
//! root lineage, Pricing and unpriced calls; cli).

mod common;

use std::path::Path;
use std::sync::Arc;

use jiff::{Timestamp, ToSpan as _};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::id::SessionId;
use pmt_obs::usage::{Call, ConfigPrices, UsageRecorder};
use pmt_store::{LineageKind, NewSession, Source, Store, Tokens};
use serde_json::Value;

use common::{ok, run, stderr, temp_home};

fn store(home: &Path, profile: &str) -> Arc<Store> {
    let home = Home::open(home).unwrap();
    let name = ProfileName::parse(profile).unwrap();
    let dir = home.profile(&name).unwrap();
    let scope = ProfileScope::open(dir, &Overrides::default(), Launch::Other).unwrap();
    Arc::new(Store::open(&scope).unwrap())
}

fn session(store: &Store, parent: Option<(SessionId, LineageKind)>) -> SessionId {
    let new = match parent {
        Some((parent, kind)) => NewSession::child(Source::Cli, parent, kind),
        None => NewSession::new(Source::Cli),
    };
    store.create_session(&new).unwrap().id
}

/// No price table: every call is unpriced unless it reports a cost.
fn recorder(store: &Arc<Store>) -> UsageRecorder {
    UsageRecorder::new(Arc::clone(store), Arc::new(ConfigPrices::default()))
}

fn call(
    rec: &UsageRecorder,
    session: SessionId,
    purpose: &str,
    input: u64,
    cost: Option<f64>,
    at: Timestamp,
) {
    rec.record_at(
        Call {
            tokens: Tokens {
                input,
                output: input / 10,
                cache_read: 1,
                cache_write: 2,
                reasoning: 3,
            },
            reported_usd: cost,
            billing_source: if cost.is_some() {
                "api_key:test"
            } else {
                "local"
            }
            .to_owned(),
            ..Call::new(session, purpose, "test", "m1")
        },
        at,
    )
    .unwrap();
}

fn json(home: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--json", "usage"];
    full.extend_from_slice(args);
    serde_json::from_str(&ok(&run(home, &full))).unwrap()
}

#[test]
fn a_root_session_counts_its_continuation_and_subagents() {
    let home = temp_home();
    let store = store(home.path(), "default");
    let rec = recorder(&store);
    let now = Timestamp::now();
    let root = session(&store, None);
    let continuation = session(&store, Some((root, LineageKind::Compression)));
    let sub_a = session(&store, Some((root, LineageKind::Delegate)));
    let sub_b = session(&store, Some((continuation, LineageKind::Delegate)));
    let other = session(&store, None);
    let mut expected_input = 0;
    for (id, calls) in [(root, 3), (continuation, 2), (sub_a, 4), (sub_b, 4)] {
        for i in 0..calls {
            let input = 100 * (i + 1);
            expected_input += input;
            call(&rec, id, "main", input, Some(0.01), now);
        }
    }
    call(&rec, other, "main", 9_999, Some(1.0), now);
    drop(store);

    let doc = json(home.path(), &["--session", &root.to_string()]);
    assert_eq!(doc["schema"], "prometheus.usage.v1");
    let total = &doc["total"];
    assert_eq!(total["calls"], 13);
    assert_eq!(total["input_tokens"], expected_input);
    assert_eq!(total["output_tokens"], expected_input / 10);
    assert_eq!(total["cache_read_tokens"], 13);
    assert_eq!(total["cache_write_tokens"], 26);
    assert_eq!(total["reasoning_tokens"], 39);
    assert!((total["cost_usd"].as_f64().unwrap() - 0.13).abs() < 1e-9);
    assert_eq!(total["unpriced_calls"], 0);

    // By root: the whole profile in two lineages.
    let doc = json(home.path(), &["--by", "root"]);
    let rows = doc["profiles"][0]["rows"].as_array().unwrap();
    let calls: Vec<(String, u64)> = rows
        .iter()
        .map(|r| {
            (
                r["key"].as_str().unwrap().to_owned(),
                r["calls"].as_u64().unwrap(),
            )
        })
        .collect();
    let mut expected = vec![(root.to_string(), 13), (other.to_string(), 1)];
    expected.sort();
    assert_eq!(calls, expected);
}

#[test]
fn a_local_model_without_a_price_is_an_unpriced_call_not_zero_cost() {
    let home = temp_home();
    let store = store(home.path(), "default");
    let id = session(&store, None);
    call(&recorder(&store), id, "main", 500, None, Timestamp::now());
    drop(store);

    let doc = json(home.path(), &[]);
    assert_eq!(doc["total"]["unpriced_calls"], 1);
    assert_eq!(doc["total"]["calls"], 1);
    assert!(doc["total"]["cost_usd"].is_null(), "{doc}");

    let text = ok(&run(home.path(), &["usage"]));
    assert!(
        text.ends_with("total: 1 calls, cost_usd —, unpriced_calls: 1\n"),
        "{text}"
    );
    assert!(!text.contains("$0.00"), "{text}");
    assert!(text.contains(", by day ("), "{text}");
}

#[test]
fn a_breakdown_by_purpose_sums_to_the_total_within_the_window() {
    let home = temp_home();
    let store = store(home.path(), "default");
    let rec = recorder(&store);
    let now = Timestamp::now();
    let id = session(&store, None);
    let child = session(&store, Some((id, LineageKind::Delegate)));
    call(&rec, id, "main", 100, Some(0.5), now - 1.hour());
    call(&rec, id, "main", 200, Some(0.25), now - 2.hours());
    call(&rec, id, "auxiliary.title", 10, None, now - 1.hour());
    call(&rec, id, "compression", 300, Some(0.1), now - 72.hours());
    call(&rec, child, "subagent", 400, Some(0.05), now - 144.hours());
    call(&rec, id, "main", 1_000, Some(9.0), now - 192.hours()); // outside 7d
    drop(store);

    let doc = json(home.path(), &["--by", "purpose", "--since", "7d"]);
    let rows = doc["profiles"][0]["rows"].as_array().unwrap();
    let purposes: Vec<&str> = rows.iter().map(|r| r["key"].as_str().unwrap()).collect();
    assert_eq!(
        purposes,
        ["auxiliary.title", "compression", "main", "subagent"]
    );
    let total = &doc["total"];
    for field in ["calls", "input_tokens", "output_tokens", "unpriced_calls"] {
        let sum: u64 = rows.iter().map(|r| r[field].as_u64().unwrap()).sum();
        assert_eq!(total[field].as_u64().unwrap(), sum, "{field}");
    }
    let cost: f64 = rows.iter().filter_map(|r| r["cost_usd"].as_f64()).sum();
    assert!((total["cost_usd"].as_f64().unwrap() - cost).abs() < 1e-9);
    assert!((cost - 0.9).abs() < 1e-9);
    assert_eq!(total["calls"], 5);

    // --until cuts the window's end; the default window is 30 days.
    let doc = json(home.path(), &["--since", "7d", "--until", "2d"]);
    assert_eq!(doc["total"]["calls"], 2);
    assert_eq!(json(home.path(), &[])["total"]["calls"], 6);
    assert_eq!(doc["by"], "day");
}

#[test]
fn all_profiles_reports_each_profile_and_a_grand_total() {
    let home = temp_home();
    let now = Timestamp::now();
    for (profile, calls) in [("default", 2), ("work", 3)] {
        let store = store(home.path(), profile);
        let id = session(&store, None);
        for _ in 0..calls {
            call(&recorder(&store), id, "main", 10, Some(0.5), now);
        }
    }
    let doc = json(home.path(), &["--all-profiles", "--by", "profile"]);
    let profiles = doc["profiles"].as_array().unwrap();
    let per: Vec<(&str, u64)> = profiles
        .iter()
        .map(|p| {
            (
                p["profile"].as_str().unwrap(),
                p["total"]["calls"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(per, [("default", 2), ("work", 3)]);
    assert_eq!(profiles[1]["rows"][0]["key"], "work");
    assert_eq!(doc["total"]["calls"], 5);
    assert!((doc["total"]["cost_usd"].as_f64().unwrap() - 2.5).abs() < 1e-9);

    // Without the flag only the selected profile counts.
    assert_eq!(json(home.path(), &[])["total"]["calls"], 2);
    let text = ok(&run(
        home.path(),
        &["-p", "work", "usage", "--all-profiles"],
    ));
    assert!(text.contains("all profiles"), "{text}");
}

#[test]
fn invalid_times_and_a_bad_zone_fail_with_their_exit_codes() {
    let home = temp_home();
    store(home.path(), "default");
    for (flag, value) in [("--since", "yesterday"), ("--until", "5é")] {
        let out = run(home.path(), &["usage", flag, value]);
        assert_eq!(out.status.code(), Some(2));
        assert_eq!(
            stderr(&out),
            format!("error: invalid {flag} value '{value}': use 30m, 2h, 7d or an RFC 3339 time\n")
        );
    }
    let out = run(home.path(), &["usage", "--by", "week"]);
    assert_eq!(out.status.code(), Some(2));
    let out = run(
        home.path(),
        &["--set", "cron.timezone=Mars/Olympus", "usage"],
    );
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(stderr(&out).contains("unknown time zone 'Mars/Olympus'"));
    let out = run(
        home.path(),
        &["usage", "--session", "ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W"],
    );
    assert_eq!(out.status.code(), Some(66));
}

#[test]
fn the_budget_command_shows_spend_and_sets_a_limit_for_the_next_call() {
    use pmt_config::schema::BudgetsConfig;
    use prometheus::slash_handlers::budget;

    let home = temp_home();
    let store = store(home.path(), "default");
    let id = session(&store, None);
    call(
        &recorder(&store),
        id,
        "main",
        10,
        Some(0.25),
        Timestamp::now(),
    );
    let dir = Home::open(home.path())
        .unwrap()
        .existing_profile(&ProfileName::default_profile())
        .unwrap();
    let scope = ProfileScope::open(dir, &Overrides::default(), Launch::Other).unwrap();
    let off = BudgetsConfig {
        unattended_root_usd: 0.0,
        ..BudgetsConfig::default()
    };
    assert_eq!(
        budget(&scope, &store, id, &off, "").unwrap().0,
        "no budgets set"
    );

    let (text, updated) = budget(&scope, &store, id, &off, "set root 2").unwrap();
    assert_eq!(text, "budgets.root_usd = 2 (applies to the next call)");
    let updated = updated.unwrap();
    assert!((updated.root_usd - 2.0).abs() < f64::EPSILON);
    assert_eq!(
        ok(&run(home.path(), &["config", "get", "budgets.root_usd"])).trim(),
        "2.0"
    );
    let (_, again) = budget(&scope, &store, id, &updated, "set daily $1.5").unwrap();
    let again = again.unwrap();
    assert!((again.daily_usd - 1.5).abs() < f64::EPSILON);
    let status = budget(&scope, &store, id, &again, "").unwrap().0;
    assert_eq!(
        status,
        "budgets (warning at 80%):\n  root       $0.25 of $2.00\n  daily      $0.25 of $1.50"
    );
    for bad in ["set weekly 3", "set root lots", "raise root 3"] {
        let err = budget(&scope, &store, id, &again, bad).unwrap_err();
        assert_eq!(err.exit_code(), 2, "{bad}");
    }
    let err = budget(&scope, &store, id, &again, "set root -1").unwrap_err();
    assert_eq!(
        err.exit_code(),
        2,
        "a negative budget is refused by the schema"
    );
}

#[test]
fn the_usage_command_shows_the_session_cache_hit_rate() {
    use prometheus::slash_handlers::session_usage as usage;

    let home = temp_home();
    let store = store(home.path(), "default");
    let id = session(&store, None);
    assert!(usage(&store, id).unwrap().ends_with("cache hit rate —"));
    // 90,000 cached of 100,000 input tokens.
    recorder(&store)
        .record_at(
            Call {
                tokens: Tokens {
                    input: 10_000,
                    output: 500,
                    cache_read: 90_000,
                    cache_write: 0,
                    reasoning: 0,
                },
                billing_source: "local".to_owned(),
                ..Call::new(id, "main", "test", "m1")
            },
            Timestamp::now(),
        )
        .unwrap();
    let text = usage(&store, id).unwrap();
    assert!(text.contains("cache read 90000"), "{text}");
    assert!(text.ends_with("cache hit rate 90%"), "{text}");
}
