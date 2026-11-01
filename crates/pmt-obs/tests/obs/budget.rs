//! The budget engine (spec: observability, Hard budgets, Budget warnings
//! and overrides; agent-turn-loop, per-turn and per-session token and cost
//! budgets, unpriced models fail closed under a cost budget).

use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan as _};
use pmt_config::schema::BudgetsConfig;
use pmt_core::error::ErrorKind;
use pmt_obs::budget::{Budgets, Place, Request, notice};
use pmt_obs::usage::Price;
use pmt_store::{LineageKind, Store};

use crate::support::{child, recorder, root, spend, store, temp_dir, use_tokens};

/// 1 USD per million tokens in and out: 10k in + 40k out = $0.05.
const PRICE: Option<Price> = Some(Price {
    input: 1.0,
    output: 1.0,
    cache_read: 1.0,
    cache_write: 1.0,
    reasoning: None,
});

fn request(place: &Place, price: Option<Price>) -> Request<'_> {
    Request {
        place,
        unattended: false,
        model: "priced",
        price,
        input_tokens: 10_000,
        max_output_tokens: 40_000,
    }
}

fn off() -> BudgetsConfig {
    BudgetsConfig {
        unattended_root_usd: 0.0,
        ..BudgetsConfig::default()
    }
}

fn budgets() -> Budgets {
    Budgets::new(TimeZone::UTC)
}

fn place(store: &Store, session: pmt_core::id::SessionId, turn: Option<Timestamp>) -> Place {
    Place::of(store, session, turn).unwrap()
}

#[test]
fn a_root_budget_stops_a_subagent_call_in_the_delegation_tree() {
    let dir = temp_dir();
    let store = store(dir.path());
    let rec = recorder(&store);
    let root_id = root(&store);
    let subagent = child(&store, root_id, LineageKind::Delegate);
    let now = Timestamp::now();
    spend(&rec, root_id, 0.90, now);
    spend(&rec, subagent, 0.08, now);
    let config = BudgetsConfig {
        root_usd: 1.00,
        ..off()
    };

    let at = place(&store, subagent, Some(now));
    let refused = budgets()
        .check(&store, &config, &request(&at, PRICE), now)
        .unwrap_err();
    assert_eq!(refused.error.kind(), ErrorKind::BudgetExceeded);
    assert_eq!(refused.error.exit_code(), 3, "a CLI one-shot exits 3");
    assert_eq!(refused.error.code().code, "budget.exceeded");
    assert_eq!(
        refused.error.message(),
        "root budget exceeded: spent $0.98 of $1.00; the next call could cost up to $0.05"
    );
    assert_eq!(refused.event.scope, "root");
    assert_eq!(refused.event.unit, "usd");
    assert!((refused.event.spent - 0.98).abs() < 1e-9);
    assert!((refused.event.estimate.unwrap() - 0.05).abs() < 1e-9);

    // A smaller call still fits.
    let small = Request {
        input_tokens: 1_000,
        max_output_tokens: 1_000,
        ..request(&at, PRICE)
    };
    assert!(budgets().check(&store, &config, &small, now).is_ok());
}

#[test]
fn an_unpriced_model_under_a_usd_budget_is_refused_unless_allowed() {
    let dir = temp_dir();
    let store = store(dir.path());
    let session = root(&store);
    let now = Timestamp::now();
    let at = place(&store, session, None);
    let config = BudgetsConfig {
        session_usd: 5.0,
        ..off()
    };
    let unpriced = Request {
        model: "llama-local",
        ..request(&at, None)
    };
    let refused = budgets()
        .check(&store, &config, &unpriced, now)
        .unwrap_err();
    assert_eq!(refused.error.kind(), ErrorKind::BudgetExceeded);
    assert_eq!(refused.error.code().code, "budget.unpriced_model");
    assert_eq!(
        refused.error.message(),
        "cost budget set but model 'llama-local' has no price; set models.llama-local.price or budgets.allow_unpriced = true"
    );
    assert_eq!(refused.event.scope, "session");

    // allow_unpriced: USD limits are skipped, token limits still apply.
    let allowed = BudgetsConfig {
        allow_unpriced: true,
        session_tokens: 60_000,
        ..config.clone()
    };
    assert!(budgets().check(&store, &allowed, &unpriced, now).is_ok());
    use_tokens(&recorder(&store), session, 20_000, 1_000, now);
    let refused = budgets()
        .check(&store, &allowed, &unpriced, now)
        .unwrap_err();
    assert_eq!(refused.error.code().code, "budget.exceeded");
    assert_eq!(refused.event.unit, "tokens");

    // No USD budget: an unpriced model is fine.
    assert!(budgets().check(&store, &off(), &unpriced, now).is_ok());
}

