//! `prometheus skills install|uninstall|browse|search|inspect|check|
//! update|audit|accept|reset` for hub skills (spec: skills, Hub install
//! pipeline and trust policy; Skills lockfile and tamper detection; Hub
//! discovery, inspection, updates and audit; Hub source identifiers;
//! Blueprint skills never auto-schedule).

use std::fmt::Write as _;
use std::sync::Arc;

use pmt_core::error::{Error, codes};
use pmt_core::text::Catalog;
use pmt_net::http::OutboundPolicy;
use pmt_skillhub::http::{Client, Endpoints};
use pmt_skillhub::install::{self, Installed, Options};
use pmt_skillhub::sources::Listing;
use pmt_skillhub::{Hub, Identifier, inspect, maintain, search};
use pmt_store::Store;
use pmt_store::skill_usage::CreatedBy;
use serde_json::json;

use super::Env;
use crate::output::{json, print_raw};

/// The hub of the profile: HTTP from `network.*`, the services (a local
/// fake in debug builds with `PMT_TEST_SKILLS_HUB`), the tokens from the
/// secrets store.
///
/// # Errors
///
/// `skill.hub_unavailable` when the HTTP client cannot be built.
pub(crate) fn hub(env: &Env) -> Result<Hub, Error> {
    let network = &env.scope.config().network;
    let policy = OutboundPolicy {
        force_ipv4: network.force_ipv4,
        extra_ca_file: network.extra_ca_file.as_ref().map(Into::into),
        proxy: network.proxy.clone(),
    };
    let endpoints = env
        .launch
        .test_skills_hub()
        .map_or_else(Endpoints::public, Endpoints::under);
    let mut hub = Hub::new(
        Arc::new(Client::new(&policy)?),
        endpoints,
        env.scope.dir(),
        &env.scope.config().skills,
    );
    let secret = |name: &str| {
        env.scope
            .secrets()
            .resolve(name)
            .ok()
            .map(|v| v.expose().to_owned())
    };
    hub.github_token = secret("GITHUB_TOKEN");
    hub.clawhub_token = secret("CLAWHUB_TOKEN");
    Ok(hub)
}

/// Records what an install means outside the skill directory: the usage
/// row (`created_by = hub`) and, for a blueprint skill, its suggestion
/// (never a job).
pub(crate) fn after_install(env: &Env, installed: &Installed) {
    match Store::open(&env.scope) {
        Ok(store) => {
            if let Err(e) = store.set_skill_created_by(&installed.name, CreatedBy::Hub) {
                tracing::warn!(error = %e.message(), "skill usage row not recorded");
            }
            if installed.blueprint {
                let offered = crate::cron_host::blueprints::offer_skill_blueprints(
                    &store,
                    &env.scope,
                    &env.launch,
                );
                if let Err(e) = offered {
                    tracing::warn!(error = %e.message(), "blueprint suggestion not offered");
                }
            }
        }
        Err(e) => tracing::warn!(error = %e.message(), "skill usage row not recorded"),
    }
}

fn installed_json(i: &Installed) -> serde_json::Value {
    json!({
        "name": i.name, "path": i.path, "identifier": i.identifier,
        "trust": pmt_skillhub::sources::trust_text(i.trust),
        "verdict": i.report.verdict, "signature": i.signature.record(),
        "warnings": i.warnings, "blueprint": i.blueprint, "replaced": i.replaced,
    })
}

/// `skills install <identifier> [--force] [--name N]` or `--locked`.
pub(super) fn install(
    env: &Env,
    identifier: Option<&str>,
    force: bool,
    name: Option<&str>,
    locked: bool,
) -> Result<u8, Error> {
    let hub = hub(env)?;
    if locked {
        return install_locked(env, &hub);
    }
    let identifier = identifier.ok_or_else(|| {
        Error::new(
            &codes::CLI_USAGE,
            "skills install needs an identifier (or --locked)",
        )
    })?;
    let id = Identifier::parse(identifier)?;
    let options = Options {
        force,
        name: name.map(str::to_owned),
        ..Options::default()
    };
    let installed = install::install(&hub, &id, &options)?;
    after_install(env, &installed);
    if env.json {
        return json(&installed_json(&installed));
    }
    eprint!("{}", install_text(&*english(), &installed));
    Ok(0)
}

