//! `/pet` (spec: pets, Pet slash command): toggles the pet (adopting the
//! first installed one when none is selected), `/pet <slug>` adopts an
//! installed pet, `/pet scale <factor>` resizes it everywhere and
//! `/pet off` hides it. Each is a config patch made by the client itself:
//! no model call, no turn, the session untouched. `/pet list` is the
//! surface's picker ([`Pet::List`]).

use std::fmt::Write as _;

use pmt_config::ProfileScope;
use pmt_core::error::Error;

use super::{current, patch, pets};

/// What `/pet <args>` asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Pet {
    /// Toggle.
    Toggle,
    /// Open the picker.
    List,
    /// Resize.
    Scale(String),
    /// Hide.
    Off,
    /// Adopt this installed pet.
    Adopt(String),
}

impl Pet {
    /// Parses the arguments of `/pet`.
    #[must_use]
    pub fn parse(args: &str) -> Self {
        let mut words = args.split_whitespace();
        match (words.next(), words.next()) {
            (None, _) => Self::Toggle,
            (Some("list"), _) => Self::List,
            (Some("off"), _) => Self::Off,
            (Some("scale"), Some(factor)) => Self::Scale(factor.to_owned()),
            (Some("scale"), None) => Self::Scale(String::new()),
            (Some(slug), _) => Self::Adopt(slug.to_owned()),
        }
    }
}

/// `/pet scale` and `prometheus pets scale`: sets `pets.scale`, clamped;
/// the line to show.
///
/// # Errors
///
/// `config.invalid` for a factor that is not a number; config write
/// errors.
pub fn set_scale(scope: &ProfileScope, factor: &str) -> Result<String, Error> {
    let wanted: f64 = factor.trim().parse().map_err(|_| {
        Error::new(
            &pmt_core::error::codes::CONFIG_INVALID,
            format!("'{factor}' is not a scale factor (a number from 0.1 to 3.0)"),
        )
    })?;
    let (scale, clamped) = pmt_pets::size::clamp_scale(wanted);
    let text = format!("{scale:.2}").trim_end_matches('0').to_owned();
    let text = if text.ends_with('.') {
        format!("{text}0")
    } else {
        text
    };
    patch(scope, &[("pets.scale", text.clone())])?;
    Ok(if clamped {
        format!("scale clamped to {text}")
    } else {
        format!("pet scale set to {text}")
    })
}

/// Runs `/pet` (all but `list`); the line to show.
///
/// # Errors
///
/// `pet.not_found` for a slug that is not installed; config errors.
pub fn run(scope: &ProfileScope, pet: &Pet) -> Result<String, Error> {
    let config = current(scope);
    match pet {
        Pet::Toggle if config.enabled => off(scope),
        Pet::Toggle => {
            let Some(first) = pets(scope).active(&config.slug) else {
                return Ok("no pet is installed; install one with 'prometheus pets install <slug>' (see /pet list)".to_owned());
            };
            adopt(scope, &first.meta.slug)
        }
        Pet::Off => off(scope),
        Pet::Scale(factor) => set_scale(scope, factor),
        Pet::Adopt(slug) => {
            if pets(scope).get(slug).is_none() {
                return Err(pmt_pets::store::not_found(slug));
            }
            adopt(scope, slug)
        }
        Pet::List => Ok(list(scope)),
    }
}

fn off(scope: &ProfileScope) -> Result<String, Error> {
    patch(scope, &[("pets.enabled", "false".to_owned())])?;
    Ok("pet hidden".to_owned())
}

fn adopt(scope: &ProfileScope, slug: &str) -> Result<String, Error> {
    patch(
        scope,
        &[
            ("pets.slug", slug.to_owned()),
            ("pets.enabled", "true".to_owned()),
        ],
    )?;
    Ok(format!("pet '{slug}' is out"))
}

/// The installed pets as lines (`*` marks the active one), for a line
/// interface's `/pet list`.
#[must_use]
pub fn list(scope: &ProfileScope) -> String {
    let config = current(scope);
    let installed = pets(scope).installed();
    if installed.is_empty() {
        return "no pet is installed; browse the gallery with 'prometheus pets list' and install one with 'prometheus pets install <slug>'".to_owned();
    }
    let active = pets(scope)
        .active(&config.slug)
        .map(|p| p.meta.slug)
        .unwrap_or_default();
    let mut text = String::from("installed pets (adopt one with /pet <slug>):");
    for pet in installed {
        let mark = if config.enabled && pet.meta.slug == active {
            "*"
        } else {
            " "
        };
        let _ = write!(text, "\n {mark} {}  {}", pet.meta.slug, pet.meta.name);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::Pet;

    #[test]
    fn arguments_parse() {
        assert_eq!(Pet::parse(""), Pet::Toggle);
        assert_eq!(Pet::parse(" list "), Pet::List);
        assert_eq!(Pet::parse("off"), Pet::Off);
        assert_eq!(Pet::parse("scale 0.5"), Pet::Scale("0.5".into()));
        assert_eq!(Pet::parse("mochi"), Pet::Adopt("mochi".into()));
    }
}
