//! What to complete at the cursor, worked out by walking the binary's own
//! clap command tree over the words typed so far: a subcommand, a flag, a
//! flag's value or a positional value. Walking the live tree (rather than
//! a tree frozen into the shell script) keeps completions right after an
//! update.

use clap::{Arg, Command};

/// What the word under the cursor is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Slot {
    /// A flag name (the word starts with `-`).
    Flag,
    /// The value of the option `arg` (the previous word was the option).
    OptionValue {
        /// The option's id.
        arg: String,
    },
    /// A subcommand of the current command or its next positional value.
    Word {
        /// The positional the word would fill, when there is one.
        arg: Option<String>,
    },
}

/// The command reached by the typed words and what the cursor fills.
#[derive(Debug)]
pub(super) struct Position {
    /// Subcommand names from the root, for example `["config", "set"]`.
    pub path: Vec<String>,
    /// What the cursor fills.
    pub slot: Slot,
    /// `--home` given on the line.
    pub home: Option<String>,
    /// `-p/--profile` given on the line.
    pub profile: Option<String>,
    /// No positional was typed yet, so a subcommand may follow.
    pub subcommand_allowed: bool,
}

/// Walks `words` (the typed words after the binary name, the cursor word
/// excluded) from `root`, which must be built (`Command::build`) so global
/// options are visible on every subcommand.
pub(super) fn locate(root: &Command, words: &[String], current: &str) -> Position {
    let mut walk = Walk {
        command: root,
        path: Vec::new(),
        positional: 0,
        pending: None,
        only_positional: false,
        home: None,
        profile: None,
    };
    for word in words {
        walk.step(word);
    }
    let slot = match walk.pending.take() {
        Some(arg) => Slot::OptionValue {
            arg: arg.get_id().to_string(),
        },
        None if !walk.only_positional && current.starts_with('-') => Slot::Flag,
        None => Slot::Word {
            arg: walk.positional_arg().map(|a| a.get_id().to_string()),
        },
    };
    Position {
        subcommand_allowed: walk.positional == 0,
        path: walk.path,
        slot,
        home: walk.home,
        profile: walk.profile,
    }
}

struct Walk<'a> {
    command: &'a Command,
    path: Vec<String>,
    positional: usize,
    pending: Option<&'a Arg>,
    only_positional: bool,
    home: Option<String>,
    profile: Option<String>,
}

impl<'a> Walk<'a> {
    fn step(&mut self, word: &str) {
        if let Some(arg) = self.pending.take() {
            self.record(arg, word);
            return;
        }
        if !self.only_positional && word == "--" {
            self.only_positional = true;
            return;
        }
        if !self.only_positional && word.starts_with('-') && word.len() > 1 {
            self.option(word);
            return;
        }
        if self.positional == 0
            && let Some(sub) = subcommand(self.command, word)
        {
            self.command = sub;
            self.path.push(sub.get_name().to_owned());
            return;
        }
        // A trailing multi-valued positional takes every later word.
        if !self.positional_arg().is_some_and(multiple) {
            self.positional += 1;
        }
    }

    fn option(&mut self, word: &str) {
        let (arg, inline) = if let Some(long) = word.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            (find_long(self.command, name), value)
        } else {
            let mut chars = word[1..].chars();
            let short = chars.next().unwrap_or('-');
            let rest = chars.as_str();
            let value = (!rest.is_empty()).then_some(rest);
            (find_short(self.command, short), value)
        };
        let Some(arg) = arg else { return };
        if !takes_value(arg) {
            return;
        }
        match inline {
            Some(value) => self.record(arg, value),
            None => self.pending = Some(arg),
        }
    }

    fn record(&mut self, arg: &Arg, value: &str) {
        match arg.get_id().as_str() {
            "home" => self.home = Some(value.to_owned()),
            "profile" => self.profile = Some(value.to_owned()),
            _ => {}
        }
    }

    fn positional_arg(&self) -> Option<&'a Arg> {
        let mut positionals: Vec<&Arg> = self
            .command
            .get_positionals()
            .filter(|a| !a.is_hide_set())
            .collect();
        positionals.sort_by_key(|a| a.get_index());
        let last_is_multiple = positionals.last().is_some_and(|a| multiple(a));
        match positionals.get(self.positional) {
            Some(arg) => Some(*arg),
            None if last_is_multiple => positionals.last().copied(),
            None => None,
        }
    }
}

