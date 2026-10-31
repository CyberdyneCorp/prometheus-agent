//! Tool progress, the focus view and display preferences of the line
//! interface (spec: cli, Tool progress display and focus view; Display
//! preferences). Everything here is display only.

use std::fmt::Write as _;

use pmt_agent::turn::TurnUsage;
use pmt_config::schema::{ToolProgress, ToolProgressGrouping};
use pmt_core::error::Error;

use super::Repl;
use crate::chat::session::progress::Progress;
use crate::chat::theme::Role;

/// What one turn has shown of its tool activity.
#[derive(Debug, Default)]
pub(super) struct TurnView {
    /// The tool of the previous call.
    last: Option<String>,
    /// Consecutive calls of one tool not printed yet (`accumulate`).
    group: Option<(String, u32)>,
    /// Calls the focus view hid.
    hidden: u32,
    /// Tool calls this turn.
    pub(super) calls: u32,
    /// The newest assistant text (shown as interim when a call follows).
    assistant: Option<String>,
}

/// The name of a progress mode.
#[must_use]
pub(super) fn mode_name(mode: ToolProgress) -> &'static str {
    match mode {
        ToolProgress::Off => "off",
        ToolProgress::New => "new",
        ToolProgress::All => "all",
        ToolProgress::Verbose => "verbose",
    }
}

/// `text` cut to `limit` characters (0: no limit).
#[must_use]
pub(super) fn preview(text: &str, limit: u32) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    if limit == 0 || one_line.chars().count() <= limit {
        return one_line;
    }
    let mut cut: String = one_line.chars().take(limit).collect();
    cut.push('…');
    cut
}

impl Repl {
    /// Renders one piece of tool activity of the running turn.
    pub(super) fn on_progress(&mut self, progress: Progress) {
        match progress {
            Progress::Call { tool, arguments } => self.on_call(tool, &arguments.to_string()),
            Progress::Result { tool, ok, text } => {
                if self.display.focus.is_none() && self.display.progress == ToolProgress::Verbose {
                    let limit = self.factory.scope.config().display.tool_preview_length;
                    let status = if ok { "ok" } else { "failed" };
                    let line = format!("    ↳ {tool} {status}: {}", preview(&text, limit));
                    self.say(Role::Tool, &line);
                }
            }
            Progress::Assistant(text) => self.view.assistant = Some(text),
            Progress::Reasoning(text) => {
                if self.factory.scope.config().display.show_reasoning {
                    self.say(Role::Notice, &format!("(reasoning) {}", text.trim()));
                }
            }
        }
    }

    fn on_call(&mut self, tool: String, arguments: &str) {
        self.view.calls += 1;
        let interim = self.view.assistant.take().filter(|t| !t.trim().is_empty());
        if self.display.focus.is_some() {
            self.view.hidden += 1;
            return;
        }
        if let Some(text) = interim
            && self.factory.scope.config().display.interim_messages
            && self.display.progress != ToolProgress::Off
        {
            self.say(Role::Assistant, text.trim());
        }
        let display = &self.factory.scope.config().display;
        let changed = self.view.last.as_deref() != Some(tool.as_str());
        self.view.last = Some(tool.clone());
        match self.display.progress {
            ToolProgress::New if changed => self.say(Role::Tool, &format!("  ⚙ {tool}")),
            ToolProgress::Off | ToolProgress::New => {}
            ToolProgress::All
                if display.tool_progress_grouping == ToolProgressGrouping::Separate =>
            {
                self.say(Role::Tool, &format!("  ⚙ {tool}"));
            }
            ToolProgress::All => match &mut self.view.group {
                Some((name, count)) if *name == tool => *count += 1,
                _ => {
                    self.flush_group();
                    self.view.group = Some((tool, 1));
                }
            },
            ToolProgress::Verbose => {
                let line = format!(
                    "  ⚙ {tool} {}",
                    preview(arguments, display.tool_preview_length)
                );
                self.say(Role::Tool, &line);
            }
        }
    }

    /// Prints the pending `accumulate` group.
    fn flush_group(&mut self) {
        if let Some((tool, count)) = self.view.group.take() {
            let line = if count > 1 {
                format!("  ⚙ {tool} ×{count}")
            } else {
                format!("  ⚙ {tool}")
            };
            self.say(Role::Tool, &line);
        }
    }

