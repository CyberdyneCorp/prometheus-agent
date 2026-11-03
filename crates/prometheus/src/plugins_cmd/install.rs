//! `prometheus plugins install <identifier>` (spec: plugin-system, plugins
//! CLI; Signed plugin packages and provenance; Catalog kill list;
//! security-boundaries, Signed plugins).
//!
//! The identifier is a `.pmtpkg` file, a Git URL (with `--ref <full
//! commit SHA>`; a short or symbolic ref is refused) or a catalog id
//! (resolved to its pinned ref). The plugin is staged, its signature
//! checked (catalog installs always need one unless `--allow-unsigned`;
//! local packages follow `plugins.signature_policy`; an invalid signature
//! is always refused), the kill list applied, and an unsigned install
//! shows its id, digest and requested capabilities first, and the tree is
//! threat-scanned ([`super::threat`]). It lands in
//! `<profile_home>/plugins/<id>/` with its install record, disabled unless
//! `--enable` (or a yes on the terminal's `Enable '<id>' now? [y/N]`).

use std::fmt::Write as _;
use std::path::Path;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_plugins::catalog::{Catalog, Entry, is_commit};
use pmt_plugins::host::state::StateDir;
use pmt_plugins::install::{self, Staged, Trust, check_signature};
use pmt_plugins::manifest::describe;
use pmt_plugins::package;
use pmt_plugins::record::{Flags, InstallRecord, Origin};

use super::InstallArgs;
use super::manage::{self, Decision};
use crate::context::Context;
use crate::plugins_host;

/// Runs the install.
pub(super) fn install(ctx: &Context<'_>, args: &InstallArgs) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let identifier = args.identifier.as_str();
    let fetched = if identifier.ends_with(&format!(".{}", package::EXTENSION))
        && Path::new(identifier).is_file()
    {
        from_package(&scope, Path::new(identifier), args)?
    } else if identifier.contains("://") || identifier.starts_with("git@") {
        from_git(&scope, identifier, args, None)?
    } else {
        from_catalog(&scope, identifier, args)?
    };
    let id = fetched.staged.manifest.id.clone();
    commit(&scope, fetched, args)?;
    eprintln!("installed '{id}' in profile '{}'", scope.name());
    finish(ctx, &scope, &id, args)
}

/// A staged plugin with what its record will say.
pub(super) struct Fetched {
    pub staged: Staged,
    pub record: InstallRecord,
    /// Why it has no verified signature, when it has none.
    pub unsigned: Option<String>,
    /// The repository the kill list matches, when known.
    pub repo: Option<String>,
}

fn record(source: Origin, location: &str, staged: &Staged, args: &InstallArgs) -> InstallRecord {
    InstallRecord {
        source,
        location: location.to_owned(),
        r#ref: None,
        package_digest: None,
        digest: staged.digest.clone(),
        signer: None,
        tier: None,
        installed_at: install::now(),
        flags: Flags {
            allow_unsigned: args.allow_unsigned,
            allow_removed: args.allow_removed,
            ..Flags::default()
        },
        held_update: None,
    }
}

/// Stages a local package and checks its signature (`<file>.minisig`)
/// against the catalog's key for its id.
pub(super) fn from_package(
    scope: &ProfileScope,
    path: &Path,
    args: &InstallArgs,
) -> Result<Fetched, Error> {
    let bytes = std::fs::read(path).map_err(|e| Error::io(&codes::PLUGINS_CATALOG, path, &e))?;
    let signature = std::fs::read_to_string(package::signature_path(path)).ok();
    let staged = install::stage_package(&bytes, scope.dir(), &plugins_host::running_version())?;
    let digest = package::digest(&bytes);
    let catalog = plugins_host::catalog::current(scope, false);
    let key = catalog
        .find(&staged.manifest.id)
        .map(|e| e.publisher_key.clone());
    let trust = check_signature(
        &digest,
        signature.as_deref(),
        key.as_deref(),
        &path.display().to_string(),
    )?;
    let unsigned = admit(scope, &trust, false, args)?;
    let location = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut record = record(Origin::Package, &location.to_string_lossy(), &staged, args);
    record.package_digest = Some(digest);
    record.signer = signer(&trust);
    record.flags.allow_unsigned = unsigned.is_some();
    Ok(Fetched {
        staged,
        record,
        unsigned,
        repo: None,
    })
}

fn signer(trust: &Trust) -> Option<String> {
    match trust {
        Trust::Signed(key) => Some(key.clone()),
        Trust::Unsigned(_) => None,
    }
}

/// The policy decision; `Some(why)` for an install that proceeds unsigned.
fn admit(
    scope: &ProfileScope,
    trust: &Trust,
    catalog: bool,
    args: &InstallArgs,
) -> Result<Option<String>, Error> {
    let policy = scope.config().plugins.signature_policy;
    let unsigned = install::admit(trust, catalog, policy, args.allow_unsigned)?;
    Ok(match trust {
        Trust::Unsigned(why) if unsigned => Some(why.clone()),
        _ => None,
    })
}

