//! `prometheus plugins collection show|install|export` (spec:
//! plugin-system, Plugin collections).
//!
//! `show` fetches every entry at its pinned commit and prints the review:
//! plugin, source, ref, signature and declared capabilities. `install`
//! shows that review, asks one confirmation (a terminal is required: no
//! `--yes`, exit 4 otherwise), then runs the normal per-plugin install for
//! each entry (threat scan, signature policy, kill list, the `Enable` and
//! capability-grant questions), seeds `plugins.entries.<id>` where the user
//! has no value, and installs the listed skills. A failing plugin is
//! reported and the rest continue; any failure ends with exit 5.
//! `export` writes pinned entries for the plugins whose provenance pins a
//! commit, with their non-secret settings.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::exit;
use pmt_plugins::catalog::is_commit;
use pmt_plugins::collection::{self, Collection, Item};
use pmt_plugins::host::state::StateDir;
use pmt_plugins::manifest::describe;
use pmt_plugins::record::{InstallRecord, Origin};

use super::InstallArgs;
use super::install::{self, Fetched, GitPin};
use crate::context::Context;
use crate::plugins_host;

/// `prometheus plugins collection` actions.
#[derive(Debug, Subcommand)]
pub enum CollectionAction {
    /// Review a collection: every plugin, its source, pinned commit,
    /// signature and declared capabilities (fetches each entry).
    Show {
        /// A `prometheus-collection.toml` (or Hermes `hermes-pack.yaml`).
        file: PathBuf,
    },
    /// Review a collection, confirm once, then install each plugin with the
    /// normal install and capability-grant flow (needs a terminal).
    Install {
        /// A `prometheus-collection.toml` (or Hermes `hermes-pack.yaml`).
        file: PathBuf,
    },
    /// Print a collection of the installed plugins whose provenance pins a
    /// commit, with their non-secret settings.
    Export {
        /// Only the enabled plugins.
        #[arg(long)]
        enabled_only: bool,
    },
}

/// Runs one `collection` action.
pub(super) fn run(ctx: &Context<'_>, action: &CollectionAction) -> Result<u8, Error> {
    match action {
        CollectionAction::Show { file } => show(ctx, file),
        CollectionAction::Install { file } => install_all(ctx, file),
        CollectionAction::Export { enabled_only } => export(ctx, *enabled_only),
    }
}

fn read(file: &Path) -> Result<Collection, Error> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| Error::io(&codes::PLUGINS_COLLECTION, file, &e))?;
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Collection::parse(&name, &text)
}