fn subcommand<'a>(command: &'a Command, word: &str) -> Option<&'a Command> {
    command
        .get_subcommands()
        .find(|c| c.get_name() == word || c.get_all_aliases().any(|a| a == word))
}

fn find_long<'a>(command: &'a Command, name: &str) -> Option<&'a Arg> {
    command.get_arguments().find(|a| {
        a.get_long() == Some(name)
            || a.get_all_aliases()
                .is_some_and(|aliases| aliases.contains(&name))
    })
}

fn find_short(command: &Command, short: char) -> Option<&Arg> {
    command
        .get_arguments()
        .find(|a| a.get_short() == Some(short))
}

fn takes_value(arg: &Arg) -> bool {
    arg.get_action().takes_values() && !arg.is_positional()
}

fn multiple(arg: &Arg) -> bool {
    arg.get_num_args().is_some_and(|n| n.max_values() > 1)
        || matches!(arg.get_action(), clap::ArgAction::Append)
}

/// The command at `path` under `root`.
pub(super) fn command_at<'a>(root: &'a Command, path: &[String]) -> &'a Command {
    path.iter().fold(root, |command, name| {
        subcommand(command, name).unwrap_or(command)
    })
}

/// The visible flags of `command` (long forms, plus the short form for a
/// lone `-`), with their help.
pub(super) fn flags(command: &Command, current: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for arg in command.get_arguments().filter(|a| !a.is_hide_set()) {
        let help = arg.get_help().map(ToString::to_string).unwrap_or_default();
        if let Some(long) = arg.get_long() {
            out.push((format!("--{long}"), help.clone()));
        }
        if current == "-"
            && let Some(short) = arg.get_short()
        {
            out.push((format!("-{short}"), help));
        }
    }
    out
}

/// The visible subcommands of `command`, with their about text.
pub(super) fn subcommands(command: &Command) -> Vec<(String, String)> {
    command
        .get_subcommands()
        .filter(|c| !c.is_hide_set())
        .map(|c| {
            let about = c.get_about().map(ToString::to_string).unwrap_or_default();
            (c.get_name().to_owned(), about)
        })
        .collect()
}

/// The argument `id` of `command`.
pub(super) fn argument<'a>(command: &'a Command, id: &str) -> Option<&'a Arg> {
    command.get_arguments().find(|a| a.get_id() == id)
}

#[cfg(test)]
mod tests {
    use super::{Slot, locate};

    fn at(line: &[&str], current: &str) -> super::Position {
        let mut root = crate::cli::command();
        root.build();
        let words: Vec<String> = line.iter().map(|w| (*w).to_owned()).collect();
        locate(&root, &words, current)
    }

    #[test]
    fn subcommands_flags_and_values_are_located() {
        let p = at(&["config", "set"], "agent.max");
        assert_eq!(p.path, ["config", "set"]);
        assert_eq!(
            p.slot,
            Slot::Word {
                arg: Some("key".into())
            }
        );
        let p = at(&["--home", "/h", "-p", "work", "sessions", "show"], "");
        assert_eq!(p.path, ["sessions", "show"]);
        assert_eq!(p.home.as_deref(), Some("/h"));
        assert_eq!(p.profile.as_deref(), Some("work"));
        let p = at(&["chat", "-r"], "");
        assert_eq!(
            p.slot,
            Slot::OptionValue {
                arg: "resume".into()
            }
        );
        let p = at(&["--profile=lab", "profile"], "--");
        assert_eq!(p.slot, Slot::Flag);
        assert_eq!(p.profile.as_deref(), Some("lab"));
        let p = at(&["config", "set", "display.theme"], "");
        assert_eq!(
            p.slot,
            Slot::Word {
                arg: Some("value".into())
            }
        );
    }
}
