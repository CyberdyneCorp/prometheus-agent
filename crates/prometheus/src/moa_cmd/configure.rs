//! `prometheus moa configure [<name>]`: an interactive create or update of
//! one preset. Each slot is chosen with the model picker and validated
//! with its own credential (the aggregator must also call tools); the
//! cadence is asked last. Temperatures and `enabled` of an existing
//! preset are kept.

use std::io::{self, BufRead, Write};

use pmt_config::schema::{MoaFanout, MoaPreset, MoaSlot};
use pmt_core::error::{Error, codes};
use pmt_providers::moa::Fanout;
use pmt_providers::resolve::Trace;

use crate::context::Context;
use crate::model_cmd::picker::{self, Picked};
use crate::model_cmd::{Models, block_on, validate};
use crate::output::print;
use crate::prompt::{stdin_is_tty, stdout_is_tty};

/// The refusal without a terminal.
pub(super) const REQUIRES_TERMINAL: &str = "prometheus moa configure requires a terminal; edit [moa.presets.<name>] with 'prometheus config edit' instead";

pub(super) fn run(ctx: &Context<'_>, name: Option<&str>) -> Result<u8, Error> {
    if !(stdin_is_tty() && stdout_is_tty()) {
        return Err(Error::new(
            &codes::MODEL_REQUIRES_TERMINAL,
            REQUIRES_TERMINAL,
        ));
    }
    let models = Models::open(ctx)?;
    let config = models.scope.config();
    let name = name.map_or_else(|| config.moa.default_preset.clone(), str::to_owned);
    let Some(preset) = configure_with(&models, &name, &mut io::stdin().lock(), &mut io::stderr())?
    else {
        return print(&"no preset chosen; nothing changed");
    };
    super::put(&models.scope, &name, &preset)?;
    let mut proposed = config.clone();
    proposed.moa.presets.insert(name.clone(), preset);
    let mut text = format!("saved preset '{name}'; new sessions offer moa/{name}");
    if let Some(notice) = pmt_providers::moa::billing_notice(&proposed, &name) {
        text = format!("{text}\n{notice}");
    }
    print(&text)
}

