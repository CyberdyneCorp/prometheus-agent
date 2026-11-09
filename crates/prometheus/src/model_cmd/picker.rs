//! The interactive model picker (spec: model-providers, prometheus model
//! command; Fallback chain commands): choose a provider, then one of its
//! models (live listing when cached, else the built-in catalog) or type a
//! model id. It needs a terminal on stdin and stdout; prompts go to
//! stderr so stdout stays clean.

use std::io::{self, BufRead, Write};

use pmt_core::error::{Error, codes};
use pmt_providers::registry::Provider;

use super::{Models, REQUIRES_TERMINAL, set};
use crate::context::Context;
use crate::output::print;
use crate::prompt::{stdin_is_tty, stdout_is_tty};

/// The picker's subscription choice (spec: model-providers, Picker
/// offers ChatGPT sign-in).
pub(crate) const CHATGPT_CHOICE: &str = "ChatGPT or Codex subscription (sign in)";

/// The picker's local models choice (spec: local-models, Local models
/// command): the quickstart.
pub(crate) const LOCAL_CHOICE: &str = "Local models (run on this machine)";

/// What the picker returned.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Picked {
    /// `<provider>/<model>`.
    Model(String),
    /// Sign in to this provider, then make it the profile's provider.
    SignIn(&'static str),
    /// Run the local models quickstart.
    Local,
}

/// `prometheus model`: the picker, then `model set` with the choice; the
/// ChatGPT choice signs in to `openai-codex` and sets `model.provider`.
pub(super) fn run(ctx: &Context<'_>) -> Result<u8, Error> {
    if !(stdin_is_tty() && stdout_is_tty()) {
        return Err(Error::new(
            &codes::MODEL_REQUIRES_TERMINAL,
            REQUIRES_TERMINAL,
        ));
    }
    let models = Models::open(ctx)?;
    // The stdin lock is released before the quickstart reads stdin again.
    let picked = pick(&models, &mut io::stdin().lock(), &mut io::stderr(), true)?;
    match picked {
        None => print(&"no model chosen; nothing changed"),
        Some(Picked::Model(reference)) => set::run(ctx, &reference, None, None),
        Some(Picked::Local) => {
            let chosen =
                crate::local_cmd::quickstart::run(ctx, &mut io::stdin().lock(), &mut io::stderr())?;
            match chosen {
                Some(model) => set::run(ctx, &format!("llamacpp/{model}"), None, None),
                None => print(&"no local model set up; nothing changed"),
            }
        }
        Some(Picked::SignIn(provider)) => {
            crate::auth_cmd::sign_in_default(ctx, provider)?;
            models
                .scope
                .profile()
                .config_file()
                .set_all(&[("model.provider", provider)])?;
            print(&format!("signed in; model.provider = \"{provider}\""))
        }
    }
}

/// Runs the picker on the terminal and returns `<provider>/<model>`, or
/// `None` when the user leaves without choosing.
///
/// # Errors
///
/// `model.requires_terminal` with `refusal` when stdin or stdout is not a
/// terminal; `output.io`.
pub(crate) fn choose(ctx: &Context<'_>, refusal: &str) -> Result<Option<String>, Error> {
    if !(stdin_is_tty() && stdout_is_tty()) {
        return Err(Error::new(&codes::MODEL_REQUIRES_TERMINAL, refusal));
    }
    let models = Models::open(ctx)?;
    Ok(
        match pick(&models, &mut io::stdin().lock(), &mut io::stderr(), false)? {
            Some(Picked::Model(reference)) => Some(reference),
            _ => None,
        },
    )
}

/// The picker over any input and output; `offer_sign_in` adds the
/// subscription sign-in choice first.
///
/// # Errors
///
/// `output.io` when reading or writing fails.
pub(crate) fn pick(
    models: &Models,
    input: &mut impl BufRead,
    out: &mut impl Write,
    offer_sign_in: bool,
) -> Result<Option<Picked>, Error> {
    let providers: Vec<&Provider> = models
        .registry
        .iter()
        .filter(|p| !p.descriptor().hidden && p.enabled())
        .collect();
    let mut names: Vec<String> = Vec::new();
    if offer_sign_in {
        names.push(CHATGPT_CHOICE.to_owned());
    }
    names.extend(providers.iter().map(|p| provider_label(models, p)));
    if offer_sign_in {
        names.push(LOCAL_CHOICE.to_owned());
    }
    let Some(mut index) = ask(input, out, "provider", &names)? else {
        return Ok(None);
    };
    if offer_sign_in {
        // The sign-in comes first and the local models quickstart last.
        match index {
            0 => return Ok(Some(Picked::SignIn("openai-codex"))),
            i if i == names.len() - 1 => return Ok(Some(Picked::Local)),
            _ => index -= 1,
        }
    }
    let provider = providers[index];
    super::list::list_scoped(models, provider);
    let choices = models.model_ids(provider);
    let Some(model) = ask_or_type(input, out, "model", &choices)? else {
        return Ok(None);
    };
    Ok(Some(Picked::Model(format!("{}/{model}", provider.id()))))
}

