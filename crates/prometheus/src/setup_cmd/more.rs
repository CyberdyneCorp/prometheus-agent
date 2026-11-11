//! The M2 setup sections and modes (spec: cli, Setup wizard modes): text
//! to speech, the agent (identity file, approval mode, iteration budget),
//! `--reset` of a section's settings, and the first-run offer to import
//! another agent's home through `prometheus migrate`.

use pmt_config::ProfileScope;
use pmt_config::layers::Source;
use pmt_core::error::{Error, codes};

use super::{Wizard, plain, write_changed};
use crate::cli::SetupSection;
use crate::context::Context;

/// Whether the profile sets `key` itself (its file, an override or the
/// managed layer) rather than taking the default.
pub(super) fn is_set(scope: &ProfileScope, key: &str) -> bool {
    scope.layers().source(key) != Source::Default
}

/// `key`'s value as shown in a question.
fn shown(scope: &ProfileScope, key: &str) -> String {
    scope
        .layers()
        .value(key)
        .map_or_else(|| "unset".to_owned(), plain)
}

/// `setup tts`: the speech provider and its API key.
pub(super) fn tts(scope: &ProfileScope, wizard: &mut Wizard<'_>) -> Result<(), Error> {
    const KEY: &str = "tts.provider";
    if wizard.quick && is_set(scope, KEY) {
        return wizard.say(&format!("Speech — provider {} (set)", shown(scope, KEY)));
    }
    let builtin = pmt_media::speech::tts::BUILTIN.join(", ");
    wizard.say(&format!(
        "Speech — provider: {} (available: {builtin}; Enter keeps it)",
        shown(scope, KEY)
    ))?;
    let Some(provider) = wizard.ask("TTS provider:")? else {
        return Ok(());
    };
    if write_changed(scope, &[(KEY, provider.clone())])? {
        wizard.say(&format!("{KEY} = \"{provider}\""))?;
    }
    let Some(secret) = pmt_media::speech::tts::secret(&provider) else {
        return Ok(());
    };
    if scope.secrets().resolve(secret).is_ok() {
        return Ok(());
    }
    if let Some(value) = wizard.ask_secret(secret)? {
        scope.secrets().store().set(secret, &value)?;
        wizard.say(&format!("{secret} stored in the secrets store"))?;
    }
    Ok(())
}

/// `setup agent`: the identity file, the approval mode and the iteration
/// budget.
pub(super) fn agent(scope: &ProfileScope, wizard: &mut Wizard<'_>) -> Result<(), Error> {
    wizard.say("Agent — identity, approvals and iteration budget")?;
    identity(scope, wizard)?;
    let mut pairs = Vec::new();
    for (key, question) in [
        ("approvals.mode", "approval mode (manual, smart, off)"),
        ("agent.max_iterations", "model calls per turn"),
    ] {
        if wizard.quick && is_set(scope, key) {
            continue;
        }
        if let Some(answer) = wizard.ask(&format!("{question} [{}]:", shown(scope, key)))? {
            pairs.push((key, answer));
        }
    }
    if write_changed(scope, &pairs)? {
        wizard.say("agent settings saved")?;
    }
    Ok(())
}

/// The identity file `SOUL.md`: a line of text replaces it.
fn identity(scope: &ProfileScope, wizard: &mut Wizard<'_>) -> Result<(), Error> {
    let path = scope.dir().join(pmt_config::profiles::IDENTITY_FILE);
    let present = path.is_file();
    if wizard.quick && present {
        return Ok(());
    }
    let state = if present { "present" } else { "none" };
    let name = pmt_config::profiles::IDENTITY_FILE;
    wizard.say(&format!("  identity file {name} (in the profile): {state}"))?;
    let Some(text) = wizard.ask("identity (who the agent is; Enter keeps it):")? else {
        return Ok(());
    };
    std::fs::write(&path, format!("{text}\n")).map_err(|e| super::io_error(&e))?;
    wizard.say(&format!("{} written", pmt_config::profiles::IDENTITY_FILE))
}