/// What an install tells the user.
pub(crate) fn install_text(catalog: &dyn Catalog, installed: &Installed) -> String {
    let mut text = String::new();
    for warning in &installed.warnings {
        let line = catalog.text("slash.skills.warning", &[("warning", warning)]);
        let _ = writeln!(text, "{line}");
    }
    let line = catalog.text(
        "slash.skills.installed",
        &[
            ("name", &installed.name),
            ("identifier", &installed.identifier),
            ("trust", &pmt_skillhub::sources::trust_text(installed.trust)),
            ("scan", &installed.report.verdict),
            ("signature", &installed.signature.record()),
            ("path", &installed.path.display()),
        ],
    );
    let _ = writeln!(text, "{line}");
    if installed.blueprint {
        let _ = writeln!(text, "{}", catalog.text("slash.skills.blueprint", &[]));
    }
    let _ = writeln!(text, "{}", catalog.text("slash.skills.next_session", &[]));
    text
}

fn install_locked(env: &Env, hub: &Hub) -> Result<u8, Error> {
    let rows = maintain::install_locked(hub)?;
    let failed = rows.iter().any(|r| r.action == "failed");
    if env.json {
        json(&json!({"skills": rows}))?;
    } else {
        let mut text = String::new();
        for row in &rows {
            let _ = writeln!(
                text,
                "{}: {}{}",
                row.name,
                row.action,
                row.detail
                    .as_deref()
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default()
            );
        }
        if rows.is_empty() {
            text.push_str("skills.lock lists no hub skill\n");
        }
        print_raw(&text)?;
    }
    if failed {
        return Err(Error::new(
            &codes::SKILL_LOCK_MISMATCH,
            "the locked skill set could not be reproduced",
        ));
    }
    Ok(0)
}

/// `skills uninstall <name>`.
pub(super) fn uninstall(env: &Env, name: &str) -> Result<u8, Error> {
    let dir = maintain::uninstall(&hub(env)?, name)?;
    if env.json {
        return json(&json!({"name": name, "removed": dir}));
    }
    eprintln!("uninstalled '{name}' ({})", dir.display());
    Ok(0)
}

pub(crate) fn listing_text(catalog: &dyn Catalog, listings: &[Listing]) -> String {
    let mut text = String::new();
    for l in listings {
        let installs = l
            .installs
            .map(|n| {
                let count = catalog.text("slash.skills.installs", &[("count", &n)]);
                format!(" · {count}")
            })
            .unwrap_or_default();
        let _ = writeln!(
            text,
            "{}  [{}]{installs}\n    {}",
            l.identifier,
            pmt_skillhub::sources::trust_text(l.trust),
            l.description
        );
    }
    if text.is_empty() {
        let _ = writeln!(text, "{}", catalog.text("slash.skills.none_found", &[]));
    }
    text
}

/// `skills browse [--source S]`.
pub(super) fn browse(env: &Env, source: Option<&str>) -> Result<u8, Error> {
    let listed = search::browse(&hub(env)?, source);
    if env.json {
        return json(&json!({"skills": listed}));
    }
    print_raw(&listing_text(&*english(), &listed))
}

/// `skills search <query> [--source S]`.
pub(super) fn search(env: &Env, query: &str, source: Option<&str>) -> Result<u8, Error> {
    let found = search::search(&hub(env)?, query, source);
    if env.json {
        return json(&json!({"query": query, "skills": found}));
    }
    print_raw(&listing_text(&*english(), &found))
}

/// `skills inspect <identifier>`: nothing is written.
pub(super) fn inspect(env: &Env, identifier: &str) -> Result<u8, Error> {
    let shown = inspect::inspect(&hub(env)?, &Identifier::parse(identifier)?)?;
    if env.json {
        return json(&shown);
    }
    print_raw(&inspect_text(&*english(), &shown))
}

/// What `inspect` shows.
pub(crate) fn inspect_text(catalog: &dyn Catalog, shown: &inspect::Inspection) -> String {
    let mut text = String::new();
    let mut field = |label: &str, value: &dyn std::fmt::Display| {
        let label = catalog.text(&format!("slash.skills.field.{label}"), &[]);
        let _ = writeln!(text, "{label}: {value}");
    };
    field("identifier", &shown.identifier);
    field("name", &shown.name);
    field("trust", &shown.trust);
    field("ref", &shown.resolved_ref);
    field("scan", &shown.scan.summary());
    field("signature", &shown.signature);
    if let Some(url) = &shown.upstream.repo_url {
        field("upstream", url);
    }
    if let Some(n) = shown.upstream.installs {
        field("installs", &n);
    }
    if let Some(audit) = &shown.upstream.audit {
        field("upstream_audit", audit);
    }
    field("files", &shown.files.join(", "));
    let _ = write!(text, "\n{}", shown.skill_md);
    text
}

