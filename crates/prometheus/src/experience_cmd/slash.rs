//! `/domain [NAME]` (spec: experience-library, Domain resolution for a
//! job): shows or sets the session's domain tag. An evolution job of the
//! session files its lesson under it; the session's own lessons layer was
//! decided when it started and does not change (prompt-cache invariant).

use pmt_core::error::Error;
use pmt_evolve::experience::domain;

use crate::chat::commands::Ctx;

/// Runs `/domain`.
///
/// # Errors
///
/// `experience.invalid_domain` (a name too long), store errors.
pub fn domain(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let store = &ctx.factory.store;
    let id = ctx.live.id;
    let name = args.trim();
    if name.is_empty() {
        return Ok(match store.session_domain(id)? {
            Some(current) => format!("domain: {current}"),
            None => "domain: none (an evolution job of this session is classified)".to_owned(),
        });
    }
    let name = domain::parse(name)?;
    store.set_session_domain(id, &name)?;
    Ok(format!(
        "domain set to {name}; evolution jobs of this session file their lessons there (this session's prompt is unchanged)"
    ))
}
