//! Reading a board's history (spec: kanban, Event log and run history;
//! Dispatcher tick): `log`, `runs`, `tail`, `stats` and `diag`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use pmt_core::error::Error;
use pmt_kanban::model::Event;
use pmt_kanban::{Filter, Status};
use serde_json::json;

use super::Env;
use crate::output::{document, print, print_raw};

fn when(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).map_or_else(|_| ms.to_string(), |t| t.to_string())
}

fn event_line(e: &Event) -> String {
    let payload = if e.payload.as_object().is_some_and(serde_json::Map::is_empty) {
        String::new()
    } else {
        format!(" {}", e.payload)
    };
    let task = e.task_id.as_deref().unwrap_or("board");
    format!("{} {task} {} {}{payload}", when(e.at), e.actor, e.kind)
}

/// `kanban log`.
pub(super) fn log(env: &Env<'_>, id: &str) -> Result<u8, Error> {
    let board = env.board()?;
    board.task(id)?;
    let events = board.events(Some(id))?;
    if env.ctx.json {
        return document("kanban log", &json!({"task_id": id, "events": events}));
    }
    let mut out = String::new();
    for e in &events {
        let _ = writeln!(out, "{}", event_line(e));
    }
    print_raw(&out)
}

/// `kanban runs`.
pub(super) fn runs(env: &Env<'_>, id: &str) -> Result<u8, Error> {
    let board = env.board()?;
    board.task(id)?;
    let runs = board.runs(id)?;
    if env.ctx.json {
        return document("kanban runs", &json!({"task_id": id, "runs": runs}));
    }
    let mut out = String::new();
    for r in &runs {
        let _ = writeln!(
            out,
            "{} {} as {} ({}) started {} outcome {} exit {} spend {:.4} USD {} tokens",
            r.id,
            r.role,
            r.profile,
            r.pid.map_or_else(|| "-".to_owned(), |p| format!("pid {p}")),
            when(r.started_at),
            r.outcome.as_deref().unwrap_or("open"),
            r.exit_code
                .map_or_else(|| "-".to_owned(), |c| c.to_string()),
            r.cost_usd,
            r.tokens
        );
        if let Some(session) = &r.session_id {
            let _ = writeln!(
                out,
                "    session {session}: prometheus -p {} sessions show {session}",
                r.profile
            );
        }
        if let Some(error) = &r.error {
            let _ = writeln!(out, "    {error}");
        }
    }
    print_raw(&out)
}

/// `kanban tail`.
pub(super) fn tail(env: &Env<'_>, lines: usize, follow: bool) -> Result<u8, Error> {
    let board = env.board()?;
    let events = board.events(None)?;
    let start = events.len().saturating_sub(lines);
    let mut last = 0;
    for e in &events[start..] {
        print(&event_line(e))?;
    }
    if let Some(e) = events.last() {
        last = e.id;
    }
    if !follow {
        return Ok(0);
    }
    loop {
        std::thread::sleep(Duration::from_secs(1));
        for e in board.events_since(last, &[])? {
            print(&event_line(&e))?;
            last = e.id;
        }
    }
}

/// `kanban stats`.
pub(super) fn stats(env: &Env<'_>) -> Result<u8, Error> {
    let board = env.board()?;
    let tasks = board.tasks(&Filter {
        archived: true,
        ..Filter::default()
    })?;
    let mut by_status: BTreeMap<&str, usize> =
        Status::ALL.iter().map(|s| (s.as_str(), 0)).collect();
    let mut outcomes: BTreeMap<String, usize> = BTreeMap::new();
    let mut spend = 0.0;
    for t in &tasks {
        *by_status.entry(t.status.as_str()).or_default() += 1;
        for r in board.runs(&t.id)? {
            *outcomes
                .entry(r.outcome.unwrap_or_else(|| "open".to_owned()))
                .or_default() += 1;
            spend += r.cost_usd;
        }
    }
    if env.ctx.json {
        return document(
            "kanban stats",
            &json!({"board": board.slug(), "statuses": by_status, "run_outcomes": outcomes, "spend_usd": spend}),
        );
    }
    let mut out = format!("board {}: {} task(s)\n", board.slug(), tasks.len());
    for (status, n) in &by_status {
        let _ = writeln!(out, "  {status:<9} {n}");
    }
    for (outcome, n) in &outcomes {
        let _ = writeln!(out, "  runs {outcome}: {n}");
    }
    let _ = writeln!(out, "  spend {spend:.4} USD");
    print_raw(&out)
}

/// `kanban diag`.
pub(super) fn diag(env: &Env<'_>) -> Result<u8, Error> {
    let board = env.board()?;
    let lease = board.lease()?;
    let ticks: Vec<Event> = board
        .events(None)?
        .into_iter()
        .filter(|e| e.kind == "tick")
        .rev()
        .take(5)
        .collect();
    let open: Vec<_> = board
        .open_runs()?
        .into_iter()
        .map(|r| {
            let alive = r.pid.zip(r.pid_start).is_some_and(|(p, s)| pmt_kanban::process::alive(p, s));
            json!({"run": r.id, "task_id": r.task_id, "profile": r.profile, "pid": r.pid, "alive": alive, "lease_expires_at": r.lease_expires_at})
        })
        .collect();
    let paused = pmt_core::pause::active(&env.home).map(|p| p.reason);
    if env.ctx.json {
        let lease = lease.map(|(holder, expires)| json!({"holder": holder, "expires_at": expires}));
        return document(
            "kanban diag",
            &json!({"board": board.slug(), "lease": lease, "paused": paused, "ticks": ticks, "open_runs": open}),
        );
    }
    let mut out = format!("board {}\n", board.slug());
    let _ = match &lease {
        Some((holder, expires)) => {
            writeln!(out, "dispatcher lease: {holder} until {}", when(*expires))
        }
        None => writeln!(out, "dispatcher lease: free"),
    };
    if let Some(reason) = paused {
        let _ = writeln!(out, "paused: {reason}");
    }
    for t in &ticks {
        let _ = writeln!(out, "tick {}: {}", when(t.at), t.payload);
    }
    for r in &open {
        let _ = writeln!(out, "open run {r}");
    }
    print_raw(&out)
}