/// The install options every collection entry is installed with: the
/// normal flow, no flags (a collection carries no `--allow-unsigned`,
/// `--force` or grants).
fn entry_args(item: &Item) -> InstallArgs {
    InstallArgs {
        identifier: item.label(),
        git_ref: Some(item.r#ref.clone()),
        allow_unsigned: false,
        allow_removed: false,
        enable: false,
        no_enable: false,
        grant: None,
        force: false,
    }
}

/// Fetches one entry at its pinned commit; `review` fetches an unsigned
/// one too (the review says so; installing applies the policy).
fn fetch(scope: &ProfileScope, item: &Item, review: bool) -> Result<Fetched, Error> {
    let mut args = entry_args(item);
    args.allow_unsigned = review;
    let pin = GitPin {
        commit: Some(&item.r#ref),
        subdir: item.subdir.as_deref(),
    };
    if let Some(repo) = &item.repo {
        let mut fetched = install::from_git(scope, repo, &args, None, &pin)?;
        if let Some(subdir) = &item.subdir {
            fetched.record.location = format!("{repo}#{subdir}");
        }
        return Ok(fetched);
    }
    let id = item.name.as_deref().unwrap_or_default();
    let catalog = plugins_host::catalog::current(scope, true);
    let entry = catalog.find(id).cloned().ok_or_else(|| {
        Error::new(
            &codes::PLUGINS_NOT_FOUND,
            format!("no plugin '{id}' in the catalog"),
        )
    })?;
    if let Some(removed) = catalog.blocked(&entry.id, Some(&entry.repo)) {
        return Err(removed.refusal(&entry.id));
    }
    if entry.is_package() {
        return Err(Error::new(
            &codes::PLUGINS_COLLECTION,
            format!(
                "entry '{id}': the catalog ships '{id}' as a package, not a commit; list it by `repo`"
            ),
        ));
    }
    let mut fetched = install::from_git(scope, &entry.repo, &args, Some(&entry), &pin)?;
    fetched.record.source = Origin::Catalog;
    fetched.record.location.clone_from(&entry.id);
    fetched.record.tier = Some(entry.tier.clone());
    fetched.repo = Some(entry.repo);
    Ok(fetched)
}

/// Every entry fetched, or why it could not be.
fn fetch_all(
    scope: &ProfileScope,
    collection: &Collection,
    review: bool,
) -> Vec<Result<Fetched, Error>> {
    collection
        .plugins
        .iter()
        .map(|item| fetch(scope, item, review))
        .collect()
}

/// The review `show` prints and `install` confirms.
fn review(
    scope: &ProfileScope,
    collection: &Collection,
    fetched: &[Result<Fetched, Error>],
) -> String {
    let mut text = format!(
        "Collection '{}': {} plugin(s)\n",
        if collection.name.is_empty() {
            "unnamed"
        } else {
            &collection.name
        },
        collection.plugins.len()
    );
    for (item, result) in collection.plugins.iter().zip(fetched) {
        let _ = writeln!(text, "\n{}\n  source {}", item.label(), item.source());
        let _ = writeln!(text, "  ref {}", item.r#ref);
        match result {
            Ok(fetched) => review_fetched(scope, &mut text, fetched),
            Err(err) => {
                let _ = writeln!(text, "  cannot be fetched: {}", err.message());
            }
        }
        if !item.config.is_empty() {
            let keys: Vec<&str> = item.config.keys().map(String::as_str).collect();
            let _ = writeln!(text, "  settings seeded: {}", keys.join(", "));
        }
    }
    if !collection.skills.is_empty() {
        let _ = writeln!(text, "\nskills: {}", collection.skills.join(", "));
    }
    text
}

fn review_fetched(scope: &ProfileScope, text: &mut String, fetched: &Fetched) {
    let manifest = &fetched.staged.manifest;
    let _ = writeln!(
        text,
        "  plugin '{}' {} ({})",
        manifest.id,
        manifest.version,
        manifest.kind.as_str()
    );
    let signer = fetched.record.signer.as_deref().map_or_else(
        || {
            format!(
                "unsigned ({})",
                fetched.unsigned.as_deref().unwrap_or("no signature")
            )
        },
        |key| format!("signed by {key}"),
    );
    let _ = writeln!(text, "  {signer}");
    match manifest
        .capabilities
        .requested(StateDir::new(scope.dir(), &manifest.id).root())
    {
        Ok(requested) if requested.is_empty() => text.push_str("  asks for no capabilities\n"),
        Ok(requested) => {
            text.push_str("  declared capabilities:\n");
            for capability in requested.iter() {
                let _ = writeln!(text, "    - {}", describe(capability));
            }
        }
        Err(err) => {
            let _ = writeln!(text, "  capabilities unreadable: {}", err.message());
        }
    }
}

fn show(ctx: &Context<'_>, file: &Path) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let collection = read(file)?;
    let fetched = fetch_all(&scope, &collection, true);
    print!("{}", review(&scope, &collection, &fetched));
    Ok(if fetched.iter().all(Result::is_ok) {
        exit::SUCCESS
    } else {
        exit::PARTIAL
    })
}

fn install_all(ctx: &Context<'_>, file: &Path) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let collection = read(file)?;
    if !crate::prompt::stdin_is_tty() {
        let err = Error::new(
            &codes::PLUGINS_APPROVAL_REQUIRED,
            "a collection installs only after you confirm it on a terminal",
        )
        .with_hint(format!(
            "review it with `prometheus plugins collection show {}`, then run the install on a terminal",
            file.display()
        ));
        crate::output::print_error(&err, ctx.json);
        return Ok(exit::APPROVAL_REQUIRED);
    }
    let fetched = fetch_all(&scope, &collection, false);
    eprint!("{}", review(&scope, &collection, &fetched));
    if !crate::prompt::confirm("\nInstall this collection?")? {
        eprintln!("collection install cancelled; nothing was installed");
        return Ok(exit::FAILURE);
    }
    let mut failures = Vec::new();
    for (item, result) in collection.plugins.iter().zip(fetched) {
        if let Err(err) = result.and_then(|f| install_one(ctx, &scope, item, f)) {
            eprintln!("{}: {}", item.label(), err.message());
            failures.push(item.label());
        }
    }
    for skill in &collection.skills {
        if let Err(err) = install_skill(ctx, skill) {
            eprintln!("skill {skill}: {}", err.message());
            failures.push(format!("skill {skill}"));
        }
    }
    if failures.is_empty() {
        eprintln!("collection installed");
        return Ok(exit::SUCCESS);
    }
    eprintln!("not installed: {}", failures.join(", "));
    Ok(exit::PARTIAL)
}

/// The normal install of one fetched entry, then its seeds and the
/// `Enable` question.
fn install_one(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    item: &Item,
    fetched: Fetched,
) -> Result<(), Error> {
    let args = entry_args(item);
    let id = fetched.staged.manifest.id.clone();
    super::python::check_install(&fetched.staged.dir, &fetched.staged.manifest, false)?;
    install::commit(scope, fetched, &args)?;
    eprintln!("installed '{id}' in profile '{}'", scope.name());
    seed(scope, &id, &item.config)?;
    super::python::setup(ctx, scope, &id);
    match install::finish(ctx, scope, &id, &args)? {
        0 => Ok(()),
        _ => Err(Error::new(
            &codes::PLUGINS_APPROVAL_REQUIRED,
            format!("'{id}' is installed but not enabled"),
        )),
    }
}

/// Writes the seeds the user has no value for into `plugins.entries.<id>`.
fn seed(
    scope: &ProfileScope,
    id: &str,
    seeds: &BTreeMap<String, serde_json::Value>,
) -> Result<(), Error> {
    let existing = scope.config().plugins.entries.get(id);
    let mut writes: Vec<(String, String)> = Vec::new();
    for (key, value) in seeds {
        if existing.is_some_and(|entry| entry.contains_key(key)) {
            continue;
        }
        let toml = toml::Value::try_from(value).map_err(|e| {
            Error::new(
                &codes::PLUGINS_COLLECTION,
                format!("seed '{key}' of '{id}' has no TOML form: {e}"),
            )
        })?;
        writes.push((format!("plugins.entries.{id}.{key}"), toml.to_string()));
    }
    if writes.is_empty() {
        return Ok(());
    }
    let pairs: Vec<(&str, &str)> = writes
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    scope.profile().config_file().set_all(&pairs)
}

fn install_skill(ctx: &Context<'_>, skill: &str) -> Result<(), Error> {
    let command = crate::skills_cmd::args::SkillsCommand::Install {
        identifier: Some(skill.to_owned()),
        force: false,
        name: None,
        locked: false,
    };
    match crate::skills_cmd::run(&command, ctx)? {
        0 => Ok(()),
        code => Err(Error::new(
            &codes::PLUGINS_COLLECTION,
            format!("`prometheus skills install {skill}` exited {code}"),
        )),
    }
}

fn export(ctx: &Context<'_>, enabled_only: bool) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let config = scope.config();
    let mut out = Collection {
        name: format!("{}-plugins", scope.name()),
        ..Collection::default()
    };
    for (id, record) in installed(&scope) {
        if enabled_only && !config.plugins.enabled.contains(&id) {
            continue;
        }
        let Some(mut item) = pinned(&id, &record) else {
            eprintln!(
                "skipped '{id}': its provenance ({}) pins no commit",
                record.source.as_str()
            );
            continue;
        };
        if let Some(settings) = config.plugins.entries.get(&id) {
            item.config = collection::exportable_seeds(settings);
        }
        out.plugins.push(item);
    }
    print!("{}", collection::to_toml(&out)?);
    Ok(exit::SUCCESS)
}

/// The profile's installed plugins with their records, by id.
fn installed(scope: &ProfileScope) -> Vec<(String, InstallRecord)> {
    let Ok(entries) = std::fs::read_dir(scope.dir().join("plugins")) else {
        return Vec::new();
    };
    let mut found: Vec<(String, InstallRecord)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let record = InstallRecord::read(&e.path()).ok()??;
            Some((e.file_name().to_string_lossy().into_owned(), record))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// The collection entry reproducing an install, when it pins a commit.
fn pinned(id: &str, record: &InstallRecord) -> Option<Item> {
    let commit = record.r#ref.as_deref().filter(|r| is_commit(r))?.to_owned();
    match record.source {
        Origin::Catalog => Some(Item {
            name: Some(id.to_owned()),
            r#ref: commit,
            ..Item::default()
        }),
        Origin::Git => {
            let (repo, subdir) = match record.location.split_once('#') {
                Some((repo, subdir)) => (repo.to_owned(), Some(subdir.to_owned())),
                None => (record.location.clone(), None),
            };
            Some(Item {
                repo: Some(repo),
                subdir,
                r#ref: commit,
                ..Item::default()
            })
        }
        _ => None,
    }
}
