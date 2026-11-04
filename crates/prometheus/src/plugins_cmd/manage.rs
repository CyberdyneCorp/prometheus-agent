//! `plugins list|enable|disable|grants` on the selected profile.
//!
//! Enabling is opt-in and needs the user's approval of the plugin's
//! capabilities, shown in plain language; the approval is recorded in
//! `grants.toml` for the plugin's exact content digest, so a changed
//! plugin needs approving again. Non-interactively `--grant` approves, and
//! without it the command exits 4 with `capability approval required`.

use std::fmt::Write as _;
use std::path::PathBuf;

use pmt_config::ProfileScope;
use pmt_config::grants::GrantsFile;
use pmt_core::capability::{Capability, CapabilitySet};
use pmt_core::error::{Error, codes};
use pmt_plugins::discovery::Discovered;
use pmt_plugins::grants::{Approval, Identity, approve, revoke, select};
use pmt_plugins::host::state::StateDir;
use pmt_plugins::load::{Status, survey};
use pmt_plugins::manifest::{Manifest, describe, label};
use pmt_plugins::record::{InstallRecord, Origin};
use serde::Serialize;

use super::GrantsAction;
use crate::context::Context;
use crate::output::{document, print_raw};
use crate::plugins_host;

/// `prometheus plugins list`.
pub(super) fn list(ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let loaded = plugins_host::for_session(&scope, &cwd()).current();
    if ctx.json {
        #[derive(Serialize)]
        struct Row<'a> {
            id: &'a str,
            version: Option<&'a str>,
            kind: Option<&'a str>,
            source: &'a str,
            state: &'a str,
            problem: Option<&'a str>,
            digest: Option<&'a str>,
            shadowed: &'a [PathBuf],
        }
        #[derive(Serialize)]
        struct Listing<'a> {
            profile: &'a str,
            plugins: Vec<Row<'a>>,
        }
        let plugins = loaded
            .statuses
            .iter()
            .map(|s| Row {
                id: &s.id,
                version: s.version.as_deref(),
                kind: s.kind.map(pmt_plugins::manifest::Kind::as_str),
                source: s.source.as_str(),
                state: s.state.as_str(),
                problem: s.problem.as_deref(),
                digest: s.digest.as_deref(),
                shadowed: &s.shadowed,
            })
            .collect();
        let profile = scope.name().as_str();
        return document("plugins list", &Listing { profile, plugins });
    }
    print_raw(&render(&loaded.statuses))
}

fn render(statuses: &[Status]) -> String {
    if statuses.is_empty() {
        return "no plugins found\n".to_owned();
    }
    let mut text = String::new();
    for s in statuses {
        let _ = write!(
            text,
            "{}  {}  {}  {}  {}",
            s.id,
            s.version.as_deref().unwrap_or("-"),
            s.kind.map_or("-", pmt_plugins::manifest::Kind::as_str),
            s.source.as_str(),
            s.state.as_str()
        );
        if let Some(problem) = &s.problem {
            let _ = write!(text, "  {problem}");
        }
        text.push('\n');
        for path in &s.shadowed {
            let _ = writeln!(text, "  shadows {}", path.display());
        }
    }
    text
}

/// A discovered plugin with a valid manifest, ready for grant decisions.
pub(super) struct Target {
    pub manifest: Manifest,
    pub identity: Identity,
    pub requested: CapabilitySet,
    pub dir: Option<PathBuf>,
}

/// The discovered plugin `id`.
pub(super) fn target(scope: &ProfileScope, id: &str) -> Result<Target, Error> {
    let version = plugins_host::running_version();
    let cwd = cwd();
    let blocked = plugins_host::catalog::kill_list(scope);
    let inputs = plugins_host::inputs(scope, &cwd, &version, &blocked);
    let found: Option<Discovered> = survey(&inputs)
        .into_iter()
        .map(|(d, _)| d)
        .find(|d| d.id == id);
    let Some(plugin) = found else {
        return Err(Error::new(
            &codes::PLUGINS_NOT_FOUND,
            format!("no plugin '{id}' was found"),
        )
        .with_hint("`prometheus plugins list` shows the discovered plugins"));
    };
    let manifest = plugin.manifest?;
    let digest = plugin.digest.ok_or_else(|| {
        Error::new(
            &codes::PLUGINS_DISCOVERY,
            format!("plugin '{id}' files are unreadable"),
        )
    })?;
    let requested = manifest
        .capabilities
        .requested(StateDir::new(scope.dir(), id).root())?;
    Ok(Target {
        identity: Identity {
            id: manifest.id.clone(),
            digest,
        },
        manifest,
        requested,
        dir: plugin.dir,
    })
}

