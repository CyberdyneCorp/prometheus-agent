//! `prometheus profile` (spec: configuration-and-profiles, Profile
//! lifecycle commands, Safe profile deletion, Profile selection at process
//! start, Profile creation options, Profile alias launchers, Profile export
//! excludes credentials by default).

mod alias;
mod archive;

use pmt_config::profiles::{CloneFrom, user_of};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::error::Error;
use serde::Serialize;

use crate::cli::ProfileCommand;
use crate::context::Context;
use crate::output::{document, print_raw};
use crate::prompt;

/// Runs one `profile` subcommand and returns its exit code.
///
/// # Errors
///
/// The command's failure; the caller prints it and exits with its code.
pub fn run(command: &ProfileCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let home = ctx.home()?;
    match command {
        ProfileCommand::List => list(&home, ctx.json),
        ProfileCommand::Create {
            name,
            clone_from,
            clone_all,
            clone_secrets,
            clone_channels,
            sync_imports,
            no_skills,
            description,
            no_alias,
        } => {
            let source = match (clone_from, clone_all) {
                (Some(from), _) => Some(ProfileName::parse(from)?),
                (None, true) => Some(home.select_profile(ctx.profile)?),
                (None, false) => None,
            };
            let flags = [
                ("--clone-secrets", *clone_secrets),
                ("--clone-channels", *clone_channels),
                ("--sync-imports", *sync_imports),
            ];
            if let (None, Some((flag, _))) = (&source, flags.iter().find(|(_, on)| *on)) {
                return Err(Error::new(
                    &pmt_core::error::codes::CLI_USAGE,
                    format!("{flag} needs --clone-from or --clone-all"),
                ));
            }
            let clone = source.as_ref().map(|source| CloneFrom {
                secrets: *clone_secrets,
                all: *clone_all,
                channels: *clone_channels,
                sync_imports: *sync_imports,
                ..CloneFrom::of(source)
            });
            let profile = home.create_profile(name, clone)?;
            if let Some(source) = &source {
                clone_memory(&home, source, &profile)?;
            }
            if *no_skills {
                pmt_skills::optout::set_included(&profile, false)?;
            }
            if let Some(text) = description {
                let path = profile.path().join(pmt_config::profiles::DESCRIPTION_FILE);
                pmt_core::private_fs::write_atomic(&path, text.trim().as_bytes())
                    .map_err(|e| Error::io(&pmt_core::error::codes::HOME_IO, &path, &e))?;
            }
            eprintln!(
                "created profile '{}' at {}",
                profile.name(),
                profile.path().display()
            );
            if !*no_alias {
                default_alias(profile.name(), home.root(), ctx);
            }
            Ok(0)
        }
        ProfileCommand::Alias { name, alias } => {
            let name = ProfileName::parse(name)?;
            home.existing_profile(&name)?;
            let alias = alias.as_deref().unwrap_or(name.as_str());
            let path = alias::run(&name, alias, home.root(), ctx.launch)?;
            eprintln!("wrote {} (runs prometheus -p {name})", path.display());
            note_path(&path, ctx);
            Ok(0)
        }
        ProfileCommand::Export {
            name,
            output,
            with_sessions,
            with_secrets,
        } => archive::export(&home, name, output, *with_sessions, *with_secrets, ctx.json),
        ProfileCommand::Import { file, name } => {
            archive::import(&home, file, name.as_deref(), ctx.json)
        }
        ProfileCommand::Use { name } => {
            let name = ProfileName::parse(name)?;
            home.use_profile(&name)?;
            eprintln!("profile '{name}' is now used without -p");
            Ok(0)
        }
        ProfileCommand::Show { name } => show(&home, ctx, name.as_deref()),
        ProfileCommand::Rename { old, new } => {
            let renamed = home.rename_profile(&ProfileName::parse(old)?, new)?;
            eprintln!("renamed profile '{old}' to '{}'", renamed.name());
            Ok(0)
        }
        ProfileCommand::Delete { name, yes } => delete(&home, name, *yes),
    }
}

