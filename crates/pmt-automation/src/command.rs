//! The automation command grammar every surface shares (spec:
//! session-automation, Goal command surface; Subgoals; Loop command and
//! cadence; Session heartbeat). Parsing is pure: the host applies the
//! parsed command to the session's stored state.

use crate::duration;

/// A parsed `/goal` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalCommand {
    /// `/goal <text>` or `/goal -- <text>`: set or replace, start a turn.
    Set(String),
    /// `/goal draft <text>`: expand into a contract first.
    Draft(String),
    /// `/goal show`: the goal and its contract.
    Show,
    /// `/goal` or `/goal status`.
    Status,
    /// `/goal pause`.
    Pause,
    /// `/goal resume`.
    Resume,
    /// `/goal clear` (`stop`, `done`).
    Clear,
    /// `/goal wait <pid> [reason]`.
    Wait {
        /// The process.
        pid: u32,
        /// Why.
        reason: String,
    },
    /// `/goal unwait`.
    Unwait,
    /// `/goal gate add <command>`.
    GateAdd(String),
    /// `/goal gate list` (or `/goal gate`).
    GateList,
    /// `/goal gate remove <N>` (1-based).
    GateRemove(usize),
    /// `/goal gate clear`.
    GateClear,
}

impl GoalCommand {
    /// Whether it only reads or changes control-plane state, so it runs
    /// while a turn does (spec: Goal changes while a turn runs).
    #[must_use]
    pub fn runs_while_busy(&self) -> bool {
        !matches!(self, Self::Set(_) | Self::Draft(_))
    }
}

/// What a running turn answers a new goal.
pub const BUSY_SET: &str = "A turn is running; use /stop first, then set the new goal.";

/// The `/goal` usage line.
pub const GOAL_USAGE: &str = "usage: /goal <text> | -- <text> | draft <text> | show | status | pause | resume | clear | wait <pid> [reason] | unwait | gate add <command> | gate list | gate remove <N> | gate clear";

/// Parses the arguments of `/goal`.
///
/// # Errors
///
/// The usage message for a malformed control command.
pub fn goal(args: &str) -> Result<GoalCommand, String> {
    let args = args.trim();
    if args.is_empty() {
        return Ok(GoalCommand::Status);
    }
    if let Some(rest) = args.strip_prefix("--")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        return set_text(rest);
    }
    let (word, rest) = split_word(args);
    let control = match word {
        "status" => GoalCommand::Status,
        "show" => GoalCommand::Show,
        "pause" => GoalCommand::Pause,
        "resume" => GoalCommand::Resume,
        "clear" | "stop" | "done" => GoalCommand::Clear,
        "unwait" => GoalCommand::Unwait,
        "draft" => return draft(rest),
        "wait" => return wait(rest),
        "gate" => return gate(rest),
        _ => return set_text(args),
    };
    Ok(control)
}

fn set_text(text: &str) -> Result<GoalCommand, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(GOAL_USAGE.to_owned());
    }
    Ok(GoalCommand::Set(text.to_owned()))
}

fn draft(rest: &str) -> Result<GoalCommand, String> {
    let text = rest.trim();
    if text.is_empty() {
        return Err("usage: /goal draft <objective>".to_owned());
    }
    Ok(GoalCommand::Draft(text.to_owned()))
}

fn wait(rest: &str) -> Result<GoalCommand, String> {
    let (pid, reason) = split_word(rest);
    let pid = pid
        .parse::<u32>()
        .map_err(|_| "usage: /goal wait <pid> [reason]".to_owned())?;
    Ok(GoalCommand::Wait {
        pid,
        reason: reason.trim().to_owned(),
    })
}

fn gate(rest: &str) -> Result<GoalCommand, String> {
    let (word, tail) = split_word(rest);
    match word {
        "" | "list" => Ok(GoalCommand::GateList),
        "clear" => Ok(GoalCommand::GateClear),
        "add" if !tail.trim().is_empty() => Ok(GoalCommand::GateAdd(tail.trim().to_owned())),
        "remove" => index(tail).map(GoalCommand::GateRemove),
        _ => Err("usage: /goal gate add <command> | list | remove <N> | clear".to_owned()),
    }
}

fn index(text: &str) -> Result<usize, String> {
    text.trim()
        .parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("'{}' is not a 1-based number", text.trim()))
}

/// The first whitespace-separated word and the rest.
fn split_word(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    match text.find(char::is_whitespace) {
        Some(at) => (&text[..at], &text[at..]),
        None => (text, ""),
    }
}

