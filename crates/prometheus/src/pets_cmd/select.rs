//! `pets select`, `pets off` and `pets remove`.

use std::io::{BufRead as _, Write as _};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};

use crate::pets_host::{current, patch, pets};

pub(super) fn select(scope: &ProfileScope, slug: Option<&str>) -> Result<u8, Error> {
    let slug = match slug {
        Some(slug) => slug.to_owned(),
        None => match pick(scope)? {
            Some(slug) => slug,
            None => return Ok(0),
        },
    };
    let line = crate::pets_host::slash::run(scope, &crate::pets_host::slash::Pet::Adopt(slug))?;
    crate::output::print(&line)
}

/// The picker: the installed pets numbered, answered by number or slug
/// (`None`: nothing chosen).
fn pick(scope: &ProfileScope) -> Result<Option<String>, Error> {
    let installed = pets(scope).installed();
    if installed.is_empty() {
        return Err(Error::new(&codes::PET_NOT_FOUND, "no pet is installed")
            .with_hint("install one with 'prometheus pets install <slug>'"));
    }
    if !crate::prompt::stdin_is_tty() {
        return Err(Error::new(
            &codes::CLI_USAGE,
            "name the pet: prometheus pets select <slug> (the picker needs a terminal)",
        ));
    }
    let mut err = std::io::stderr();
    for (n, pet) in installed.iter().enumerate() {
        let _ = writeln!(err, "{:>3}) {}  {}", n + 1, pet.meta.slug, pet.meta.name);
    }
    let _ = write!(err, "pet (number or slug; Enter to cancel): ");
    let _ = err.flush();
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| crate::prompt::read_error(&e))?;
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(None);
    }
    let chosen = answer
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|i| installed.get(i))
        .map_or_else(|| answer.to_owned(), |p| p.meta.slug.clone());
    Ok(Some(chosen))
}

pub(super) fn off(scope: &ProfileScope) -> Result<u8, Error> {
    let line = crate::pets_host::slash::run(scope, &crate::pets_host::slash::Pet::Off)?;
    crate::output::print(&line)
}

pub(super) fn remove(scope: &ProfileScope, slug: &str) -> Result<u8, Error> {
    let config = current(scope);
    let active = pets(scope).active(&config.slug).map(|p| p.meta.slug);
    if !pets(scope).remove(slug)? {
        return Err(pmt_pets::store::not_found(slug));
    }
    // The active pet is gone: the pet is off until another is selected.
    if active.as_deref() == Some(slug) {
        patch(
            scope,
            &[
                ("pets.enabled", "false".to_owned()),
                ("pets.slug", String::new()),
            ],
        )?;
    }
    crate::output::print(&format!("removed '{slug}'"))
}