#[test]
fn the_turn_budget_counts_only_this_turns_calls_including_children() {
    let dir = temp_dir();
    let store = store(dir.path());
    let rec = recorder(&store);
    let session = root(&store);
    let started = Timestamp::now();
    spend(&rec, session, 3.00, started - 1.hour()); // an earlier turn
    spend(&rec, session, 0.40, started + 1.second());
    let delegate = child(&store, session, LineageKind::Delegate);
    spend(&rec, delegate, 0.08, started + 2.seconds());
    let config = BudgetsConfig {
        turn_usd: 0.50,
        ..off()
    };
    let now = started + 3.seconds();
    let at = place(&store, session, Some(started));
    let refused = budgets()
        .check(&store, &config, &request(&at, PRICE), now)
        .unwrap_err();
    assert_eq!(refused.event.scope, "turn");
    assert!((refused.event.spent - 0.48).abs() < 1e-9);

    // Token budget: a delegated child that used 90,000 tokens.
    let tokens = BudgetsConfig {
        turn_tokens: 100_000,
        ..off()
    };
    let fresh = root(&store);
    let kid = child(&store, fresh, LineageKind::Delegate);
    use_tokens(&rec, kid, 80_000, 10_000, started + 1.second());
    let at = place(&store, fresh, Some(started));
    let refused = budgets()
        .check(&store, &tokens, &request(&at, PRICE), now)
        .unwrap_err();
    assert_eq!(refused.event.scope, "turn");
    assert_eq!(refused.event.unit, "tokens");
    assert_eq!(
        refused.error.message(),
        "turn budget exceeded: spent 90000 tokens of 100000 tokens; the next call could cost up to 50000 tokens"
    );
    // Outside a turn the turn limits do not apply.
    let outside = place(&store, fresh, None);
    assert!(
        budgets()
            .check(&store, &tokens, &request(&outside, PRICE), now)
            .is_ok()
    );
}

#[test]
fn a_raised_budget_applies_to_the_next_call() {
    let dir = temp_dir();
    let store = store(dir.path());
    let session = root(&store);
    let now = Timestamp::now();
    spend(&recorder(&store), session, 0.98, now);
    let engine = budgets();
    let at = place(&store, session, None);
    let tight = BudgetsConfig {
        session_usd: 1.0,
        ..off()
    };
    assert!(
        engine
            .check(&store, &tight, &request(&at, PRICE), now)
            .is_err()
    );
    let raised = BudgetsConfig {
        session_usd: 2.0,
        ..off()
    };
    assert!(
        engine
            .check(&store, &raised, &request(&at, PRICE), now)
            .is_ok()
    );
}

#[test]
fn days_and_months_are_cut_in_the_profile_zone() {
    let dir = temp_dir();
    let store = store(dir.path());
    let rec = recorder(&store);
    let session = root(&store);
    let tokyo = TimeZone::get("Asia/Tokyo").unwrap();
    // 2026-05-02 09:30 in Tokyo = 00:30 UTC; 23:30 UTC on May 1 is May 2
    // 08:30 in Tokyo (today), 14:00 UTC on May 1 is still May 1 there.
    let now: Timestamp = "2026-05-02T00:30:00Z".parse().unwrap();
    spend(&rec, session, 0.70, "2026-05-01T23:30:00Z".parse().unwrap());
    spend(&rec, session, 5.00, "2026-05-01T14:00:00Z".parse().unwrap());
    spend(&rec, session, 9.00, "2026-04-30T14:00:00Z".parse().unwrap());
    let at = place(&store, session, None);
    let daily = BudgetsConfig {
        daily_usd: 0.74,
        ..off()
    };
    let refused = Budgets::new(tokyo.clone())
        .check(&store, &daily, &request(&at, PRICE), now)
        .unwrap_err();
    assert_eq!(refused.event.scope, "daily");
    assert!((refused.event.spent - 0.70).abs() < 1e-9);
    let monthly = BudgetsConfig {
        monthly_usd: 5.74,
        ..off()
    };
    let refused = Budgets::new(tokyo)
        .check(&store, &monthly, &request(&at, PRICE), now)
        .unwrap_err();
    assert_eq!(refused.event.scope, "monthly");
    assert!(
        (refused.event.spent - 5.70).abs() < 1e-9,
        "April is not counted"
    );
    // In UTC the 23:30 row is May 1 too: the day has spent 5.70.
    let utc = budgets().check(
        &store,
        &BudgetsConfig {
            daily_usd: 6.0,
            ..off()
        },
        &request(&at, PRICE),
        now,
    );
    assert!(utc.is_ok(), "May 2 UTC has spent nothing yet");
}