/// The settings `--reset` restores for `section` (secrets are never
/// touched).
fn reset_keys(section: SetupSection, platform: Option<&str>) -> Vec<String> {
    let keys: &[&str] = match section {
        SetupSection::Model => &["model.provider", "model.default"],
        SetupSection::Tools => &["tools.enabled_toolsets", "toolsets"],
        SetupSection::Terminal => &["terminal.backend"],
        SetupSection::Memory => &["memory.memory_enabled", "memory.user_profile_enabled"],
        SetupSection::Tts => &["tts.provider"],
        SetupSection::Agent => &[
            "approvals.mode",
            "agent.max_iterations",
            "prompt.load_identity",
        ],
        SetupSection::Secrets => &[],
        SetupSection::Gateway => {
            return platform
                .map(|p| vec![format!("gateway.platforms.{p}")])
                .unwrap_or_default();
        }
    };
    keys.iter().map(|k| (*k).to_owned()).collect()
}

/// `setup [section] --reset`: after a confirmation, the sections' keys
/// go back to their schema defaults; secrets are kept.
pub(super) fn reset(
    ctx: &Context<'_>,
    sections: &[SetupSection],
    platform: Option<&str>,
) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let keys: Vec<String> = sections
        .iter()
        .flat_map(|s| reset_keys(*s, platform))
        .filter(|k| is_set(&scope, k))
        .collect();
    if keys.is_empty() {
        eprintln!("nothing to reset: these settings are at their defaults");
        return Ok(0);
    }
    let mut wizard = Wizard::stdio();
    wizard.say(&format!(
        "Reset to defaults (secrets are kept): {}",
        keys.join(", ")
    ))?;
    if !wizard.confirm("Reset these settings?", false)? {
        wizard.say("nothing reset")?;
        return Ok(1);
    }
    let file = scope.profile().config_file();
    for key in &keys {
        file.unset(key)?;
    }
    wizard.say(&format!("{} setting(s) reset", keys.len()))?;
    Ok(0)
}

/// The other agents' homes present on this machine: `(label, the
/// `prometheus` arguments that import one into the profile)`.
fn importable(ctx: &Context<'_>, scope: &ProfileScope) -> Vec<(String, Vec<String>)> {
    let Some(user) = ctx.launch.user_home() else {
        return Vec::new();
    };
    let profile = scope.name().to_string();
    let mut found = Vec::new();
    let local = ctx.launch.local_app_data().map(std::path::PathBuf::as_path);
    let hermes = pmt_migrate::default_hermes_home(user, local);
    if hermes.is_dir() {
        // `-p` names Hermes profiles there: the target is `--into-profile`.
        let args = ["migrate", "from-hermes", "--into-profile", &profile];
        found.push((
            format!("Hermes ({})", hermes.display()),
            args.map(str::to_owned).to_vec(),
        ));
    }
    for detected in pmt_migrate::agents::detect(user, scope.name()) {
        let command = format!("from-{}", detected.agent.id());
        let args = ["-p", &profile, "migrate", &command];
        found.push((
            format!("{} ({})", detected.agent.label(), detected.source),
            args.map(str::to_owned).to_vec(),
        ));
    }
    found
}

/// On a profile's first setup: offers to import each detected agent home
/// through `prometheus migrate <from-...>` (asked once per profile).
pub(super) fn offer_import(ctx: &Context<'_>, wizard: &mut Wizard<'_>) -> Result<(), Error> {
    let scope = ctx.scope()?;
    let Ok(store) = pmt_store::Store::open(&scope) else {
        return Ok(());
    };
    if !store.mark_onboarding(pmt_store::onboarding::Step::SetupOffered, "import")? {
        return Ok(());
    }
    for (label, args) in importable(ctx, &scope) {
        if !wizard.confirm(&format!("Import from {label} into this profile?"), false)? {
            continue;
        }
        let exe = std::env::current_exe()
            .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot find this binary: {e}")))?;
        let status = std::process::Command::new(exe)
            .arg("--home")
            .arg(scope.home())
            .args(&args)
            .status()
            .map_err(|e| super::io_error(&e))?;
        if !status.success() {
            wizard.say(&format!(
                "the import from {label} did not finish (status {status})"
            ))?;
        }
    }
    Ok(())
}