/// The kill-list refusal of an installed plugin, unless its install
/// record allows it.
pub(super) fn check_blocked(scope: &ProfileScope, target: &Target) -> Result<(), Error> {
    let record = target
        .dir
        .as_deref()
        .and_then(|dir| InstallRecord::read(dir).ok().flatten());
    if record.as_ref().is_some_and(|r| r.flags.allow_removed) {
        return Ok(());
    }
    let repo = record
        .as_ref()
        .filter(|r| matches!(r.source, Origin::Git | Origin::Catalog))
        .map(|r| r.location.as_str());
    let catalog = pmt_plugins::catalog::Catalog {
        removed: plugins_host::catalog::kill_list(scope),
        ..pmt_plugins::catalog::Catalog::default()
    };
    match catalog.blocked(&target.manifest.id, repo) {
        Some(removed) => Err(removed.refusal(&target.manifest.id)),
        None => Ok(()),
    }
}

/// What the person decided about a plugin's capabilities.
pub(super) enum Decision {
    /// Approved (now or before).
    Approved,
    /// Declined on the terminal.
    Declined,
    /// Nobody could answer and `--grant` was not given.
    Unanswered,
}

/// Approves `target`'s capabilities: `--grant`, an approval already on
/// file, or the plain-language screen on a terminal.
pub(super) fn decide(
    scope: &ProfileScope,
    target: &Target,
    grant: Option<&str>,
) -> Result<Decision, Error> {
    let file = GrantsFile::of(scope.profile());
    if let Some(list) = grant {
        approve(
            &file,
            &target.identity,
            &select(&target.requested, list)?,
            "cli",
        )?;
        return Ok(Decision::Approved);
    }
    let approval = Approval::of(&file.load()?, &target.identity, &target.requested);
    if matches!(&approval, Approval::Approved { pending, .. } if pending.is_empty()) {
        return Ok(Decision::Approved);
    }
    Ok(match ask(target)? {
        Some(true) => {
            approve(&file, &target.identity, &target.requested, "cli")?;
            Decision::Approved
        }
        Some(false) => Decision::Declined,
        None => Decision::Unanswered,
    })
}

/// Prints `capability approval required` (exit 4) for `id`.
pub(super) fn approval_required(json: bool, id: &str, command: &str) -> u8 {
    let err = Error::new(
        &codes::PLUGINS_APPROVAL_REQUIRED,
        "capability approval required",
    )
    .with_hint(format!(
        "run `prometheus plugins {command} {id}` on a terminal, or pass --grant all"
    ));
    crate::output::print_error(&err, json);
    pmt_core::exit::APPROVAL_REQUIRED
}

/// `prometheus plugins enable <id> [--grant ...]`.
pub(super) fn enable(ctx: &Context<'_>, id: &str, grant: Option<&str>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let target = target(&scope, id)?;
    check_blocked(&scope, &target)?;
    match decide(&scope, &target, grant)? {
        Decision::Approved => {}
        Decision::Declined => {
            eprintln!("'{id}' was not enabled");
            return Ok(pmt_core::exit::FAILURE);
        }
        Decision::Unanswered => return Ok(approval_required(ctx.json, id, "enable")),
    }
    set_lists(&scope, id, true)?;
    eprintln!(
        "enabled '{id}' in profile '{}'; it loads in sessions started from now on",
        scope.name()
    );
    Ok(0)
}

/// The approval screen; `None` when no terminal can answer.
fn ask(target: &Target) -> Result<Option<bool>, Error> {
    let mut screen = format!(
        "Plugin '{}' {} ({}) asks to:\n",
        target.manifest.id, target.manifest.version, target.identity.digest
    );
    for capability in target.requested.iter() {
        let _ = writeln!(screen, "  - {}", describe(capability));
    }
    if !crate::prompt::stdin_is_tty() {
        eprint!("{screen}");
        return Ok(None);
    }
    Ok(Some(crate::prompt::confirm(&format!(
        "{screen}Approve these capabilities for '{}'?",
        target.manifest.id
    ))?))
}