    /// Ends the turn's tool display (the last group, the focus line);
    /// returns the turn's tool call count.
    pub(super) fn end_progress(&mut self) -> u32 {
        let mut pending = Vec::new();
        if let Some(progress) = self.progress.as_mut() {
            while let Ok(event) = progress.try_recv() {
                pending.push(event);
            }
        }
        for event in pending {
            self.on_progress(event);
        }
        self.flush_group();
        let view = std::mem::take(&mut self.view);
        if self.display.focus.is_some() && view.hidden > 0 {
            self.say(Role::Notice, &format!("{} tool lines hidden", view.hidden));
        }
        view.calls
    }

    /// The turn summary line, when `display.turn_summary` is on.
    pub(super) fn turn_summary(&self, elapsed: std::time::Duration, calls: u32, usage: &TurnUsage) {
        let display = &self.factory.scope.config().display;
        if display.turn_summary && !self.display.quiet {
            self.say(
                Role::Notice,
                &summary(elapsed, calls, usage, display.show_cost),
            );
        }
    }

    /// `/focus [on|off|status]`.
    pub(super) fn focus(&mut self, args: &str) -> String {
        match args.trim() {
            "on" => {
                if self.display.focus.is_none() {
                    self.display.focus = Some(self.display.progress);
                    self.display.progress = ToolProgress::Off;
                }
                "focus on: tool progress hidden".to_owned()
            }
            "off" => match self.display.focus.take() {
                Some(previous) => {
                    self.display.progress = previous;
                    format!("focus off: tool progress {}", mode_name(previous))
                }
                None => "focus is off".to_owned(),
            },
            _ => match self.display.focus {
                Some(_) => "focus is on".to_owned(),
                None => format!(
                    "focus is off (tool progress {})",
                    mode_name(self.display.progress)
                ),
            },
        }
    }

    /// `/verbose`: the next progress mode (the one focus restores while
    /// focus is on).
    pub(super) fn verbose(&mut self) -> String {
        let slot = match &mut self.display.focus {
            Some(saved) => saved,
            None => &mut self.display.progress,
        };
        *slot = match *slot {
            ToolProgress::Off => ToolProgress::New,
            ToolProgress::New => ToolProgress::All,
            ToolProgress::All => ToolProgress::Verbose,
            ToolProgress::Verbose => ToolProgress::Off,
        };
        format!("tool progress: {}", mode_name(*slot))
    }

    /// `/timestamps [on|off|status]` (alias `/ts`), saved in
    /// `display.timestamps`.
    pub(super) fn timestamps(&mut self, args: &str) -> Result<String, Error> {
        let on = match args.trim() {
            "on" => true,
            "off" => false,
            _ => {
                let state = if self.display.timestamps { "on" } else { "off" };
                return Ok(format!("timestamps are {state}"));
            }
        };
        self.factory
            .scope
            .profile()
            .config_file()
            .set("display.timestamps", if on { "true" } else { "false" })?;
        self.display.timestamps = on;
        Ok(format!("timestamps {}", if on { "on" } else { "off" }))
    }

    /// `text` with the time prefix when timestamps are on.
    pub(super) fn stamped(&self, text: &str) -> String {
        if !self.display.timestamps {
            return text.to_owned();
        }
        let format = &self.factory.scope.config().display.timestamp_format;
        let now = jiff::Zoned::now();
        format!("[{}] {text}", now.strftime(format.as_str()))
    }
}

/// The one-line turn summary: duration, tool calls, tokens (and cost).
#[must_use]
pub(super) fn summary(
    elapsed: std::time::Duration,
    calls: u32,
    usage: &TurnUsage,
    cost: bool,
) -> String {
    let tools = if calls == 1 { "tool" } else { "tools" };
    let mut line = format!(
        "· {:.1}s · {calls} {tools} · {} in / {} out tokens",
        elapsed.as_secs_f64(),
        usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens,
        usage.output_tokens
    );
    if cost {
        let _ = match usage.cost_usd {
            Some(usd) => write!(line, " · ${usd:.4}"),
            None => write!(line, " · cost unknown"),
        };
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_are_cut_to_the_configured_length() {
        assert_eq!(preview("a  b\nc", 0), "a b c");
        assert_eq!(preview("abcdef", 3), "abc…");
        assert_eq!(preview("abc", 3), "abc");
    }

    #[test]
    fn the_summary_names_duration_tools_and_tokens() {
        let usage = TurnUsage {
            input_tokens: 10,
            output_tokens: 5,
            cost_usd: Some(0.5),
            ..TurnUsage::default()
        };
        let line = summary(std::time::Duration::from_millis(1500), 2, &usage, true);
        assert_eq!(line, "· 1.5s · 2 tools · 10 in / 5 out tokens · $0.5000");
    }
}
