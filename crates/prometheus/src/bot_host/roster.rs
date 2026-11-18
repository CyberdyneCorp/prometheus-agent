//! The teammates a bot can reach and how a `message_agent` target names
//! one (spec: bot-mode, `message_agent` tool; Roster presentation, hiding
//! and sections; desktop-app, Agent-to-agent messages from bot chats).
//!
//! A target resolves in order: an exact profile name, the `@`-tag of a
//! bot title (`Research Buddy` -> `@research-buddy` or `@researchbuddy`),
//! `@prometheus` for the primary bot, `<peer>/<agent>` or `<peer>` for a
//! peer gateway, `<handle>@<connection>` for an agent on another desktop
//! connection. A friendly title never hides a profile name: the profile
//! match comes first.

use std::path::Path;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::error::Error;
use pmt_rpc::contracts::BotMeta;

use super::meta;

/// The primary bot's tag.
pub const PRIMARY_TAG: &str = "prometheus";

/// A bot of this home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Teammate {
    /// Its profile.
    pub profile: String,
    /// Its title (the profile name when unset).
    pub title: String,
    /// What it does.
    pub description: Option<String>,
    /// `bot_mode.enabled`.
    pub enabled: bool,
    /// Its metadata.
    pub meta: BotMeta,
}

impl Teammate {
    /// `@<profile>`.
    #[must_use]
    pub fn handle(&self) -> String {
        format!("@{}", self.profile)
    }

    /// "🤖 <title> (@<profile>)": how its messages are attributed.
    #[must_use]
    pub fn attribution(&self) -> String {
        format!("🤖 {} ({})", self.title, self.handle())
    }
}

/// Where a target lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// A profile of this home.
    Local(Box<Teammate>),
    /// An agent of a registered peer gateway (`default` for a bare peer).
    Peer {
        /// The peer.
        peer: String,
        /// The agent on it.
        agent: String,
    },
    /// An agent on another desktop connection (`<handle>@<connection>`).
    Connection(String),
}

/// Every profile of `home` as a teammate (a profile that cannot be read
/// is left out).
///
/// # Errors
///
/// `home.io` when the profiles cannot be listed.
pub fn local(home: &Path) -> Result<Vec<Teammate>, Error> {
    let layout = Home::open(home)?;
    Ok(layout
        .profiles()?
        .into_iter()
        .filter_map(|name| teammate(&layout, &name))
        .collect())
}

fn teammate(layout: &Home, name: &ProfileName) -> Option<Teammate> {
    let dir = layout.existing_profile(name).ok()?;
    let scope = ProfileScope::open(dir, &Overrides::default(), Launch::Other).ok()?;
    let store = pmt_store::Store::open(&scope).ok()?;
    Some(of(&scope, &store))
}

/// The teammate `scope` is, with its metadata from `store`.
#[must_use]
pub fn of(scope: &ProfileScope, store: &pmt_store::Store) -> Teammate {
    let meta = meta::read(store).unwrap_or_default();
    let profile = scope.name().to_string();
    let description = meta
        .description
        .clone()
        .or_else(|| pmt_config::profiles::description(scope.dir()));
    Teammate {
        title: meta::title_of(&profile, &meta),
        enabled: scope.config().bot_mode.enabled,
        description,
        profile,
        meta,
    }
}

/// The two tags of a title: words joined by `-`, and run together.
#[must_use]
pub fn tags(title: &str) -> [String; 2] {
    let words: Vec<String> = title
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    [words.join("-"), words.concat()]
}

/// Why a target did not resolve: the text the sending agent sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved(pub String);

/// Resolves `target` against `teammates` and `peers` (the sender's
/// registered peer gateways).
///
/// # Errors
///
/// [`Unresolved`] naming the candidates of an ambiguous title, or the
/// roster (closest names first) for an unknown target.
pub fn resolve(
    target: &str,
    teammates: &[Teammate],
    peers: &[String],
) -> Result<Resolved, Unresolved> {
    let target = target.trim();
    let bare = target.strip_prefix('@').unwrap_or(target);
    if let Some(found) = teammates.iter().find(|t| t.profile == bare) {
        return Ok(Resolved::Local(Box::new(found.clone())));
    }
    let lower = bare.to_lowercase();
    let titled: Vec<&Teammate> = teammates
        .iter()
        .filter(|t| tags(&t.title).contains(&lower))
        .collect();
    match titled.as_slice() {
        [one] => return Ok(Resolved::Local(Box::new((*one).clone()))),
        [_, _, ..] => {
            let names: Vec<String> = titled.iter().map(|t| t.handle()).collect();
            return Err(Unresolved(format!(
                "'{target}' names several bots ({}); use the profile name",
                names.join(", ")
            )));
        }
        [] => {}
    }
    if lower == PRIMARY_TAG
        && let Some(primary) = teammates.iter().find(|t| t.profile == "default")
    {
        return Ok(Resolved::Local(Box::new(primary.clone())));
    }
    if let Some(peer) = by_peer(bare, peers) {
        return Ok(peer);
    }
    if let Some((handle, connection)) = bare.rsplit_once('@')
        && !handle.is_empty()
        && !connection.is_empty()
        && !connection.contains('.')
    {
        return Ok(Resolved::Connection(format!(
            "@{}@{connection}",
            handle.trim_start_matches('@')
        )));
    }
    Err(unknown(target, teammates, peers))
}