/// Stages a catalog entry at its pinned ref.
pub(super) fn from_catalog(
    scope: &ProfileScope,
    id: &str,
    args: &InstallArgs,
) -> Result<Fetched, Error> {
    let catalog = plugins_host::catalog::current(scope, true);
    let Some(entry) = catalog.find(id).cloned() else {
        return Err(Error::new(
            &codes::PLUGINS_NOT_FOUND,
            format!("no plugin '{id}' in the catalog"),
        )
        .with_hint("`prometheus plugins search <word>` searches it"));
    };
    if let Some(removed) = catalog.blocked(&entry.id, Some(&entry.repo))
        && !args.allow_removed
    {
        return Err(removed.refusal(&entry.id));
    }
    let mut fetched = if entry.is_package() {
        catalog_package(scope, &entry, args)?
    } else {
        from_git(scope, &entry.repo, args, Some(&entry))?
    };
    fetched.record.source = Origin::Catalog;
    fetched.record.location.clone_from(&entry.id);
    fetched.record.tier = Some(entry.tier.clone());
    fetched.repo = Some(entry.repo);
    Ok(fetched)
}

fn catalog_package(
    scope: &ProfileScope,
    entry: &Entry,
    args: &InstallArgs,
) -> Result<Fetched, Error> {
    let url = entry.package.as_deref().unwrap_or_default();
    let missing = || Error::new(&codes::PLUGINS_CATALOG, format!("{url} was not found"));
    let bytes = plugins_host::catalog::download(scope, url)?.ok_or_else(missing)?;
    let digest = package::digest(&bytes);
    if digest != entry.r#ref {
        return Err(Error::new(
            &codes::PLUGINS_SIGNATURE,
            format!("{url} does not match the catalog digest {}", entry.r#ref),
        ));
    }
    let signature = plugins_host::catalog::download(scope, &format!("{url}.minisig"))?
        .map(|b| String::from_utf8_lossy(&b).into_owned());
    let trust = check_signature(
        &digest,
        signature.as_deref(),
        Some(&entry.publisher_key),
        url,
    )?;
    let unsigned = admit(scope, &trust, true, args)?;
    let staged = install::stage_package(&bytes, scope.dir(), &plugins_host::running_version())?;
    if staged.manifest.id != entry.id {
        return Err(Error::new(
            &codes::PLUGINS_SIGNATURE,
            format!(
                "{url} holds plugin '{}', not '{}'",
                staged.manifest.id, entry.id
            ),
        ));
    }
    let mut record = record(Origin::Catalog, &entry.id, &staged, args);
    record.r#ref = Some(entry.r#ref.clone());
    record.package_digest = Some(digest);
    record.signer = signer(&trust);
    record.flags.allow_unsigned = unsigned.is_some();
    Ok(Fetched {
        staged,
        record,
        unsigned,
        repo: None,
    })
}

/// Checks out `url` at the pinned commit (`--ref`, or the catalog
/// entry's); the tree's `plugin.minisig` signs its content digest.
pub(super) fn from_git(
    scope: &ProfileScope,
    url: &str,
    args: &InstallArgs,
    entry: Option<&Entry>,
) -> Result<Fetched, Error> {
    let commit = match (entry, args.git_ref.as_deref()) {
        (Some(entry), _) => entry.r#ref.clone(),
        (None, Some(sha)) if is_commit(sha) => sha.to_lowercase(),
        _ => {
            return Err(
                Error::new(&codes::CLI_USAGE, "--ref must be a full commit SHA")
                    .with_hint("a branch, tag or short SHA can move; pin the 40-hex commit"),
            );
        }
    };
    let dir = install::staging_dir(scope.dir())?;
    let checkout = git_checkout(url, &commit, &dir);
    let signature = std::fs::read_to_string(dir.join(install::TREE_SIGNATURE)).ok();
    let _ = std::fs::remove_file(dir.join(install::TREE_SIGNATURE));
    let staged = install::stage_dir(dir, &plugins_host::running_version(), checkout)?;
    let key = match entry {
        Some(entry) => Some(entry.publisher_key.clone()),
        None => plugins_host::catalog::current(scope, false)
            .find(&staged.manifest.id)
            .map(|e| e.publisher_key.clone()),
    };
    let trust = check_signature(&staged.digest, signature.as_deref(), key.as_deref(), url)?;
    let unsigned = admit(scope, &trust, entry.is_some(), args)?;
    let mut record = record(Origin::Git, url, &staged, args);
    record.r#ref = Some(commit);
    record.signer = signer(&trust);
    record.flags.allow_unsigned = unsigned.is_some();
    Ok(Fetched {
        staged,
        record,
        unsigned,
        repo: Some(url.to_owned()),
    })
}