/// A provider's picker row: its id (with the display name of the MoA
/// provider) and whether it is connected.
fn provider_label(models: &Models, provider: &Provider) -> String {
    let ready = if connected(models, provider) {
        "  (connected)"
    } else {
        ""
    };
    if provider.id() == pmt_providers::moa::PROVIDER_ID {
        format!(
            "{} ({}){ready}",
            provider.id(),
            pmt_providers::moa::DISPLAY_NAME
        )
    } else {
        format!("{}{ready}", provider.id())
    }
}

/// The connected providers and their picker models, for a chat's model
/// picker (spec: messaging-gateway, Interactive model picker): only
/// providers that can answer now, each with at least one model.
pub(crate) fn chat_choices(models: &Models) -> Vec<pmt_gateway::messaging::ModelChoice> {
    models
        .registry
        .iter()
        .filter(|p| !p.descriptor().hidden && p.enabled() && connected(models, p))
        .map(|p| pmt_gateway::messaging::ModelChoice {
            provider: p.id().to_owned(),
            models: models.model_ids(p),
        })
        .filter(|c| !c.models.is_empty())
        .collect()
}

/// `moa/<preset>` for every Mixture of Agents preset of `config`: the
/// models of the `Mixture of Agents` provider row in the TUI and remote
/// pickers (`model.options`).
pub(crate) fn preset_ids(config: &pmt_config::Config) -> Vec<String> {
    config
        .moa
        .names()
        .into_iter()
        .map(|name| format!("{}/{name}", pmt_providers::moa::PROVIDER_ID))
        .collect()
}

/// Whether `provider` needs no key, one of its secrets is stored or it
/// has a usable sign-in.
fn connected(models: &Models, provider: &Provider) -> bool {
    provider.auth() == pmt_config::schema::ProviderAuth::None
        || provider
            .secret_names()
            .iter()
            .any(|name| models.scope.secrets().resolve(name).is_ok())
        || models
            .accounts
            .credentials()
            .usable(provider.id())
            .is_ok_and(|s| s.is_some())
}

/// Lists `items` numbered and reads a number or an item; `None` on an
/// empty answer.
pub(crate) fn ask(
    input: &mut impl BufRead,
    out: &mut impl Write,
    what: &str,
    items: &[String],
) -> Result<Option<usize>, Error> {
    for (i, item) in items.iter().enumerate() {
        writeln!(out, "{:>3}) {item}", i + 1).map_err(|e| io_error(&e))?;
    }
    loop {
        let Some(answer) = read(input, out, &format!("{what} number (empty to cancel): "))? else {
            return Ok(None);
        };
        let by_number = answer
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=items.len()).contains(n))
            .map(|n| n - 1);
        let by_name = items
            .iter()
            .position(|item| item.split_whitespace().next() == Some(answer.as_str()));
        if let Some(index) = by_number.or(by_name) {
            return Ok(Some(index));
        }
        writeln!(out, "no {what} '{answer}'").map_err(|e| io_error(&e))?;
    }
}

/// Like [`ask`], but any other text is taken as a model id.
fn ask_or_type(
    input: &mut impl BufRead,
    out: &mut impl Write,
    what: &str,
    items: &[String],
) -> Result<Option<String>, Error> {
    for (i, item) in items.iter().enumerate() {
        writeln!(out, "{:>3}) {item}", i + 1).map_err(|e| io_error(&e))?;
    }
    let prompt = format!("{what} number or id (empty to cancel): ");
    let Some(answer) = read(input, out, &prompt)? else {
        return Ok(None);
    };
    let chosen = answer
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=items.len()).contains(n))
        .map_or(answer, |n| items[n - 1].clone());
    Ok(Some(chosen))
}