fn by_peer(bare: &str, peers: &[String]) -> Option<Resolved> {
    let (peer, agent) = bare.split_once('/').unwrap_or((bare, "default"));
    (peers.iter().any(|p| p == peer) && !agent.is_empty()).then(|| Resolved::Peer {
        peer: peer.to_owned(),
        agent: agent.to_owned(),
    })
}

/// The refusal of an unknown target: the roster, closest names first.
fn unknown(target: &str, teammates: &[Teammate], peers: &[String]) -> Unresolved {
    let mut names: Vec<String> = teammates.iter().map(|t| t.profile.clone()).collect();
    names.extend(peers.iter().map(|p| format!("{p}/<agent>")));
    let wanted = target.trim_start_matches('@').to_lowercase();
    names.sort_by_key(|name| (distance(&wanted, &name.to_lowercase()), name.clone()));
    Unresolved(format!(
        "unknown teammate '{target}'; closest names on the roster: {}",
        names.join(", ")
    ))
}

/// Levenshtein distance.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitute = previous + usize::from(ca != *cb);
            previous = row[j + 1];
            row[j + 1] = substitute.min(row[j] + 1).min(previous + 1);
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mate(profile: &str, title: &str) -> Teammate {
        Teammate {
            profile: profile.into(),
            title: title.into(),
            description: None,
            enabled: true,
            meta: BotMeta::default(),
        }
    }

    fn local_profile(resolved: Result<Resolved, Unresolved>) -> String {
        match resolved {
            Ok(Resolved::Local(t)) => t.profile,
            other => panic!("not local: {other:?}"),
        }
    }

    /// Scenario: Friendly name cannot hijack a profile name.
    #[test]
    fn a_profile_name_wins_over_a_title() {
        let roster = [mate("scribe", "scribe"), mate("writer", "Scribe")];
        assert_eq!(local_profile(resolve("scribe", &roster, &[])), "scribe");
        assert_eq!(local_profile(resolve("@writer", &roster, &[])), "writer");
    }

    #[test]
    fn titles_resolve_by_their_tags_and_shared_titles_are_refused() {
        let roster = [
            mate("research", "Research Buddy"),
            mate("default", "default"),
            mate("a", "Twin"),
            mate("b", "Twin"),
        ];
        assert_eq!(
            local_profile(resolve("@research-buddy", &roster, &[])),
            "research"
        );
        assert_eq!(
            local_profile(resolve("@researchbuddy", &roster, &[])),
            "research"
        );
        assert_eq!(
            local_profile(resolve("@prometheus", &roster, &[])),
            "default"
        );
        let shared = resolve("@twin", &roster, &[]).unwrap_err().0;
        assert!(shared.contains("@a") && shared.contains("@b"), "{shared}");
    }

    #[test]
    fn peers_and_connections_resolve_by_their_forms() {
        let peers = ["homelab".to_owned()];
        assert_eq!(
            resolve("homelab/research", &[], &peers),
            Ok(Resolved::Peer {
                peer: "homelab".into(),
                agent: "research".into()
            })
        );
        assert_eq!(
            resolve("homelab", &[], &peers),
            Ok(Resolved::Peer {
                peer: "homelab".into(),
                agent: "default".into()
            })
        );
        assert_eq!(
            resolve("@writer@Laptop", &[], &peers),
            Ok(Resolved::Connection("@writer@Laptop".into()))
        );
    }

    /// Scenario: Unknown teammate.
    #[test]
    fn an_unknown_target_lists_the_closest_names_first() {
        let roster = [
            mate("writer", "writer"),
            mate("researcher", "researcher"),
            mate("default", "default"),
        ];
        let refused = resolve("reseacher", &roster, &[]).unwrap_err().0;
        assert!(
            refused.starts_with(
                "unknown teammate 'reseacher'; closest names on the roster: researcher, "
            ),
            "{refused}"
        );
        assert!(resolve("someone@example.com", &roster, &[]).is_err());
    }
}
