//! `prometheus profile` (spec: configuration-and-profiles, Profile
//! lifecycle commands, Safe profile deletion, Profile selection at process
//! start).

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
            clone_secrets,
        } => {
            let source = clone_from.as_deref().map(ProfileName::parse).transpose()?;
            let clone = source.as_ref().map(|source| CloneFrom {
                source,
                secrets: *clone_secrets,
            });
            let profile = home.create_profile(name, clone)?;
            eprintln!(
                "created profile '{}' at {}",
                profile.name(),
                profile.path().display()
            );
            Ok(0)
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
            name: name.to_string(),
        })
        .collect();
    if json {
        let active = active.to_string();
        return document("profile list", &Listing { active, profiles });
    }
    let text: String = profiles
        .iter()
        .map(|row| [if row.active { "* " } else { "  " }, &row.name, "\n"].concat())
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
    print_raw(&format!(
        "name: {}\npath: {}\nactive: {}\nsecrets: {} ({} backend)\nin use: {in_use}\n",
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
                return Ok(prompt::NEEDS_CONFIRMATION);
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