/// One trimmed line; `None` when empty or at end of input.
fn read(
    input: &mut impl BufRead,
    out: &mut impl Write,
    prompt: &str,
) -> Result<Option<String>, Error> {
    write!(out, "{prompt}").map_err(|e| io_error(&e))?;
    out.flush().map_err(|e| io_error(&e))?;
    let mut line = String::new();
    input.read_line(&mut line).map_err(|e| io_error(&e))?;
    let line = line.trim();
    Ok((!line.is_empty()).then(|| line.to_owned()))
}

fn io_error(err: &io::Error) -> Error {
    Error::new(&codes::OUTPUT_IO, format!("the model picker failed: {err}"))
}

#[cfg(test)]
mod tests {
    use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};

    use super::*;

    /// Scenario: Presets in every picker (spec: mixture-of-agents, Preset
    /// management commands).
    #[test]
    fn a_mixture_of_agents_row_lists_every_preset() {
        let target = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target");
        let dir = tempfile::tempdir_in(target).unwrap();
        let home = Home::open(dir.path()).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        profile
            .config_file()
            .replace(
                "schema_version = 1
                 [moa.presets.review]
reference_models = [{provider = \"mock\", model = \"a\"}]
                 aggregator = {provider = \"mock\", model = \"b\"}
",
            )
            .unwrap();
        let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
        let models = Models::for_config(scope.clone(), scope.config(), None).unwrap();
        // The CLI picker.
        let mut out = Vec::new();
        let picked = pick(&models, &mut "moa\n2\n".as_bytes(), &mut out, false).unwrap();
        assert_eq!(picked, Some(Picked::Model("moa/review".to_owned())));
        let shown = String::from_utf8(out).unwrap();
        assert!(shown.contains("moa (Mixture of Agents)"), "{shown}");
        assert!(
            shown.contains("1) default") && shown.contains("2) review"),
            "{shown}"
        );
        // The chat picker of the gateway.
        let choices = chat_choices(&models);
        let moa = choices.iter().find(|c| c.provider == "moa").unwrap();
        assert_eq!(moa.models, ["default", "review"]);
        // The TUI and remote pickers (`model.options`).
        assert_eq!(preset_ids(scope.config()), ["moa/default", "moa/review"]);
    }

    #[test]
    fn choosing_a_provider_then_a_listed_or_typed_model() {
        let target = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target");
        let dir = tempfile::tempdir_in(target).unwrap();
        let home = Home::open(dir.path()).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
        let models = Models::for_config(scope.clone(), scope.config(), None).unwrap();
        let mut out = Vec::new();
        let picked = pick(&models, &mut "anthropic\n1\n".as_bytes(), &mut out, false).unwrap();
        assert_eq!(
            picked,
            Some(Picked::Model("anthropic/claude-opus-4-1".to_owned()))
        );
        let typed = pick(
            &models,
            &mut "nope\ndeepseek\nmy-model\n".as_bytes(),
            &mut out,
            false,
        )
        .unwrap();
        assert_eq!(typed, Some(Picked::Model("deepseek/my-model".to_owned())));
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("no provider 'nope'")
        );
        assert_eq!(
            pick(&models, &mut "\n".as_bytes(), &mut Vec::new(), false).unwrap(),
            None
        );
    }

    #[test]
    fn picker_offers_the_chatgpt_sign_in_first() {
        let target = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target");
        let dir = tempfile::tempdir_in(target).unwrap();
        let home = Home::open(dir.path()).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
        let models = Models::for_config(scope.clone(), scope.config(), None).unwrap();
        let mut out = Vec::new();
        let picked = pick(&models, &mut "1\n".as_bytes(), &mut out, true).unwrap();
        assert_eq!(picked, Some(Picked::SignIn("openai-codex")));
        assert!(
            String::from_utf8(out)
                .unwrap()
                .starts_with(&format!("  1) {CHATGPT_CHOICE}"))
        );
        let second = pick(&models, &mut "2\n1\n".as_bytes(), &mut Vec::new(), true).unwrap();
        assert!(matches!(second, Some(Picked::Model(_))));
        let local = pick(&models, &mut "Local\n".as_bytes(), &mut Vec::new(), true).unwrap();
        assert_eq!(
            local,
            Some(Picked::Local),
            "the local models quickstart is offered"
        );
    }
}