/// Asks for preset `name`'s slots and cadence on `input`, prompting on
/// `out`; `None` when the user leaves without a complete preset.
///
/// # Errors
///
/// `output.io` when reading or writing fails.
pub(crate) fn configure_with(
    models: &Models,
    name: &str,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> Result<Option<MoaPreset>, Error> {
    let existing = models.scope.config().moa.preset(name);
    say(
        out,
        &format!("preset '{name}': choose the reference models, then the aggregator"),
    )?;
    let mut references = Vec::new();
    loop {
        say(out, &format!("reference model {}:", references.len() + 1))?;
        match pick_slot(models, input, out, false)? {
            Some(slot) => references.push(slot),
            None if references.is_empty() => return Ok(None),
            None => break,
        }
        if !yes(input, out, "add another reference? [y/N] ")? {
            break;
        }
    }
    say(
        out,
        "aggregator (acts with the session's tools; the whole tool loop is billed to it):",
    )?;
    let Some(aggregator) = pick_slot(models, input, out, true)? else {
        return Ok(None);
    };
    let fanout = ask_fanout(input, out, existing.as_ref().map(|p| &p.fanout))?;
    Ok(Some(MoaPreset {
        reference_models: references,
        aggregator,
        reference_temperature: existing.as_ref().and_then(|p| p.reference_temperature),
        aggregator_temperature: existing.as_ref().and_then(|p| p.aggregator_temperature),
        fanout,
        enabled: existing.as_ref().is_none_or(|p| p.enabled),
    }))
}

/// One slot from the picker, validated with its credential; a slot that
/// does not validate is reported and asked again.
fn pick_slot(
    models: &Models,
    input: &mut impl BufRead,
    out: &mut impl Write,
    aggregator: bool,
) -> Result<Option<MoaSlot>, Error> {
    loop {
        let Some(Picked::Model(reference)) = picker::pick(models, input, out, false)? else {
            return Ok(None);
        };
        match block_on(check(models, &reference, aggregator))? {
            Ok(slot) => return Ok(Some(slot)),
            Err(err) => say(
                out,
                &format!("{reference}: {}; choose another", err.message()),
            )?,
        }
    }
}

async fn check(models: &Models, reference: &str, aggregator: bool) -> Result<MoaSlot, Error> {
    let config = models.scope.config();
    let target = pmt_providers::target::parse(reference, &config.model, &models.registry, None)?;
    if target.provider == pmt_providers::moa::PROVIDER_ID {
        return Err(Error::new(
            &codes::CONFIG_INVALID,
            "a preset cannot use another Mixture of Agents preset (recursive MoA is not allowed)",
        ));
    }
    let resolver = models.resolver(config, None);
    let route = if aggregator {
        validate(&resolver, &models.catalog, &target).await?.0
    } else {
        resolver
            .resolve_target(&target, &mut Trace::default())
            .await?
    };
    Ok(MoaSlot {
        provider: route.provider,
        model: route.model,
        reasoning_effort: None,
    })
}

fn ask_fanout(
    input: &mut impl BufRead,
    out: &mut impl Write,
    current: Option<&MoaFanout>,
) -> Result<MoaFanout, Error> {
    let current = current.cloned().unwrap_or_default();
    let shown = match &current {
        MoaFanout::Text(text) => text.clone(),
        MoaFanout::Table { mode, n } => format!("{mode}:{}", n.unwrap_or_default()),
    };
    loop {
        let prompt = format!("advisor cadence: user_turn, per_iteration or every_n:N [{shown}]: ");
        let Some(answer) = read(input, out, &prompt)? else {
            return Ok(current);
        };
        let fanout = MoaFanout::Text(answer.clone());
        if Fanout::parse(&fanout).is_ok() {
            return Ok(fanout);
        }
        say(out, &format!("'{answer}' is not a cadence"))?;
    }
}

fn yes(input: &mut impl BufRead, out: &mut impl Write, prompt: &str) -> Result<bool, Error> {
    Ok(read(input, out, prompt)?.is_some_and(|a| a.eq_ignore_ascii_case("y")))
}

fn read(
    input: &mut impl BufRead,
    out: &mut impl Write,
    prompt: &str,
) -> Result<Option<String>, Error> {
    write!(out, "{prompt}")
        .and_then(|()| out.flush())
        .map_err(|e| io_error(&e))?;
    let mut line = String::new();
    input.read_line(&mut line).map_err(|e| io_error(&e))?;
    let line = line.trim();
    Ok((!line.is_empty()).then(|| line.to_owned()))
}

fn say(out: &mut impl Write, text: &str) -> Result<(), Error> {
    writeln!(out, "{text}").map_err(|e| io_error(&e))
}

fn io_error(err: &io::Error) -> Error {
    Error::new(&codes::OUTPUT_IO, format!("moa configure failed: {err}"))
}

#[cfg(test)]
mod tests {
    use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};

    use super::*;

    #[test]
    fn configure_picks_each_slot_refuses_nesting_and_asks_the_cadence() {
        let target = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target");
        let dir = tempfile::tempdir_in(target).unwrap();
        let home = Home::open(dir.path()).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        profile
            .config_file()
            .replace("schema_version = 1\n[providers.acme]\nbase_url = \"http://127.0.0.1:9/v1\"\n")
            .unwrap();
        let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
        let models = Models::for_config(scope.clone(), scope.config(), None).unwrap();
        // Two references, a refused `moa` aggregator, then a keyless one.
        let answers = "acme\nref-a\ny\nacme\nref-b\nn\nmoa\n1\nacme\nagg\nevery_n:3\n";
        let mut out = Vec::new();
        let preset = configure_with(&models, "team", &mut answers.as_bytes(), &mut out)
            .unwrap()
            .unwrap();
        let labels: Vec<String> = preset.reference_models.iter().map(MoaSlot::label).collect();
        assert_eq!(labels, ["acme/ref-a", "acme/ref-b"]);
        assert_eq!(preset.aggregator.label(), "acme/agg");
        assert_eq!(preset.fanout, MoaFanout::Text("every_n:3".into()));
        assert!(preset.enabled);
        let shown = String::from_utf8(out).unwrap();
        assert!(shown.contains("recursive MoA is not allowed"), "{shown}");
        // Leaving at the first reference changes nothing.
        let none = configure_with(&models, "team", &mut "\n".as_bytes(), &mut Vec::new()).unwrap();
        assert_eq!(none, None);
        super::super::put(&scope, "team", &preset).unwrap();
        let written = scope.profile().config_file().read().unwrap().0;
        assert!(written.contains("[moa.presets.team]"), "{written}");
        assert!(
            written.contains("aggregator = { provider = \"acme\", model = \"agg\" }"),
            "{written}"
        );
    }

    /// What `configure` saves goes through `put`, which refuses a slot on
    /// an unknown provider with exit 78 naming the key (spec:
    /// mixture-of-agents, MoA presets and validation).
    #[test]
    fn saving_a_slot_on_an_unknown_provider_is_refused_naming_the_key() {
        let target = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target");
        let dir = tempfile::tempdir_in(target).unwrap();
        let home = Home::open(dir.path()).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        profile
            .config_file()
            .replace("schema_version = 1\n")
            .unwrap();
        let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
        let slot = |provider: &str| MoaSlot {
            provider: provider.into(),
            model: "m".into(),
            reasoning_effort: None,
        };
        let preset = MoaPreset {
            reference_models: vec![slot("mock")],
            aggregator: slot("nowhere"),
            reference_temperature: None,
            aggregator_temperature: None,
            fanout: MoaFanout::default(),
            enabled: true,
        };
        let err = super::super::put(&scope, "far", &preset).unwrap_err();
        assert_eq!(err.code(), &codes::CONFIG_INVALID);
        assert_eq!(err.exit_code(), 78);
        assert!(
            err.message()
                .contains("moa.presets.far.aggregator.provider"),
            "{}",
            err.message()
        );
        let written = scope.profile().config_file().read().unwrap().0;
        assert!(!written.contains("moa.presets"), "{written}");
    }
}
