//! `prometheus profile describe` (spec: configuration-and-profiles, Profile
//! descriptions): a description of at most 500 characters, typed with
//! `--text` or written by the `profile` auxiliary route from the profile's
//! name, identity file, model and installed skills. An existing
//! description is never replaced without `--overwrite`.

use std::path::PathBuf;
use std::sync::Arc;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_providers::auxiliary::AuxTask;
use pmt_store::{NewSession, Source, Store};

use super::args::DescribeArgs;
use crate::chat::session::model::{Models, RouteFlags};
use crate::context::Context;

/// The longest description kept.
pub const MAX_CHARS: usize = 500;

/// What `--auto` asks the model.
const SYSTEM: &str = "You write a one-paragraph description (at most 400 characters) of an AI \
agent profile for a list of profiles, from its name, identity file, model and skills: what it is \
for and what it is good at. Answer with the description only, no preamble, no quotes.";

/// The longest identity excerpt sent to the model.
const IDENTITY_CHARS: usize = 4_000;

/// Runs `profile describe`.
///
/// # Errors
///
/// Usage errors, a description over 500 characters, model errors.
pub fn run(home: &Home, ctx: &Context<'_>, args: &DescribeArgs) -> Result<u8, Error> {
    if args.text.is_none() && !args.auto {
        return Err(usage("profile describe needs --text <TEXT> or --auto"));
    }
    if let Some(text) = &args.text
        && text.trim().chars().count() > MAX_CHARS
    {
        return Err(Error::new(
            &codes::CLI_USAGE,
            format!("a description holds at most {MAX_CHARS} characters"),
        ));
    }
    let names = if args.all {
        home.profiles()?
            .into_iter()
            .filter(|n| super::description(home, n).is_none())
            .collect()
    } else {
        let name = match &args.name {
            Some(name) => ProfileName::parse(name)?,
            None => home.select_profile(ctx.profile)?,
        };
        home.existing_profile(&name)?;
        vec![name]
    };
    for name in names {
        if super::description(home, &name).is_some() && !args.overwrite {
            println!("description exists; use --overwrite");
            continue;
        }
        let text = match &args.text {
            Some(text) => text.trim().to_owned(),
            None => generate(home, ctx, &name)?,
        };
        write(home, &name, &text)?;
        eprintln!("described profile '{name}'");
    }
    Ok(0)
}

/// Stores `text` as `name`'s description.
///
/// # Errors
///
/// `home.io`.
pub fn write(home: &Home, name: &ProfileName, text: &str) -> Result<(), Error> {
    let path = home
        .profile_path(name)
        .join(pmt_config::profiles::DESCRIPTION_FILE);
    pmt_core::private_fs::write_atomic(&path, text.as_bytes())
        .map_err(|e| Error::io(&codes::HOME_IO, &path, &e))
}

/// One description from the auxiliary model, cut to [`MAX_CHARS`].
fn generate(home: &Home, ctx: &Context<'_>, name: &ProfileName) -> Result<String, Error> {
    let scope = ProfileScope::open(
        home.existing_profile(name)?,
        &Overrides::default(),
        Launch::Other,
    )?;
    let store = Arc::new(Store::open(&scope)?);
    let session = store.create_session(&NewSession {
        source_tag: Some("profile-describe".to_owned()),
        title: Some(format!("describe profile {name}")),
        hidden: true,
        ..NewSession::new(Source::Cli)
    })?;
    let models = Models::open(
        &scope,
        &store,
        ctx.launch.user_home().map(PathBuf::as_path),
        &RouteFlags::default(),
        None,
    )?;
    let reply = crate::model_cmd::block_on(models.ask_auxiliary(
        AuxTask::Profile,
        session.id,
        SYSTEM,
        &facts(&scope),
        600,
    ))??;
    let text = reply.unwrap_or_default();
    let text: String = text.trim().chars().take(MAX_CHARS).collect();
    if text.is_empty() {
        return Err(Error::new(
            &codes::PROVIDER_CALL_FAILED,
            "the model gave no description",
        ));
    }
    Ok(text)
}

/// The profile facts the model describes.
fn facts(scope: &ProfileScope) -> String {
    let identity: String = std::fs::read_to_string(scope.path("SOUL.md"))
        .unwrap_or_default()
        .chars()
        .take(IDENTITY_CHARS)
        .collect();
    let skills = installed_skills(scope);
    format!(
        "Profile name: {}\nModel: {}\nSkills: {}\n\nIdentity file:\n{}",
        scope.name(),
        scope
            .config()
            .model
            .default
            .as_deref()
            .unwrap_or("(default)"),
        if skills.is_empty() {
            "(none)".to_owned()
        } else {
            skills.join(", ")
        },
        if identity.trim().is_empty() {
            "(none)"
        } else {
            identity.trim()
        }
    )
}

/// The names of the profile's own skills (`<profile>/skills/<name>/`).
fn installed_skills(scope: &ProfileScope) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(scope.path("skills"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("SKILL.md").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn usage(message: &str) -> Error {
    Error::new(&codes::CLI_USAGE, message)
}
