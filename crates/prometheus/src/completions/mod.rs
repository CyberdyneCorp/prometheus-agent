//! Shell completions (spec: cli, Shell completions): `prometheus
//! completions <shell>` prints a small script that registers a completion
//! function for `prometheus` and `pmt`; on every TAB the function runs the
//! hidden `prometheus __complete <shell> [--current=<word>] -- <words>`,
//! which walks this binary's command tree ([`engine`]) and prints the
//! candidates, adding profile names, session ids and titles, config keys,
//! skill names and slash commands read from local files ([`values`]).

mod engine;
mod values;

use std::io::Write as _;
use std::path::PathBuf;

use clap::ValueEnum;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};

use self::engine::Slot;

/// A shell `completions` can register with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    /// GNU bash (3.2 and later).
    Bash,
    /// zsh.
    Zsh,
    /// fish.
    Fish,
    /// `PowerShell`.
    Powershell,
    /// elvish.
    Elvish,
}

/// Prints the registration script of `shell`.
///
/// # Errors
///
/// `output.io` when stdout cannot be written.
pub fn script(shell: Shell) -> Result<u8, Error> {
    let text = match shell {
        Shell::Bash => BASH,
        Shell::Zsh => ZSH,
        Shell::Fish => FISH,
        Shell::Powershell => POWERSHELL,
        Shell::Elvish => ELVISH,
    };
    crate::output::print_raw(text)
}

/// `prometheus __complete`: prints the candidates for the cursor word,
/// one per line (`value<TAB>help` for the shells that show help). Never
/// fails: a completion that cannot read something offers less.
///
/// # Errors
///
/// `output.io` when stdout cannot be written.
pub fn complete(
    shell: Shell,
    current: Option<&str>,
    words: &[String],
    launch: &LaunchEnv,
) -> Result<u8, Error> {
    // The words start with the binary name; without --current the last
    // word is the one under the cursor.
    let words = words.get(1..).unwrap_or_default();
    let (typed, current) = match current {
        Some(current) => (words, current),
        None => match words.split_last() {
            Some((last, typed)) => (typed, last.as_str()),
            None => (words, ""),
        },
    };
    let mut root = crate::cli::command();
    root.build();
    let position = engine::locate(&root, typed, current);
    let home = position
        .home
        .as_deref()
        .map(PathBuf::from)
        .or_else(|| pmt_config::home::resolve(None, launch).ok());
    let sources = values::Sources::new(home, position.profile.clone());
    let command = engine::command_at(&root, &position.path);
    let mut found = match &position.slot {
        Slot::Flag => engine::flags(command, current),
        Slot::OptionValue { arg } => {
            argument_values(&sources, &position.path, command, arg, current)
        }
        Slot::Word { arg } => {
            let mut found = if position.subcommand_allowed {
                engine::subcommands(command)
            } else {
                Vec::new()
            };
            if let Some(arg) = arg {
                found.extend(argument_values(
                    &sources,
                    &position.path,
                    command,
                    arg,
                    current,
                ));
            }
            found
        }
    };
    found.retain(|(value, _)| value.starts_with(current));
    found.dedup_by(|a, b| a.0 == b.0);
    print(shell, &found)
}

fn argument_values(
    sources: &values::Sources,
    path: &[String],
    command: &clap::Command,
    arg: &str,
    current: &str,
) -> Vec<(String, String)> {
    if let Some(arg) = engine::argument(command, arg) {
        let possible: Vec<(String, String)> = arg
            .get_possible_values()
            .iter()
            .filter(|v| !v.is_hide_set())
            .map(|v| {
                let help = v.get_help().map(ToString::to_string).unwrap_or_default();
                (v.get_name().to_owned(), help)
            })
            .collect();
        if !possible.is_empty() {
            return possible;
        }
    }
    values::kind(path, arg).map_or_else(Vec::new, |kind| sources.values(kind, current))
}

fn print(shell: Shell, found: &[(String, String)]) -> Result<u8, Error> {
    let mut out = String::new();
    for (value, help) in found {
        let help = help.lines().next().unwrap_or_default();
        let line = match shell {
            Shell::Bash | Shell::Elvish => value.clone(),
            // `_describe` splits value and help at the first unescaped ':'.
            Shell::Zsh => format!("{}:{help}", value.replace(':', "\\:")),
            Shell::Fish | Shell::Powershell => format!("{value}\t{help}"),
        };
        out.push_str(&line);
        out.push('\n');
    }
    std::io::stdout()
        .lock()
        .write_all(out.as_bytes())
        .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot write stdout: {e}")))?;
    Ok(0)
}

const BASH: &str = r#"# prometheus completions for bash; load with:
#   eval "$(prometheus completions bash)"
_prometheus() {
    local line
    COMPREPLY=()
    while IFS= read -r line; do
        COMPREPLY+=("$(printf '%q' "$line")")
    done < <("${COMP_WORDS[0]}" __complete bash -- "${COMP_WORDS[@]:0:COMP_CWORD+1}" 2>/dev/null)
}
complete -o default -F _prometheus prometheus pmt
"#;

const ZSH: &str = r#"#compdef prometheus pmt
# prometheus completions for zsh; load with:
#   source <(prometheus completions zsh)
_prometheus() {
    local -a candidates
    candidates=("${(@f)$("${words[1]}" __complete zsh -- "${(@)words[1,CURRENT]}" 2>/dev/null)}")
    candidates=(${candidates:#})
    if (( ${#candidates} == 0 )); then
        _files
        return
    fi
    _describe -t values 'prometheus' candidates
}
if [[ "${zsh_eval_context[-1]}" == loadautofunc ]]; then
    _prometheus "$@"
else
    compdef _prometheus prometheus pmt
fi
"#;

const FISH: &str = r"# prometheus completions for fish; load with:
#   prometheus completions fish | source
function __prometheus_complete
    set -l typed (commandline -opc)
    $typed[1] __complete fish --current=(commandline -ct) -- $typed 2>/dev/null
end
complete -c prometheus -f -a '(__prometheus_complete)'
complete -c pmt -f -a '(__prometheus_complete)'
";

const POWERSHELL: &str = r#"# prometheus completions for PowerShell; load with:
#   prometheus completions powershell | Out-String | Invoke-Expression
Register-ArgumentCompleter -Native -CommandName prometheus, pmt -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)
    $typed = @($commandAst.CommandElements |
        Where-Object { $_.Extent.EndOffset -lt $cursorPosition } |
        ForEach-Object { $_.ToString() })
    & $typed[0] __complete powershell "--current=$wordToComplete" -- @typed 2>$null | ForEach-Object {
        $value, $help = $_ -split "`t", 2
        if (-not $help) { $help = $value }
        [System.Management.Automation.CompletionResult]::new($value, $value, 'ParameterValue', $help)
    }
}
"#;

const ELVISH: &str = r"# prometheus completions for elvish; load with:
#   eval (prometheus completions elvish | slurp)
var prometheus-complete = {|@words|
    var current = $words[-1]
    var typed = $words[0..(- (count $words) 1)]
    (external $words[0]) __complete elvish --current=$current -- $@typed
}
set edit:completion:arg-completer[prometheus] = $prometheus-complete
set edit:completion:arg-completer[pmt] = $prometheus-complete
";
