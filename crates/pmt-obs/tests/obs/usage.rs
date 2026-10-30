//! The usage ledger writer, pricing and totals (spec: observability,
//! Per-call usage ledger, Pricing and unpriced calls, Totals keyed by root
//! lineage, Usage command).

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_config::schema::{Config, ModelEntry, ModelPrice, ModelRoute};
use pmt_obs::usage::report::{GroupBy, report};
use pmt_obs::usage::{Call, ConfigPrices, format_cost, format_total_cost};
use pmt_store::{CostStatus, LineageKind, Tokens, UsageFilter, UsageScope};

use crate::support::{child, config_recorder, recorder, root, store, temp_dir, use_tokens};

#[test]
fn an_auxiliary_title_call_is_one_more_row_beside_the_main_call() {
    let dir = temp_dir();
    let store = store(dir.path());
    let rec = recorder(&store);
    let session = root(&store);
    let turn = Some("turn_1".to_owned());
    let main = rec
        .record(Call {
            turn_id: turn.clone(),
            billing_source: "api_key:test/work".to_owned(),
            tokens: Tokens {
                input: 1_000,
                output: 200,
                ..Tokens::default()
            },
            ..Call::new(session, "main", "test", "priced")
        })
        .unwrap();
    rec.record(Call {
        turn_id: turn,
        tokens: Tokens {
            input: 300,
            output: 12,
            ..Tokens::default()
        },
        ..Call::new(session, "auxiliary.title", "local", "title-mini")
    })
    .unwrap();

    let rows = store.usage_events(session).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.iter().filter(|r| r.purpose == "main").count(), 1);
    let title = rows
        .iter()
        .find(|r| r.purpose == "auxiliary.title")
        .unwrap();
    assert_eq!(title.model, "title-mini");
    assert_eq!(title.cost_usd, None);
    assert_eq!(title.cost_status, CostStatus::Unknown);
    assert!(main.call_id.starts_with("call_"));
    assert_eq!(main.cost_status, CostStatus::Estimated);
    assert!((main.cost_usd.unwrap() - 0.0012).abs() < 1e-12);
}

#[test]
fn a_local_model_without_a_price_counts_as_unpriced_never_as_zero() {
    let dir = temp_dir();
    let store = store(dir.path());
    let session = root(&store);
    // A config price table without an entry for the local model.
    let mut config = Config::default();
    config.models.insert(
        "openai/gpt-x".to_owned(),
        ModelEntry::Route(ModelRoute {
            price: Some(ModelPrice {
                input: 2.0,
                output: 8.0,
                ..ModelPrice::default()
            }),
            ..ModelRoute::default()
        }),
    );
    let rec = config_recorder(&store, ConfigPrices::from_config(&config));
    use_tokens(&rec, session, 500, 50, Timestamp::now());

    let totals = store.usage_totals(UsageScope::Profile).unwrap();
    assert_eq!(totals.calls, 1);
    assert_eq!(totals.unpriced_calls, 1);
    assert_eq!(format_total_cost(&totals), "—");
    assert_eq!(format_cost(None), "—");

    // A priced call beside it: the total shows the priced sum, and the
    // unpriced call is still counted apart.
    rec.record(Call {
        tokens: Tokens {
            input: 1_000_000,
            ..Tokens::default()
        },
        billing_source: "api_key:openai".to_owned(),
        ..Call::new(session, "main", "openai", "gpt-x")
    })
    .unwrap();
    let totals = store.usage_totals(UsageScope::Profile).unwrap();
    assert_eq!((totals.calls, totals.unpriced_calls), (2, 1));
    assert_eq!(format_total_cost(&totals), "$2.0000");
}

#[test]
fn subscription_and_cached_calls_are_recorded_at_zero_with_their_status() {
    let dir = temp_dir();
    let store = store(dir.path());
    let rec = recorder(&store);
    let session = root(&store);
    let sub = rec
        .record(Call {
            billing_source: "subscription:anthropic/max".to_owned(),
            reported_usd: Some(3.0),
            ..Call::new(session, "main", "test", "priced")
        })
        .unwrap();
    assert_eq!(
        (sub.cost_usd, sub.cost_status),
        (Some(0.0), CostStatus::Subscription)
    );
    let cached = rec
        .record(Call {
            cache_hit: true,
            request_hash: Some("sha256:ab".to_owned()),
            ..Call::new(session, "main", "test", "priced")
        })
        .unwrap();
    assert_eq!(cached.cost_usd, Some(0.0));
    assert!(store.usage_events(session).unwrap()[1].cache_hit);
}

