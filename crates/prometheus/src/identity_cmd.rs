//! `prometheus identity list|unlink` (spec: messaging-gateway,
//! Cross-platform identity linking): the messaging accounts linked into
//! one person with `/link`, and taking one out.

use std::fmt::Write as _;

use clap::Subcommand;
use pmt_core::error::{Error, codes};
use serde_json::json;

use crate::context::Context;
use crate::output::{json, print_raw};

/// `prometheus identity` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum IdentityCommand {
    /// Every linked account, by person.
    List,
    /// Take an account (`<platform>:<sender_id>`), or every account of a
    /// person on a platform (`<person_id> <platform>`), out of its link.
    Unlink {
        /// `<platform>:<sender_id>`, or a person id.
        account: String,
        /// The platform, when `account` is a person id.
        platform: Option<String>,
    },
}

/// Runs one `identity` subcommand.
///
/// # Errors
///
/// `cli.usage` (2) for an account that is not linked; store errors.
pub fn run(command: &IdentityCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let store = pmt_store::Store::open(&scope)?;
    match command {
        IdentityCommand::List => {
            let links = store.identity_links()?;
            if ctx.json {
                let rows: Vec<_> = links
                    .iter()
                    .map(|l| {
                        json!({"person_id": l.person_id, "platform": l.platform,
                            "sender_id": l.sender_id, "linked_at": l.linked_at.to_string()})
                    })
                    .collect();
                return json(&json!({ "links": rows }));
            }
            let mut out = String::new();
            for l in &links {
                let _ = writeln!(
                    out,
                    "{}  {}:{}  {}",
                    l.person_id, l.platform, l.sender_id, l.linked_at
                );
            }
            if out.is_empty() {
                out.push_str("no linked accounts\n");
            }
            print_raw(&out)
        }
        IdentityCommand::Unlink { account, platform } => {
            let (person, platform, sender) = match (account.split_once(':'), platform) {
                (Some((platform, sender)), None) => {
                    let person = store.person_of(platform, sender)?.ok_or_else(|| {
                        Error::new(&codes::CLI_USAGE, format!("{account} is not linked"))
                    })?;
                    (person, platform.to_owned(), Some(sender.to_owned()))
                }
                (_, Some(platform)) => (account.clone(), platform.clone(), None),
                (None, None) => {
                    return Err(Error::new(
                        &codes::CLI_USAGE,
                        "name an account as <platform>:<sender_id>, or a person id and a platform",
                    ));
                }
            };
            let removed = store.unlink_identity(&person, &platform, sender.as_deref())?;
            if removed == 0 {
                return Err(Error::new(
                    &codes::CLI_USAGE,
                    format!("{person} has no linked account on {platform}"),
                ));
            }
            print_raw(&format!(
                "unlinked {removed} account(s) on {platform} from {person}\n"
            ))
        }
    }
}