/// A parsed `/subgoal` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubgoalCommand {
    /// `/subgoal <text>`.
    Add(String),
    /// `/subgoal`.
    List,
    /// `/subgoal remove <N>`.
    Remove(usize),
    /// `/subgoal clear`.
    Clear,
}

/// Parses the arguments of `/subgoal`.
///
/// # Errors
///
/// A malformed `remove`.
pub fn subgoal(args: &str) -> Result<SubgoalCommand, String> {
    let args = args.trim();
    let (word, rest) = split_word(args);
    match word {
        "" => Ok(SubgoalCommand::List),
        "clear" if rest.trim().is_empty() => Ok(SubgoalCommand::Clear),
        "remove" if !rest.trim().is_empty() => index(rest).map(SubgoalCommand::Remove),
        _ => Ok(SubgoalCommand::Add(args.to_owned())),
    }
}

/// A parsed `/loop` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopCommand {
    /// Set (replacing any loop).
    Set(LoopSpec),
    /// `/loop status` (or `/loop`).
    Status,
    /// `/loop pause`.
    Pause,
    /// `/loop resume`.
    Resume,
    /// `/loop stop`.
    Stop,
}

/// What `/loop [interval] <prompt> [--times N] [--until <condition>]`
/// asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopSpec {
    /// Fixed interval; `None` is self-paced.
    pub every_secs: Option<u64>,
    /// The prompt or slash command.
    pub prompt: String,
    /// `--times N`.
    pub times: Option<u32>,
    /// `--until <condition>`.
    pub until: Option<String>,
}

/// The `/loop` usage line.
pub const LOOP_USAGE: &str = "usage: /loop [interval] <prompt-or-slash-command> [--times N] [--until <condition>] | status | pause | resume | stop";

/// Parses the arguments of `/loop`; `min_interval_secs` is
/// `loops.min_interval_secs`.
///
/// # Errors
///
/// The usage message, or the refusal of an interval below the floor.
pub fn looping(args: &str, min_interval_secs: u64) -> Result<LoopCommand, String> {
    let args = args.trim();
    match args {
        "" | "status" => return Ok(LoopCommand::Status),
        "pause" => return Ok(LoopCommand::Pause),
        "resume" => return Ok(LoopCommand::Resume),
        "stop" => return Ok(LoopCommand::Stop),
        _ => {}
    }
    let (text, times, until) = loop_flags(args)?;
    let (first, rest) = split_word(&text);
    let (every_secs, prompt) = match duration::parse(first) {
        Some(secs) => (Some(secs), rest.trim().to_owned()),
        None => (None, text.trim().to_owned()),
    };
    if let Some(secs) = every_secs
        && secs < min_interval_secs
    {
        return Err(format!(
            "interval must be at least {min_interval_secs}s (loops.min_interval_secs)"
        ));
    }
    if prompt.is_empty() {
        return Err(LOOP_USAGE.to_owned());
    }
    Ok(LoopCommand::Set(LoopSpec {
        every_secs,
        prompt,
        times,
        until,
    }))
}

/// Takes `--times N` and `--until <condition>` (which runs to the end or
/// to `--times`) out of `args`.
fn loop_flags(args: &str) -> Result<(String, Option<u32>, Option<String>), String> {
    let mut words: Vec<&str> = args.split_whitespace().collect();
    let mut times = None;
    if let Some(at) = words.iter().position(|w| *w == "--times") {
        let n = words
            .get(at + 1)
            .and_then(|w| w.parse::<u32>().ok())
            .filter(|n| *n > 0)
            .ok_or_else(|| "--times needs a positive number".to_owned())?;
        times = Some(n);
        words.drain(at..at + 2);
    }
    let mut until = None;
    if let Some(at) = words.iter().position(|w| *w == "--until") {
        let condition = words[at + 1..].join(" ");
        if condition.is_empty() {
            return Err("--until needs a condition".to_owned());
        }
        until = Some(condition);
        words.truncate(at);
    }
    Ok((words.join(" "), times, until))
}

/// A parsed `/heartbeat` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartbeatCommand {
    /// `/heartbeat every <interval> <prompt>`.
    Set {
        /// Seconds between fires.
        every_secs: u64,
        /// What each fire asks.
        prompt: String,
    },
    /// `/heartbeat status` (or `/heartbeat`).
    Status,
    /// `/heartbeat pause`.
    Pause,
    /// `/heartbeat resume`.
    Resume,
    /// `/heartbeat clear`.
    Clear,
}

