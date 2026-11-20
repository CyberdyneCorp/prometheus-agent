//! The TUI's live session views in the daemon (spec: tui, Split panes for
//! subagents and tool output; Session timeline scrubber): a delegated
//! child's transcript feed, a session's turns, and the branch session a
//! rewind makes, with the workspace checkpoint that holds the files as
//! they were after the kept turn.

use pmt_core::error::{Error, codes};
use pmt_core::id::{MessageId, SessionId};
use pmt_delegation::live::{ChildItem, Feeds};
use pmt_gateway::live::{LiveHost, SubagentFeed};
use pmt_gateway::{HostFuture, Link};
use pmt_rpc::contracts::{
    RewindParams, SessionRewound, SubagentEventKind, SubagentEventPayload, Timeline, TimelineTurn,
};
use pmt_store::{Message, Role, Store, Visibility};
use tokio::sync::mpsc;

use super::{DaemonHost, session_id};

impl LiveHost for DaemonHost {
    fn subagent_feed<'a>(
        &'a self,
        profile: &'a str,
        session_id: &'a str,
        subagent_id: &'a str,
    ) -> HostFuture<'a, SubagentFeed> {
        Box::pin(async move {
            let served = self.profile(profile)?;
            let children = self.subagents_of(&served, session_id).await?;
            let Some(child) = children
                .subagents
                .iter()
                .find(|c| c.subagent_id == subagent_id)
            else {
                return Err(Error::new(
                    &codes::DELEGATION_NOT_IN_TREE,
                    format!("subagent {subagent_id} is not a child of session {session_id}"),
                ));
            };
            let item = |item: ChildItem| payload(session_id, subagent_id, item);
            let Some(subscription) = Feeds::process().subscribe(subagent_id) else {
                return Ok(SubagentFeed {
                    status: child.status.clone(),
                    backlog: Vec::new(),
                    live: None,
                });
            };
            let backlog = subscription.backlog.into_iter().map(item).collect();
            let live = subscription.live.map(|mut items| {
                let (tx, rx) = mpsc::unbounded_channel();
                let (session, child) = (session_id.to_owned(), subagent_id.to_owned());
                pmt_core::spawn::spawn(async move {
                    while let Some(next) = items.recv().await {
                        if tx.send(payload(&session, &child, next)).is_err() {
                            return;
                        }
                    }
                });
                rx
            });
            Ok(SubagentFeed {
                status: subscription.status.unwrap_or_else(|| "running".to_owned()),
                backlog,
                live,
            })
        })
    }

    fn timeline<'a>(&'a self, profile: &'a str, session: &'a str) -> HostFuture<'a, Timeline> {
        Box::pin(async move {
            let served = self.profile(profile)?;
            let id = session_id(session)?;
            timeline(&served.store, id)
        })
    }

    fn rewind<'a>(
        &'a self,
        profile: &'a str,
        params: RewindParams,
        link: Link,
    ) -> HostFuture<'a, SessionRewound> {
        Box::pin(async move {
            let served = self.profile(profile)?;
            let id = session_id(&params.session_id)?;
            let messages = served.store.messages(id, Visibility::Active)?;
            let starts = turn_starts(&messages);
            let to = usize::try_from(params.to_turn).unwrap_or(usize::MAX);
            if to == 0 || to > starts.len() {
                return Err(Error::new(
                    &codes::STORE_REWIND_TARGET_INVALID,
                    format!(
                        "session {id} has {} turns; there is no turn {}",
                        starts.len(),
                        params.to_turn
                    ),
                ));
            }
            // The branch keeps everything before the next turn's input.
            let last = starts
                .get(to)
                .map_or(messages.len(), |next| *next)
                .saturating_sub(1);
            let branch = served
                .store
                .fork_session(id, Some(messages[last].id), None)?;
            let session = self.load(&served, id, &link).await?;
            let checkpoint = served
                .store
                .messages(id, Visibility::All)
                .ok()
                .and_then(|all| {
                    let kept = users_through(&all, messages[starts[to - 1]].id);
                    later_checkpoint(&session.live, &params.session_id, kept)
                });
            Ok(SessionRewound {
                session_id: branch.id.to_string(),
                parent_session_id: params.session_id,
                turns: params.to_turn,
                checkpoint,
            })
        })
    }
}

