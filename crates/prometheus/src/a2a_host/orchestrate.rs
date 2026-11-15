//! `a2a_orchestrate` (spec: agent-interop, Outbound A2A tools): one
//! message to every configured agent advertising a capability, at once;
//! `first` keeps the first success, `best` asks the auxiliary judge which
//! reply answers best and attaches every reply.

use std::fmt::Write as _;

use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use serde_json::{Value, json};

use super::tool::Port;
use crate::kanban_host::goal::GoalJudge;

/// Asks every agent of `names`; with `first`, stops at the first success.
pub(super) async fn fan_out(port: &Port, names: &[String], message: &str, first: bool) -> Value {
    let mut pending: FuturesUnordered<_> = names
        .iter()
        .map(|name| async move { (name.clone(), port.ask(name, message).await) })
        .collect();
    let mut replies = Vec::new();
    while let Some((agent, answer)) = pending.next().await {
        match answer {
            Ok(reply) if first => return json!({"mode": "first", "agent": agent, "reply": reply}),
            Ok(reply) => replies.push(json!({"agent": agent, "reply": reply})),
            Err(error) => replies.push(json!({"agent": agent, "error": error})),
        }
    }
    replies.sort_by(|a, b| a["agent"].as_str().cmp(&b["agent"].as_str()));
    if first {
        return json!({"mode": "first", "error": "no agent answered", "replies": replies});
    }
    json!({"mode": "all", "replies": replies})
}

/// The `best` answer: the judge's pick among the successful replies of
/// `all` (the first success when the judge cannot decide), with every
/// reply attached.
pub(super) async fn rank(judge: Option<&dyn GoalJudge>, message: &str, all: Value) -> Value {
    let replies = all["replies"].as_array().cloned().unwrap_or_default();
    let answered: Vec<&Value> = replies.iter().filter(|r| r["reply"].is_string()).collect();
    if answered.is_empty() {
        return json!({"mode": "best", "error": "no agent answered", "replies": replies});
    }
    let mut prompt = format!(
        "Several agents answered the same request. Reply with only the number of the reply that answers it best.\n\nRequest:\n{message}\n"
    );
    for (i, reply) in answered.iter().enumerate() {
        let _ = write!(
            prompt,
            "\nReply {}:\n{}\n",
            i + 1,
            reply["reply"].as_str().unwrap_or_default()
        );
    }
    let (pick, note) = match judge {
        Some(judge) => match judge.ask(prompt).await {
            Ok(verdict) => (number(&verdict, answered.len()), None),
            Err(error) => (None, Some(format!("the judge failed: {error}"))),
        },
        None => (
            None,
            Some("no judge route; the first reply is shown".to_owned()),
        ),
    };
    let best = answered[pick.unwrap_or(1) - 1];
    let mut out = json!({"mode": "best", "best": best, "replies": replies});
    if let Some(note) = note {
        out["note"] = note.into();
    }
    out
}

/// The first number in `text` naming one of `count` replies.
fn number(text: &str, count: usize) -> Option<usize> {
    text.split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .filter(|n| (1..=count).contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Judge(&'static str);

    impl GoalJudge for Judge {
        fn ask(
            &self,
            _prompt: String,
        ) -> pmt_agent::turn::model::BoxFuture<'_, Result<String, String>> {
            let verdict = self.0.to_owned();
            Box::pin(async move { Ok(verdict) })
        }
    }

    #[tokio::test]
    async fn the_judge_picks_the_best_reply() {
        let all = json!({"replies": [
            {"agent": "a", "reply": "short"},
            {"agent": "b", "error": "down"},
            {"agent": "c", "reply": "thorough"},
        ]});
        let ranked = rank(Some(&Judge("Reply 2 is best.")), "q", all.clone()).await;
        assert_eq!(ranked["best"]["agent"], "c");
        assert_eq!(ranked["replies"].as_array().unwrap().len(), 3);
        let unsure = rank(Some(&Judge("none")), "q", all).await;
        assert_eq!(unsure["best"]["agent"], "a");
        assert_eq!(number("7", 3), None);
    }
}