/// `prometheus plugins disable <id>`.
pub(super) fn disable(ctx: &Context<'_>, id: &str) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    target(&scope, id).map(|_| ()).or_else(|err| {
        // A broken plugin can still be disabled.
        if err.code() == &codes::PLUGINS_NOT_FOUND {
            Err(err)
        } else {
            Ok(())
        }
    })?;
    set_lists(&scope, id, false)?;
    eprintln!("disabled '{id}' in profile '{}'", scope.name());
    Ok(0)
}

/// Adds `id` to `plugins.enabled` and removes it from `plugins.disabled`
/// (`enable`), or the reverse.
pub(super) fn set_lists(scope: &ProfileScope, id: &str, enable: bool) -> Result<(), Error> {
    let plugins = &scope.config().plugins;
    let (mut add_to, mut remove_from) = if enable {
        (plugins.enabled.clone(), plugins.disabled.clone())
    } else {
        (plugins.disabled.clone(), plugins.enabled.clone())
    };
    if !add_to.iter().any(|x| x == id) {
        add_to.push(id.to_owned());
    }
    remove_from.retain(|x| x != id);
    let (enabled, disabled) = if enable {
        (add_to, remove_from)
    } else {
        (remove_from, add_to)
    };
    scope.profile().config_file().set_all(&[
        ("plugins.enabled", &toml_list(&enabled)),
        ("plugins.disabled", &toml_list(&disabled)),
    ])
}

/// Removes `id` from `plugins.enabled` and `plugins.disabled` (`remove`).
pub(super) fn forget_lists(scope: &ProfileScope, id: &str) -> Result<(), Error> {
    let plugins = &scope.config().plugins;
    let keep =
        |list: &[String]| -> Vec<String> { list.iter().filter(|x| *x != id).cloned().collect() };
    scope.profile().config_file().set_all(&[
        ("plugins.enabled", &toml_list(&keep(&plugins.enabled))),
        ("plugins.disabled", &toml_list(&keep(&plugins.disabled))),
    ])
}

fn toml_list(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|s| format!("{s:?}")).collect();
    format!("[{}]", quoted.join(", "))
}

/// `prometheus plugins grants show|approve|revoke`.
pub(super) fn grants(ctx: &Context<'_>, action: &GrantsAction) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let file = GrantsFile::of(scope.profile());
    match action {
        GrantsAction::Show { id } => {
            let target = target(&scope, id)?;
            print_raw(&show(
                &target,
                &Approval::of(&file.load()?, &target.identity, &target.requested),
            ))
        }
        GrantsAction::Approve { id, capability } => {
            let target = target(&scope, id)?;
            let chosen = select(&target.requested, capability.as_deref().unwrap_or("all"))?;
            let added = approve(&file, &target.identity, &chosen, "cli")?;
            eprintln!("approved {} capabilities for '{id}'", added.len());
            Ok(0)
        }
        GrantsAction::Revoke { id, capability } => {
            let capability = capability
                .as_deref()
                .map(|text| one_capability(&scope, id, text))
                .transpose()?;
            let removed = revoke(&file, id, capability.as_ref())?;
            for grant in &removed {
                eprintln!("revoked {} ({})", grant.id, label(&grant.capability));
            }
            Ok(0)
        }
    }
}

/// A capability named in either spelling, resolved against what the
/// plugin requests when it can be found.
fn one_capability(scope: &ProfileScope, id: &str, text: &str) -> Result<Capability, Error> {
    if let Ok(target) = target(scope, id)
        && let Ok(set) = select(&target.requested, text)
        && let Some(capability) = set.iter().next()
    {
        return Ok(capability.clone());
    }
    Capability::parse(text)
}

fn show(target: &Target, approval: &Approval) -> String {
    let mut text = format!(
        "{} {} ({})\n",
        target.manifest.id, target.manifest.version, target.identity.digest
    );
    let granted = match approval {
        Approval::Approved { granted, .. } => granted.clone(),
        Approval::Changed => {
            text.push_str("the plugin changed since it was approved; nothing is granted\n");
            CapabilitySet::new()
        }
        Approval::Unapproved => CapabilitySet::new(),
    };
    if target.requested.is_empty() {
        text.push_str("requests no capabilities\n");
    }
    for capability in target.requested.iter() {
        let state = if granted.allows(capability) {
            "granted"
        } else {
            "pending"
        };
        let _ = writeln!(
            text,
            "{state}  {}  ({})",
            label(capability),
            describe(capability)
        );
    }
    text
}

pub(super) fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_default()
}