#[test]
fn the_unattended_budget_applies_only_to_unattended_runs() {
    let dir = temp_dir();
    let store = store(dir.path());
    let session = root(&store);
    let now = Timestamp::now();
    spend(&recorder(&store), session, 4.99, now);
    let config = BudgetsConfig::default(); // unattended_root_usd = 5.00
    let at = place(&store, session, None);
    assert!(
        budgets()
            .check(&store, &config, &request(&at, PRICE), now)
            .is_ok()
    );
    let cron = Request {
        unattended: true,
        ..request(&at, PRICE)
    };
    let refused = budgets().check(&store, &config, &cron, now).unwrap_err();
    assert_eq!(refused.event.scope, "unattended");
}

#[test]
fn a_warning_is_given_once_per_scope_and_period_at_the_ratio() {
    let dir = temp_dir();
    let store = store(dir.path());
    let rec = recorder(&store);
    let session = root(&store);
    let now = Timestamp::now();
    let config = BudgetsConfig {
        root_usd: 1.0,
        session_usd: 10.0,
        ..off()
    };
    let engine = budgets();
    let at = place(&store, session, None);
    spend(&rec, session, 0.70, now);
    assert!(
        engine
            .after_call(&store, &config, &at, false, now)
            .unwrap()
            .is_empty()
    );
    spend(&rec, session, 0.11, now);
    let warnings = engine.after_call(&store, &config, &at, false, now).unwrap();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].scope, "root");
    assert_eq!(
        notice(&warnings[0]),
        "budget warning: root spend $0.81 of $1.00 (81%)"
    );
    // Not again for the same root, neither after a call nor before one.
    spend(&rec, session, 0.01, now);
    assert!(
        engine
            .after_call(&store, &config, &at, false, now)
            .unwrap()
            .is_empty()
    );
    let small = Request {
        input_tokens: 10,
        max_output_tokens: 10,
        ..request(&at, PRICE)
    };
    assert!(
        engine
            .check(&store, &config, &small, now)
            .unwrap()
            .warnings
            .is_empty()
    );
    // Another root is another period.
    let other = root(&store);
    spend(&rec, other, 0.85, now);
    let at_other = place(&store, other, None);
    let allowed = engine
        .check(
            &store,
            &config,
            &Request {
                place: &at_other,
                ..small
            },
            now,
        )
        .unwrap();
    assert_eq!(allowed.warnings.len(), 1);
}

#[test]
fn the_model_cannot_raise_its_own_budget() {
    use pmt_config::{Home, ProfileName};
    use pmt_obs::budget::guard::{ConfigWriteGuard, check_key};

    let dir = temp_dir();
    let home = Home::open(dir.path()).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    let config = profile.config_file();
    config.set("budgets.root_usd", "1.0").unwrap();
    let path = config.path().to_owned();
    let current = std::fs::read_to_string(&path).unwrap();

    // A config tool setting the key.
    let err = check_key("budgets.root_usd").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);

    // A file tool rewriting config.toml with a raised budget.
    let guard = ConfigWriteGuard::new(home.root());
    let raised = current.replace("root_usd = 1.0", "root_usd = 1000.0");
    assert_ne!(raised, current);
    let err = guard.check(&path, &raised).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);
    assert_eq!(err.exit_code(), 77);
    // The same file with the budgets untouched may be written.
    assert!(guard.check(&path, &format!("{current}\n# note\n")).is_ok());
}