#[test]
fn continuation_and_child_sessions_roll_up_to_the_root() {
    let dir = temp_dir();
    let store = store(dir.path());
    let rec = recorder(&store);
    let root_id = root(&store);
    let continuation = child(&store, root_id, LineageKind::Compression);
    let sub_a = child(&store, root_id, LineageKind::Delegate);
    let sub_b = child(&store, continuation, LineageKind::Delegate);
    let unrelated = root(&store);
    let now = Timestamp::now();
    for (session, calls) in [
        (root_id, 3),
        (continuation, 2),
        (sub_a, 4),
        (sub_b, 4),
        (unrelated, 5),
    ] {
        for i in 0..calls {
            use_tokens(&rec, session, 100 + i, 10, now);
        }
    }

    // What `prometheus usage --session <root>` selects.
    let mut subtree = vec![root_id];
    subtree.extend(store.tree(root_id).unwrap().into_iter().map(|n| n.id));
    let filter = UsageFilter {
        sessions: Some(subtree),
        ..UsageFilter::default()
    };
    let summed = store.usage_sum(&filter).unwrap();
    assert_eq!(summed.calls, 13);
    let mut by_rows = pmt_store::UsageTotals::default();
    store
        .for_each_usage(&filter, |event, root_of| {
            assert_eq!(root_of, root_id);
            by_rows.add(&event);
            Ok(())
        })
        .unwrap();
    assert_eq!(by_rows, summed, "the total equals the sum of the rows");
    assert_eq!(
        store.usage_totals(UsageScope::Lineage(sub_b)).unwrap(),
        summed
    );

    let by_root = report(
        &store,
        "default",
        &UsageFilter::default(),
        GroupBy::Root,
        &TimeZone::UTC,
    )
    .unwrap();
    let keys: Vec<(&str, u64)> = by_root
        .rows
        .iter()
        .map(|r| (r.key.as_str(), r.totals.calls))
        .collect();
    let mut expected = [(root_id.to_string(), 13), (unrelated.to_string(), 5)];
    expected.sort();
    assert_eq!(
        keys,
        expected
            .iter()
            .map(|(k, n)| (k.as_str(), *n))
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_breakdown_by_purpose_sums_to_the_total() {
    let dir = temp_dir();
    let store = store(dir.path());
    let rec = recorder(&store);
    let session = root(&store);
    let child_id = child(&store, session, LineageKind::Delegate);
    let at = |s: &str| s.parse::<Timestamp>().unwrap();
    for (purpose, session, ts) in [
        ("main", session, "2026-05-01T10:00:00Z"),
        ("main", session, "2026-05-02T10:00:00Z"),
        ("auxiliary.title", session, "2026-05-02T10:00:01Z"),
        ("compression", session, "2026-05-03T23:30:00Z"),
        ("subagent", child_id, "2026-05-04T00:30:00Z"),
        ("main", session, "2026-04-01T00:00:00Z"),
    ] {
        rec.record_at(
            Call {
                tokens: Tokens {
                    input: 10,
                    output: 1,
                    ..Tokens::default()
                },
                billing_source: "api_key:test".to_owned(),
                ..Call::new(session, purpose, "test", "priced")
            },
            at(ts),
        )
        .unwrap();
    }
    let since = UsageFilter {
        since: Some(at("2026-04-26T00:00:00Z")),
        ..UsageFilter::default()
    };
    let by_purpose = report(&store, "default", &since, GroupBy::Purpose, &TimeZone::UTC).unwrap();
    let rows: Vec<(&str, u64)> = by_purpose
        .rows
        .iter()
        .map(|r| (r.key.as_str(), r.totals.calls))
        .collect();
    assert_eq!(
        rows,
        [
            ("auxiliary.title", 1),
            ("compression", 1),
            ("main", 2),
            ("subagent", 1)
        ]
    );
    let mut sum = pmt_store::UsageTotals::default();
    for row in &by_purpose.rows {
        sum.merge(&row.totals);
    }
    assert_eq!(sum.calls, by_purpose.total.calls);
    assert_eq!(sum.tokens, by_purpose.total.tokens);
    assert!((sum.cost_usd - by_purpose.total.cost_usd).abs() < 1e-12);
    assert_eq!(by_purpose.total, store.usage_sum(&since).unwrap());

    // Days are cut in the profile's zone: 23:30 UTC on May 3 is May 4 in
    // Lisbon (UTC+1 in May).
    let lisbon = TimeZone::get("Europe/Lisbon").unwrap();
    let by_day = report(&store, "default", &since, GroupBy::Day, &lisbon).unwrap();
    let days: Vec<(&str, u64)> = by_day
        .rows
        .iter()
        .map(|r| (r.key.as_str(), r.totals.calls))
        .collect();
    assert_eq!(
        days,
        [("2026-05-01", 1), ("2026-05-02", 2), ("2026-05-04", 2)]
    );
}
