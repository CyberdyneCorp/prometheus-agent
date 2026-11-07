//! `prometheus memory log|diff|revert|search` (spec: memory, Memory revision
//! journal; Semantic recall).

use std::fmt::Write as _;
use std::path::Path;

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_memory::journal::{self, Since};
use pmt_store::{MemoryOrigin, RevisionFilter};
use serde_json::json;

use super::Env;
use super::args::OriginArg;
use crate::output::{json, print_raw};

const fn origin(arg: OriginArg) -> MemoryOrigin {
    match arg {
        OriginArg::User => MemoryOrigin::User,
        OriginArg::Agent => MemoryOrigin::Agent,
        OriginArg::BackgroundReview => MemoryOrigin::BackgroundReview,
        OriginArg::Provider => MemoryOrigin::Provider,
        OriginArg::Consolidation => MemoryOrigin::Consolidation,
        OriginArg::Import => MemoryOrigin::Import,
    }
}

fn since_filter(since: Option<&str>) -> Result<RevisionFilter, Error> {
    since.map_or_else(
        || Ok(RevisionFilter::default()),
        |s| Ok(Since::parse(s, Timestamp::now())?.filter()),
    )
}

/// `memory log`.
pub(super) fn log(env: &Env, since: Option<&str>, o: Option<OriginArg>) -> Result<u8, Error> {
    let filter = RevisionFilter {
        origin: o.map(origin),
        ..since_filter(since)?
    };
    let revisions = env.store.memory_revisions(&filter)?;
    if env.json {
        let rows: Vec<_> = revisions
            .iter()
            .map(|r| {
                json!({
                    "rev": r.rev,
                    "entry_id": r.entry_id,
                    "target": r.target,
                    "operation": r.operation.as_str(),
                    "before": r.before.as_ref().map(|e| &e.content),
                    "after": r.after.as_ref().map(|e| &e.content),
                    "origin": r.origin,
                    "session_id": r.session_id,
                    "at": r.at.to_string(),
                })
            })
            .collect();
        return json(&rows);
    }
    if revisions.is_empty() {
        eprintln!("no memory revisions");
        return Ok(0);
    }
    let mut text = String::new();
    for r in &revisions {
        let _ = writeln!(text, "{}", journal::log_line(r));
    }
    print_raw(&text)
}

/// `memory diff`.
pub(super) fn diff(env: &Env, since: Option<&str>, session: Option<&str>) -> Result<u8, Error> {
    let filter = RevisionFilter {
        session: session.map(str::to_owned),
        ..since_filter(since)?
    };
    let text = journal::diff(&env.store.memory_revisions(&filter)?);
    if env.json {
        return json(&json!({"diff": text}));
    }
    if text.is_empty() {
        eprintln!("no memory changes");
        return Ok(0);
    }
    print_raw(&text)
}

/// `memory revert`.
pub(super) fn revert(env: &Env, rev: i64) -> Result<u8, Error> {
    let new = journal::revert(&env.store, rev)?;
    if env.json {
        return json(&json!({"reverted": rev, "rev": new}));
    }
    eprintln!("reverted revision {rev} as revision {new} (from the next session)");
    Ok(0)
}

/// `memory search`: the archival ranking recall would use.
pub(super) fn search(
    env: &Env,
    query: &str,
    explain: bool,
    user_home: Option<&Path>,
) -> Result<u8, Error> {
    let embedder = if env.config().recall.embedding_model.is_some() {
        let models = crate::chat::session::model::Models::open(
            &env.scope,
            &env.store,
            user_home,
            &crate::chat::session::model::RouteFlags::default(),
            None,
        )?;
        models.embedder(None)
    } else {
        None
    };
    let redactor = env.scope.secrets().redactor()?;
    let ranked = crate::model_cmd::block_on(pmt_memory::rank::rank(
        &env.store,
        env.config(),
        embedder.as_deref(),
        &redactor,
        query,
    ))??;
    if env.json {
        return json(&ranked);
    }
    if ranked.is_empty() {
        eprintln!("no archival entry matches");
        return Ok(0);
    }
    let mut text = String::new();
    for s in &ranked {
        let _ = write!(text, "{} {:.3}", s.id, s.score);
        if explain {
            let _ = write!(text, " (bm25 {:.3}", s.bm25);
            if let Some(cos) = s.cosine {
                let _ = write!(text, ", cosine {cos:.3}");
            }
            text.push(')');
        }
        let _ = writeln!(
            text,
            "  {}",
            s.entry.content.lines().next().unwrap_or_default()
        );
    }
    print_raw(&text)
}
