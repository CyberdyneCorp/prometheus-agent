//! `prometheus kanban dispatch --once|--loop` (spec: kanban, Single
//! dispatcher per home): one tick, or a standalone loop, as the home's
//! only dispatcher; another live holder of the lease (the daemon) makes
//! it exit 75 without claiming anything.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use pmt_core::error::Error;
use pmt_kanban::dispatch::Summary;
use pmt_kanban::lease::RENEW_MS;
use serde_json::json;

use super::Env;
use crate::output::{document, print_raw};

fn render(summaries: &[Summary]) -> String {
    let mut out = String::new();
    for s in summaries {
        let _ = writeln!(
            out,
            "{}: claimed {}, skipped_unassigned {}, skipped_allowlist {}, capped {}, reclaimed {}, blocked {}",
            s.board,
            s.claimed.len(),
            s.skipped_unassigned.len(),
            s.skipped_allowlist.len(),
            s.capped.len(),
            s.reclaimed.len(),
            s.blocked.len()
        );
    }
    out
}

/// One tick, or ticks every `kanban.dispatch_interval_secs` until
/// interrupted, renewing the lease every 20 s in between.
pub(super) fn dispatch(env: &Env<'_>, looping: bool) -> Result<u8, Error> {
    let renew_every = Duration::from_millis(u64::try_from(RENEW_MS).unwrap_or(20_000));
    let mut last_tick: Option<Instant> = None;
    let mut last_renew = Instant::now();
    loop {
        let config = env.scope.config().kanban.clone();
        let interval = Duration::from_secs(config.dispatch_interval_secs.max(1));
        if last_tick.is_none_or(|t| t.elapsed() >= interval) {
            pmt_core::pause::check(&env.home)?;
            let summaries = crate::kanban_host::tick(&env.home, "cli", &config, env.ctx.launch)?;
            if env.ctx.json {
                document("kanban dispatch", &json!({"boards": summaries}))?;
            } else {
                print_raw(&render(&summaries))?;
            }
            if !looping {
                return Ok(0);
            }
            last_tick = Some(Instant::now());
            last_renew = Instant::now();
        } else if last_renew.elapsed() >= renew_every {
            crate::kanban_host::renew(&env.home, "cli")?;
            last_renew = Instant::now();
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