/// The `/heartbeat` usage line.
pub const HEARTBEAT_USAGE: &str =
    "usage: /heartbeat every <interval> <prompt> | status | pause | resume | clear";

/// Parses the arguments of `/heartbeat`; `min_interval_secs` is
/// `heartbeat.min_interval_secs`.
///
/// # Errors
///
/// The usage message, or the refusal of an interval below the minimum.
pub fn heartbeat(args: &str, min_interval_secs: u64) -> Result<HeartbeatCommand, String> {
    let args = args.trim();
    match args {
        "" | "status" => return Ok(HeartbeatCommand::Status),
        "pause" => return Ok(HeartbeatCommand::Pause),
        "resume" => return Ok(HeartbeatCommand::Resume),
        "clear" => return Ok(HeartbeatCommand::Clear),
        _ => {}
    }
    let (word, rest) = split_word(args);
    if word != "every" {
        return Err(HEARTBEAT_USAGE.to_owned());
    }
    let (interval, prompt) = split_word(rest);
    let every_secs = duration::parse(interval).ok_or_else(|| HEARTBEAT_USAGE.to_owned())?;
    if every_secs < min_interval_secs {
        return Err(format!(
            "heartbeat interval must be at least {min_interval_secs}s"
        ));
    }
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err(HEARTBEAT_USAGE.to_owned());
    }
    Ok(HeartbeatCommand::Set {
        every_secs,
        prompt: prompt.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_control_word_escape_sets_literal_text() {
        assert_eq!(
            goal("-- pause the nightly cron"),
            Ok(GoalCommand::Set("pause the nightly cron".into()))
        );
        assert_eq!(goal("pause the nightly cron"), Ok(GoalCommand::Pause));
    }

    #[test]
    fn draft_is_matched_as_a_whole_word_only() {
        assert_eq!(
            goal("drafting docs"),
            Ok(GoalCommand::Set("drafting docs".into()))
        );
        assert_eq!(
            goal("draft migrate auth to JWT"),
            Ok(GoalCommand::Draft("migrate auth to JWT".into()))
        );
    }

    #[test]
    fn control_words_and_aliases_parse() {
        assert_eq!(goal(""), Ok(GoalCommand::Status));
        assert_eq!(goal("stop"), Ok(GoalCommand::Clear));
        assert_eq!(goal("done now"), Ok(GoalCommand::Clear));
        assert_eq!(
            goal("wait 4242 CI running"),
            Ok(GoalCommand::Wait {
                pid: 4242,
                reason: "CI running".into()
            })
        );
        assert_eq!(
            goal("gate add cargo test"),
            Ok(GoalCommand::GateAdd("cargo test".into()))
        );
        assert_eq!(goal("gate remove 1"), Ok(GoalCommand::GateRemove(1)));
        assert!(goal("gate remove 0").is_err());
        assert!(goal("wait abc").is_err());
    }

    #[test]
    fn subgoals_parse() {
        assert_eq!(subgoal(""), Ok(SubgoalCommand::List));
        assert_eq!(subgoal("remove 2"), Ok(SubgoalCommand::Remove(2)));
        assert_eq!(
            subgoal("add a regression test"),
            Ok(SubgoalCommand::Add("add a regression test".into()))
        );
    }

    #[test]
    fn an_interval_below_the_floor_is_refused() {
        assert_eq!(
            looping("10s check the queue", 30),
            Err("interval must be at least 30s (loops.min_interval_secs)".into())
        );
    }

    #[test]
    fn loops_parse_interval_flags_and_self_pacing() {
        assert_eq!(
            looping("2m poll CI --times 3", 30),
            Ok(LoopCommand::Set(LoopSpec {
                every_secs: Some(120),
                prompt: "poll CI".into(),
                times: Some(3),
                until: None,
            }))
        );
        assert_eq!(
            looping(
                "keep an eye on the migration --until the migration finished",
                30
            ),
            Ok(LoopCommand::Set(LoopSpec {
                every_secs: None,
                prompt: "keep an eye on the migration".into(),
                times: None,
                until: Some("the migration finished".into()),
            }))
        );
        assert_eq!(looping("", 30), Ok(LoopCommand::Status));
    }

    #[test]
    fn a_heartbeat_below_the_minimum_is_refused() {
        assert_eq!(
            heartbeat("every 30s ping", 60),
            Err("heartbeat interval must be at least 60s".into())
        );
        assert_eq!(
            heartbeat("every 10m check CI", 60),
            Ok(HeartbeatCommand::Set {
                every_secs: 600,
                prompt: "check CI".into()
            })
        );
    }
}