/// `skills check [<name>]`: exit 1 when anything needs attention.
pub(super) fn check(env: &Env, name: Option<&str>) -> Result<u8, Error> {
    let rows = maintain::check(&hub(env)?, name)?;
    let attention = rows
        .iter()
        .any(|r| r.state != "up_to_date" || r.locally_modified);
    if env.json {
        json(&json!({"skills": rows}))?;
        return Ok(u8::from(attention));
    }
    print_raw(&check_text(&*english(), &rows))?;
    Ok(u8::from(attention))
}

/// The `check` rows.
pub(crate) fn check_text(catalog: &dyn Catalog, rows: &[maintain::Checked]) -> String {
    let mut text = String::new();
    for r in rows {
        let modified = if r.locally_modified {
            format!(" ({})", catalog.text("slash.skills.locally_modified", &[]))
        } else {
            String::new()
        };
        let detail = r
            .detail
            .as_deref()
            .map(|d| format!(": {d}"))
            .unwrap_or_default();
        let _ = writeln!(text, "{}: {}{modified}{detail}", r.name, r.state);
    }
    if rows.is_empty() {
        let _ = writeln!(text, "{}", catalog.text("slash.skills.no_hub_skills", &[]));
    }
    text
}

/// `skills update [<name>] [--force]`.
pub(super) fn update(env: &Env, name: Option<&str>, force: bool) -> Result<u8, Error> {
    let rows = maintain::update(&hub(env)?, name, force)?;
    let failed = rows.iter().any(|r| r.action == "failed");
    if env.json {
        json(&json!({"skills": rows}))?;
        return Ok(u8::from(failed));
    }
    print_raw(&update_text(&*english(), &rows))?;
    Ok(u8::from(failed))
}

/// The `update` rows.
pub(crate) fn update_text(catalog: &dyn Catalog, rows: &[maintain::Updated]) -> String {
    let mut text = String::new();
    for r in rows {
        let detail = r
            .detail
            .as_deref()
            .map(|d| format!(" ({d})"))
            .unwrap_or_default();
        let _ = writeln!(text, "{}: {}{detail}", r.name, r.action);
    }
    if rows.is_empty() {
        let _ = writeln!(text, "{}", catalog.text("slash.skills.no_hub_skills", &[]));
    }
    text
}

/// `skills audit`: exit 1 when a skill was quarantined.
pub(super) fn audit(env: &Env) -> Result<u8, Error> {
    let rows = maintain::audit(&hub(env)?)?;
    let quarantined = rows.iter().any(|r| r.quarantined);
    if env.json {
        json(&json!({"skills": rows}))?;
        return Ok(u8::from(quarantined));
    }
    print_raw(&audit_text(&*english(), &rows))?;
    Ok(u8::from(quarantined))
}

/// The `audit` rows.
pub(crate) fn audit_text(catalog: &dyn Catalog, rows: &[maintain::Audited]) -> String {
    let mut text = String::new();
    for r in rows {
        let note = if r.quarantined {
            format!(" -> {}", catalog.text("slash.skills.quarantined", &[]))
        } else {
            String::new()
        };
        let _ = writeln!(text, "{}: {}{note}", r.name, r.summary);
    }
    if rows.is_empty() {
        let _ = writeln!(text, "{}", catalog.text("slash.skills.no_hub_skills", &[]));
    }
    text
}

/// `skills accept <name>`.
pub(super) fn accept(env: &Env, name: &str) -> Result<u8, Error> {
    let digest = maintain::accept(&hub(env)?, name)?;
    if env.json {
        return json(&json!({"name": name, "content_digest": digest}));
    }
    eprintln!("accepted the current files of '{name}' ({digest}); it loads from the next session");
    Ok(0)
}

/// `skills reset <name>` for a hub skill; `None` when `name` is not one.
pub(super) fn reset(env: &Env, name: &str) -> Option<Result<u8, Error>> {
    let hub = match hub(env) {
        Ok(hub) => hub,
        Err(e) => return Some(Err(e)),
    };
    maintain::entry(&hub, name).ok()?;
    Some(maintain::reset(&hub, name).map(|()| {
        eprintln!("restored '{name}' to its locked content");
        0
    }))
}

/// `prometheus skills` reports are English, like every command's.
fn english() -> Arc<pmt_i18n::Strings> {
    pmt_i18n::Strings::english(pmt_i18n::Surface::Core)
}