#[derive(Serialize)]
struct Row {
    name: String,
    active: bool,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

/// The default launcher of a new profile; a refusal is a note, not a
/// failure (spec: configuration-and-profiles, Profile creation options).
fn default_alias(name: &ProfileName, root: &std::path::Path, ctx: &Context<'_>) {
    match alias::run(name, name.as_str(), root, ctx.launch) {
        Ok(path) => {
            eprintln!("alias launcher: {} (skip with --no-alias)", path.display());
            note_path(&path, ctx);
        }
        Err(err) => eprintln!("no alias launcher: {}", err.message()),
    }
}

fn note_path(launcher: &std::path::Path, ctx: &Context<'_>) {
    if let Some(dir) = launcher.parent()
        && !alias::dir_on_path(dir, ctx.launch)
    {
        eprintln!("note: {} is not on PATH", dir.display());
    }
}

fn description(home: &Home, name: &ProfileName) -> Option<String> {
    let path = home
        .profile_path(name)
        .join(pmt_config::profiles::DESCRIPTION_FILE);
    std::fs::read_to_string(path)
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}

fn list(home: &Home, json: bool) -> Result<u8, Error> {
    #[derive(Serialize)]
    struct Listing {
        active: String,
        profiles: Vec<Row>,
    }
    let active = home.active_profile()?;
    let profiles: Vec<Row> = home
        .profiles()?
        .into_iter()
        .map(|name| Row {
            active: name == active,
            path: home.profile_path(&name).display().to_string(),
            description: description(home, &name),
            name: name.to_string(),
        })
        .collect();
    if json {
        let active = active.to_string();
        return document("profile list", &Listing { active, profiles });
    }
    let text: String = profiles
        .iter()
        .map(|row| {
            let about = row
                .description
                .as_deref()
                .map(|d| format!("  {d}"))
                .unwrap_or_default();
            [
                if row.active { "* " } else { "  " },
                &row.name,
                &about,
                "\n",
            ]
            .concat()
        })
        .collect();
    print_raw(&text)
}

fn show(home: &Home, ctx: &Context<'_>, name: Option<&str>) -> Result<u8, Error> {
    #[derive(Serialize)]
    struct Shown {
        #[serde(flatten)]
        row: Row,
        secrets_backend: &'static str,
        secrets: usize,
        in_use_by_pid: Option<u32>,
    }
    let name = match name {
        Some(name) => ProfileName::parse(name)?,
        None => home.select_profile(ctx.profile)?,
    };
    let scope = ProfileScope::open(
        home.existing_profile(&name)?,
        &Overrides::default(),
        Launch::Other,
    )?;
    let shown = Shown {
        row: Row {
            active: home.active_profile()? == name,
            path: scope.dir().display().to_string(),
            description: description(home, &name),
            name: name.to_string(),
        },
        secrets_backend: match scope.secrets().backend() {
            pmt_config::schema::SecretsBackend::File => "file",
            pmt_config::schema::SecretsBackend::Keychain => "keychain",
        },
        secrets: scope.secrets().store().names()?.len(),
        in_use_by_pid: user_of(home.root(), &name)?.map(|(_, pid)| pid),
    };
    if ctx.json {
        return document("profile show", &shown);
    }
    let in_use = shown
        .in_use_by_pid
        .map_or_else(|| "no".to_owned(), |pid| format!("yes (pid {pid})"));
    let about = shown
        .row
        .description
        .as_deref()
        .map(|d| format!("description: {d}\n"))
        .unwrap_or_default();
    print_raw(&format!(
        "name: {}\n{about}path: {}\nactive: {}\nsecrets: {} ({} backend)\nin use: {in_use}\n",
        shown.row.name,
        shown.row.path,
        if shown.row.active { "yes" } else { "no" },
        shown.secrets,
        shown.secrets_backend,
    ))
}

fn delete(home: &Home, name: &str, yes: bool) -> Result<u8, Error> {
    let name = ProfileName::parse(name)?;
    // Refusals (default, unknown, in use) come before the question.
    home.existing_profile(&name)?;
    if !yes {
        let what = format!(
            "this deletes profile '{name}' and everything in {}",
            home.profile_path(&name).display()
        );
        match prompt::confirm_typed(&what, name.as_str())? {
            None => {
                eprintln!("error: deleting profile '{name}' needs confirmation; rerun with --yes");
                return Ok(pmt_core::exit::APPROVAL_REQUIRED);
            }
            Some(false) => {
                eprintln!("not deleted");
                return Ok(1);
            }
            Some(true) => {}
        }
    }
    home.delete_profile(&name)?;
    eprintln!("deleted profile '{name}'");
    Ok(0)
}

/// Copies `source`'s built-in memory (it lives in `state.db`) into the new
/// `target`: every entry keeps its id, store, tier and pin; changes still
/// waiting for approval stay behind (spec: configuration-and-profiles,
/// Profile lifecycle commands).
fn clone_memory(
    home: &Home,
    source: &ProfileName,
    target: &pmt_config::ProfileDir,
) -> Result<(), Error> {
    let open = |profile: pmt_config::ProfileDir| -> Result<pmt_store::Store, Error> {
        let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other)?;
        pmt_store::Store::open(&scope)
    };
    let entries = open(home.existing_profile(source)?)?.memory_entries(None)?;
    if entries.is_empty() {
        return Ok(());
    }
    open(target.clone())?.memory_batch(None, |batch| {
        batch.author = Some(pmt_store::Author {
            origin: pmt_store::MemoryOrigin::Import,
            session: None,
        });
        batch.entries.clone_from(&entries);
        Ok(())
    })
}