/// `git clone` then a detached checkout of `commit`; the `.git` directory
/// is dropped so only the plugin's files remain.
fn git_checkout(url: &str, commit: &str, dir: &Path) -> Result<(), Error> {
    let git = |args: &[&str]| -> Result<String, Error> {
        let out = std::process::Command::new("git")
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|e| Error::new(&codes::PLUGINS_CATALOG, format!("cannot run git: {e}")))?;
        if !out.status.success() {
            return Err(Error::new(
                &codes::PLUGINS_CATALOG,
                format!(
                    "git {} failed: {}",
                    args.first().copied().unwrap_or_default(),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let path = dir.to_string_lossy();
    git(&["clone", "--quiet", "--no-checkout", url, &path])?;
    git(&["-C", &path, "checkout", "--quiet", "--detach", commit])?;
    let head = git(&["-C", &path, "rev-parse", "HEAD"])?;
    if head != commit {
        return Err(Error::new(
            &codes::PLUGINS_CATALOG,
            format!("{url} checked out {head}, not {commit}"),
        ));
    }
    std::fs::remove_dir_all(dir.join(".git"))
        .map_err(|e| Error::io(&codes::PLUGINS_CATALOG, dir, &e))
}

/// Applies the kill list, shows the unsigned screen, then moves the plugin
/// into the profile with its record.
fn commit(scope: &ProfileScope, mut fetched: Fetched, args: &InstallArgs) -> Result<(), Error> {
    check_kill_list(scope, &fetched, args)?;
    super::threat::check(
        scope.config().plugins.scan_on_install,
        &fetched.staged,
        args.force,
        &mut fetched.record.flags,
    )?;
    if let Some(why) = &fetched.unsigned {
        unsigned_screen(scope, &fetched.staged, why)?;
    }
    install::commit(fetched.staged, scope.dir(), &fetched.record, false)?;
    Ok(())
}

/// Refuses a staged plugin (install or update) whose id or repository is
/// on the kill list, unless `--allow-removed` (or, for an update, the
/// install record) allows it.
///
/// # Errors
///
/// `plugins.removed` (77) with the kill list's reason.
pub(super) fn check_kill_list(
    scope: &ProfileScope,
    fetched: &Fetched,
    args: &InstallArgs,
) -> Result<(), Error> {
    let id = &fetched.staged.manifest.id;
    let catalog = Catalog {
        entries: Vec::new(),
        removed: plugins_host::catalog::kill_list(scope),
    };
    match catalog.blocked(id, fetched.repo.as_deref()) {
        Some(removed) if !args.allow_removed => Err(removed.refusal(id)),
        _ => Ok(()),
    }
}

/// What an unsigned install shows before it proceeds: id, digest and the
/// capabilities it asks for; on a terminal it asks to confirm.
fn unsigned_screen(scope: &ProfileScope, staged: &Staged, why: &str) -> Result<(), Error> {
    let manifest = &staged.manifest;
    let mut screen = format!(
        "Installing '{}' {} without a verified signature ({why}).\\ndigest {}\\n",
        manifest.id, manifest.version, staged.digest
    );
    let requested = manifest
        .capabilities
        .requested(StateDir::new(scope.dir(), &manifest.id).root())?;
    if requested.is_empty() {
        screen.push_str("it asks for no capabilities\\n");
    }
    for capability in requested.iter() {
        let _ = writeln!(screen, "  - {}", describe(capability));
    }
    if !crate::prompt::stdin_is_tty() {
        eprint!("{screen}");
        return Ok(());
    }
    if crate::prompt::confirm(&format!("{screen}Install it anyway?"))? {
        Ok(())
    } else {
        Err(Error::new(&codes::PLUGINS_UNSIGNED, "install cancelled"))
    }
}

/// `--enable`, `--no-enable` or the terminal's question.
fn finish(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    id: &str,
    args: &InstallArgs,
) -> Result<u8, Error> {
    let enable = if args.enable {
        true
    } else if args.no_enable || !crate::prompt::stdin_is_tty() {
        false
    } else {
        crate::prompt::confirm(&format!("Enable '{id}' now?"))?
    };
    if !enable {
        eprintln!("'{id}' is disabled; `prometheus plugins enable {id}` enables it");
        return Ok(0);
    }
    let target = manage::target(scope, id)?;
    match manage::decide(scope, &target, args.grant.as_deref())? {
        Decision::Approved => {
            manage::set_lists(scope, id, true)?;
            eprintln!("enabled '{id}'; it loads in sessions started from now on");
            Ok(0)
        }
        Decision::Declined => {
            eprintln!("'{id}' was installed but not enabled");
            Ok(pmt_core::exit::FAILURE)
        }
        Decision::Unanswered => Ok(manage::approval_required(ctx.json, id, "enable")),
    }
}
