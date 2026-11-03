//! The turn's side of the plugin hooks that watch or rewrite replies
//! (spec: plugin-system, Transform hooks rewrite results and replies;
//! Streaming and lifecycle observer hooks; `pre_verify` continuation
//! hook).
//!
//! [`Stream`] sits between the model and the surface's delta sink: it
//! tells the stream observers (`on_stream_start`, `on_stream_delta`,
//! `on_stream_end`) about each model call's text, through
//! [`TurnHooks::observe`], which never blocks the token path and discards
//! every answer, so an observer cannot change what streams; and when a
//! `transform_llm_output` callback holds the stream (decided once, at
//! turn start), it forwards nothing, so no reply text reaches the surface
//! before the hook has seen it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value, json};

use super::model::{DeltaSink, ModelReply, RouteInfo};
use super::run::Turn;
use super::{FinalReply, Transformed, TurnHooks, Verify, WITHHELD, interactive_platform};

/// The delta sink of one turn.
pub(super) struct Stream {
    inner: Arc<dyn DeltaSink>,
    hooks: Arc<dyn TurnHooks>,
    held: bool,
    session: String,
    started: AtomicBool,
    text: Mutex<String>,
}

impl Stream {
    pub(super) fn new(
        inner: Arc<dyn DeltaSink>,
        hooks: Arc<dyn TurnHooks>,
        held: bool,
        session: String,
    ) -> Self {
        Self {
            inner,
            hooks,
            held,
            session,
            started: AtomicBool::new(false),
            text: Mutex::new(String::new()),
        }
    }

    /// One model call finished: `on_stream_end` with its whole text, when
    /// it streamed any.
    pub(super) fn end(&self) {
        if !self.started.swap(false, Ordering::SeqCst) {
            return;
        }
        let text = std::mem::take(&mut *self.text.lock().unwrap_or_else(PoisonError::into_inner));
        self.hooks.observe(
            "on_stream_end",
            fields(&self.session, [("text", json!(text))]),
        );
    }
}

impl DeltaSink for Stream {
    fn text(&self, text: &str) {
        if !self.held {
            self.inner.text(text);
        }
        if !self.started.swap(true, Ordering::SeqCst) {
            self.hooks
                .observe("on_stream_start", fields(&self.session, []));
        }
        self.text
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_str(text);
        self.hooks.observe(
            "on_stream_delta",
            fields(&self.session, [("delta", json!(text))]),
        );
    }
}

/// A payload of `session_id` plus `extra`.
pub(super) fn fields<const N: usize>(
    session: &str,
    extra: [(&str, Value); N],
) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("session_id".to_owned(), json!(session));
    for (key, value) in extra {
        map.insert(key.to_owned(), value);
    }
    map
}

/// The paths a successful `write_file` or `patch` call edited.
pub(super) fn edited_paths(tool: &str, arguments: &str) -> Vec<String> {
    if !matches!(tool, "write_file" | "patch") {
        return Vec::new();
    }
    let Ok(args) = serde_json::from_str::<Value>(arguments) else {
        return Vec::new();
    };
    let mut paths: Vec<String> = args
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .into_iter()
        .collect();
    if let Some(patch) = args.get("patch").and_then(Value::as_str) {
        paths.extend(patch.lines().filter_map(|line| {
            ["*** Update File: ", "*** Add File: ", "*** Delete File: "]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
                .map(|p| p.trim().to_owned())
        }));
    }
    paths
}

impl Turn<'_> {
    /// Starts the turn's stream: freezes whether the reply is held and
    /// wraps the surface's delta sink.
    pub(super) fn start_stream(&mut self) {
        let Some(hooks) = self.agent.setup.turn_hooks.clone() else {
            return;
        };
        self.held = hooks.holds_stream();
        let stream = Arc::new(Stream::new(
            Arc::clone(&self.options.deltas),
            hooks,
            self.held,
            self.agent.setup.session.to_string(),
        ));
        self.options.deltas = Arc::clone(&stream) as Arc<dyn DeltaSink>;
        self.stream = Some(stream);
    }

    /// Fires an observer hook with `session_id` and `extra`.
    pub(super) fn observe_hook<const N: usize>(
        &self,
        hook: &'static str,
        extra: [(&str, Value); N],
    ) {
        if let Some(hooks) = &self.agent.setup.turn_hooks {
            hooks.observe(hook, fields(&self.agent.setup.session.to_string(), extra));
        }
    }

    /// The final reply after `transform_llm_output`; a held turn delivers
    /// it to the surface now, in one piece.
    pub(super) async fn transform_final(&self, text: String, route: &RouteInfo) -> String {
        let Some(hooks) = self.agent.setup.turn_hooks.clone() else {
            return text;
        };
        let session = self.agent.setup.session.to_string();
        let reply = FinalReply {
            session_id: &session,
            text: &text,
            model: &route.model,
            platform: &self.agent.setup.prompt.platform,
        };
        let delivered = match hooks.transform_llm_output(reply, self.held).await {
            Transformed::Unchanged => text,
            Transformed::Replaced(replaced) => replaced,
            Transformed::Withheld => WITHHELD.to_owned(),
        };
        if let (true, Some(stream)) = (self.held, &self.stream) {
            stream.inner.text(&delivered);
        }
        delivered
    }

    /// `pre_verify` before a toolless reply ends a turn that edited files:
    /// the continuation message, while under `agent.max_verify_nudges`.
    pub(super) async fn verify_nudge(
        &mut self,
        reply: &ModelReply,
        route: &RouteInfo,
    ) -> Option<String> {
        let hooks = self.agent.setup.turn_hooks.clone()?;
        if self.edited.is_empty() || self.verify_nudges >= self.config.agent.max_verify_nudges {
            return None;
        }
        let session = self.agent.setup.session.to_string();
        let platform = &self.agent.setup.prompt.platform;
        let verify = Verify {
            session_id: &session,
            platform,
            model: &route.model,
            coding: interactive_platform(platform),
            attempt: self.verify_nudges,
            final_response: &reply.text,
            changed_paths: self.edited.iter().cloned().collect(),
        };
        let message = hooks.pre_verify(verify).await?;
        self.verify_nudges += 1;
        Some(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_name_their_paths_and_reads_do_not() {
        assert_eq!(edited_paths("write_file", r#"{"path":"a.rs"}"#), ["a.rs"]);
        assert_eq!(
            edited_paths(
                "patch",
                r#"{"mode":"v4a","patch":"*** Begin Patch\n*** Update File: b.rs\n*** Add File: c.rs\n"}"#
            ),
            ["b.rs", "c.rs"]
        );
        assert!(edited_paths("read_file", r#"{"path":"a.rs"}"#).is_empty());
    }
}
