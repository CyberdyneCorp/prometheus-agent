//! `prometheus setup [model|secrets|tools|terminal|memory|gateway]` (spec:
//! cli, Setup wizard; Onboarding state; configuration-and-profiles,
//! Secrets are not configuration): a line-by-line wizard that shows each
//! current value, keeps it on Enter and writes only through the config
//! writer (`config.toml`) and the secrets store (`secrets.toml` / the
//! keychain), so accepting every default changes nothing. `--non-
//! interactive` takes values from `--set` and secrets from the
//! environment, exiting 78 when a required one is missing.

mod model;
pub mod sections;

use std::io::{self, BufRead, Write};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};

use crate::cli::{SetupArgs, SetupSection};
use crate::context::Context;

/// The wizard's terminal: answers from stdin, prompts on stderr.
pub(crate) struct Wizard<'a> {
    input: Box<dyn BufRead + 'a>,
    out: Box<dyn Write + 'a>,
    /// Whether stdin is a terminal (secrets are then read hidden).
    tty: bool,
}

impl Wizard<'_> {
    /// Stdin answers, stderr prompts.
    pub(crate) fn stdio() -> Wizard<'static> {
        Wizard {
            input: Box::new(io::BufReader::new(io::stdin())),
            out: Box::new(io::stderr()),
            tty: crate::prompt::stdin_is_tty(),
        }
    }

    /// Writes one line.
    pub(crate) fn say(&mut self, text: &str) -> Result<(), Error> {
        writeln!(self.out, "{text}").map_err(|e| io_error(&e))
    }

    /// Asks `question`; `None` for an empty answer or end of input.
    pub(crate) fn ask(&mut self, question: &str) -> Result<Option<String>, Error> {
        write!(self.out, "{question} ").map_err(|e| io_error(&e))?;
        self.out.flush().map_err(|e| io_error(&e))?;
        let mut line = String::new();
        self.input.read_line(&mut line).map_err(|e| io_error(&e))?;
        let line = line.trim();
        Ok((!line.is_empty()).then(|| line.to_owned()))
    }

    /// Asks for a secret (hidden on a terminal); `None` to skip.
    pub(crate) fn ask_secret(
        &mut self,
        name: &str,
    ) -> Result<Option<pmt_core::secret::SecretString>, Error> {
        let question = format!("value for {name} (input hidden; Enter skips):");
        let value = if self.tty {
            rpassword::prompt_password(format!("{question} ")).map_err(|e| io_error(&e))?
        } else {
            self.ask(&question)?.unwrap_or_default()
        };
        Ok((!value.is_empty()).then(|| pmt_core::secret::SecretString::new(value)))
    }

    /// Yes/no with a default.
    pub(crate) fn confirm(&mut self, question: &str, default: bool) -> Result<bool, Error> {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        Ok(match self.ask(&format!("{question} {hint}"))? {
            None => default,
            Some(answer) => matches!(answer.to_lowercase().as_str(), "y" | "yes"),
        })
    }
}

pub(crate) fn io_error(err: &io::Error) -> Error {
    Error::new(&codes::OUTPUT_IO, format!("the setup wizard failed: {err}"))
}

/// Runs `prometheus setup`.
///
/// # Errors
///
/// `setup.missing_value` (78) in a non-interactive run; configuration,
/// secrets and sign-in errors.
pub fn run(args: &SetupArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    if args.reset_onboarding {
        let scope = ctx.scope()?;
        let removed = pmt_store::Store::open(&scope)?.reset_onboarding()?;
        return crate::output::print(&format!("onboarding reset ({removed} records cleared)"));
    }
    let sections = match args.section {
        Some(section) => vec![section],
        None => vec![
            SetupSection::Model,
            SetupSection::Secrets,
            SetupSection::Terminal,
            SetupSection::Memory,
        ],
    };
    if args.non_interactive {
        return sections::non_interactive(ctx, &sections);
    }
    let mut wizard = Wizard::stdio();
    for section in sections {
        run_section(ctx, &mut wizard, section)?;
    }
    wizard.say("setup done")?;
    Ok(0)
}

fn run_section(
    ctx: &Context<'_>,
    wizard: &mut Wizard<'_>,
    section: SetupSection,
) -> Result<(), Error> {
    match section {
        SetupSection::Model => model::interactive(ctx, wizard),
        SetupSection::Secrets => sections::secrets(&ctx.scope()?, wizard),
        SetupSection::Tools => crate::tools_cmd::configure::interactive(ctx, None, wizard),
        SetupSection::Terminal => sections::terminal(&ctx.scope()?, wizard),
        SetupSection::Memory => sections::memory(&ctx.scope()?, wizard),
        SetupSection::Gateway => Err(sections::gateway_unavailable()),
    }
}

/// The interactive start of a profile with no model: offers `setup model`
/// (recorded in the onboarding state); returns whether a model was set up.
///
/// # Errors
///
/// Store and setup errors.
pub fn offer(ctx: &Context<'_>) -> Result<bool, Error> {
    let scope = ctx.scope()?;
    if let Ok(store) = pmt_store::Store::open(&scope) {
        store.mark_onboarding(pmt_store::onboarding::Step::SetupOffered, "model")?;
    }
    let mut wizard = Wizard::stdio();
    if !wizard.confirm(
        "No model is configured for this profile. Set one up now?",
        true,
    )? {
        return Ok(false);
    }
    model::interactive(ctx, &mut wizard)?;
    Ok(model::configured(&ctx.scope()?))
}

/// Writes `pairs` when any differs from the file's current value (so an
/// unchanged answer leaves `config.toml` byte-identical).
pub(crate) fn write_changed(scope: &ProfileScope, pairs: &[(&str, String)]) -> Result<bool, Error> {
    let layers = scope.layers();
    let changed: Vec<(&str, &str)> = pairs
        .iter()
        .filter(|(key, value)| {
            layers
                .value(key)
                .is_none_or(|current| plain(current) != *value)
        })
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    if changed.is_empty() {
        return Ok(false);
    }
    scope.profile().config_file().set_all(&changed)?;
    Ok(true)
}

/// A config value as the writer takes it (strings unquoted).
pub(crate) fn plain(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}