/// One item of a child's transcript as `subagent.event` carries it.
fn payload(session_id: &str, subagent_id: &str, item: ChildItem) -> SubagentEventPayload {
    let (kind, text, tool, status) = match item {
        ChildItem::Assistant(text) => (SubagentEventKind::Assistant, Some(text), None, None),
        ChildItem::ToolCall { tool, hint } => {
            (SubagentEventKind::ToolCall, Some(hint), Some(tool), None)
        }
        ChildItem::ToolResult { tool, output } => (
            SubagentEventKind::ToolResult,
            Some(output),
            Some(tool),
            None,
        ),
        ChildItem::Finished(status) => (SubagentEventKind::Finished, None, None, Some(status)),
    };
    SubagentEventPayload {
        session_id: session_id.to_owned(),
        subagent_id: subagent_id.to_owned(),
        kind,
        text,
        tool,
        status,
    }
}

/// Whether `message` starts a turn: a user input, not a notice row the
/// host appended (those carry a `kind`).
fn starts_turn(message: &Message) -> bool {
    message.role == Role::User
        && message
            .metadata
            .as_ref()
            .is_none_or(|m| m.get("kind").is_none())
}

/// The indexes of the turn-starting messages.
fn turn_starts(messages: &[Message]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, m)| starts_turn(m))
        .map(|(n, _)| n)
        .collect()
}

/// The session's turns: when each started, its input's first line, its
/// tool calls and what its model calls cost (the ledger rows written
/// between its start and the next turn's).
fn timeline(store: &Store, id: SessionId) -> Result<Timeline, Error> {
    let messages = store.messages(id, Visibility::Active)?;
    let usage = store.usage_events(id)?;
    let starts = turn_starts(&messages);
    let mut turns = Vec::with_capacity(starts.len());
    for (n, start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(messages.len());
        let rows = &messages[*start..end];
        let from = messages[*start].created_at;
        let until = messages.get(end).map(|m| m.created_at);
        let costs: Vec<Option<f64>> = usage
            .iter()
            .filter(|u| u.ts >= from && until.is_none_or(|until| u.ts < until))
            .map(|u| u.cost_usd)
            .collect();
        let cost_usd = (!costs.is_empty() && costs.iter().all(Option::is_some))
            .then(|| costs.iter().flatten().sum());
        let tools = rows.iter().filter(|m| m.role == Role::Tool).count();
        turns.push(TimelineTurn {
            turn: u32::try_from(n + 1).unwrap_or(u32::MAX),
            message_id: rows[0].id.to_string(),
            at: from.to_string(),
            first_line: rows[0]
                .content
                .as_deref()
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned(),
            tools: u32::try_from(tools).unwrap_or(u32::MAX),
            cost_usd,
        });
    }
    Ok(Timeline { turns })
}

/// How many user rows the session had once `message` was written: the
/// turn number a checkpoint taken during that turn records.
fn users_through(all: &[Message], message: MessageId) -> u64 {
    let Some(seq) = all.iter().find(|m| m.id == message).map(|m| m.seq) else {
        return 0;
    };
    let users = all
        .iter()
        .filter(|m| m.role == Role::User && m.seq <= seq)
        .count();
    u64::try_from(users).unwrap_or(u64::MAX)
}

/// The workspace checkpoint (`files.rollback` number, 1 the newest) that
/// holds the files as they were once turn `kept` had ended: the first
/// one `session` took during a later turn, before that turn's change.
fn later_checkpoint(live: &crate::chat::session::Live, session: &str, kept: u64) -> Option<u32> {
    let snapshots = live.place.snapshots()?;
    let checkpoints = pmt_tools::snapshots::restore::checkpoints(
        snapshots.store(),
        snapshots.tree(),
        snapshots.workspace(),
    );
    // Newest first: the last match is the earliest.
    let index = checkpoints
        .iter()
        .rposition(|cp| cp.session == session && cp.turn > kept)?;
    u32::try_from(index + 1).ok()
}
