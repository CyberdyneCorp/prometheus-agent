//! The bot protocol of canonical Bot Chats and the notices a turn carries
//! (spec: bot-mode, Bot protocol in canonical Bot Chats; Mentions from any
//! chat).
//!
//! The protocol section (the bot's own handle and its teammates) is put
//! into a Bot Chat's system prompt once, when the session's prompt is
//! built, and the roster it listed is remembered. A later roster change
//! reaches the chat as one `roster.update` notice stored with the next
//! turn's user message, never by rebuilding the prompt. An `@handle` in
//! any chat that names a bot of the live roster gets a notice naming
//! that bot; the text itself is never forwarded to it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use pmt_agent::turn::TurnNotes;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_rpc::contracts::RosterAgent;
use pmt_store::Store;
use serde::{Deserialize, Serialize};

use super::roster::{self, Resolved, Teammate};

/// The store preference remembering the roster a Bot Chat's prompt (or
/// its last notice) listed.
fn seen_pref(session: &str) -> String {
    format!("bot.roster_seen.{session}")
}

/// One roster line: handle -> "title — role (machine: m)".
type Lines = BTreeMap<String, String>;

/// The store preference holding the remote part of the roster
/// `bot.roster.list` uses: the daemon writes it whenever its cached peer
/// listings or the connections a desktop reported change.
const REMOTE_PREF: &str = "bot.roster.remote";

/// The remote teammates of a profile, as the daemon last saw them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Remote {
    /// Each registered peer's agents (`@<peer>/<agent>` lines).
    pub peers: BTreeMap<String, Lines>,
    /// Agents of the desktop's other connections
    /// (`@<agent>@<connection>` lines).
    pub connections: Lines,
}

impl Remote {
    /// Adds `agent` of registered peer `peer` running on `machine`.
    pub fn peer(&mut self, peer: &str, agent: &RosterAgent, machine: &str) {
        self.peers.entry(peer.to_owned()).or_default().insert(
            format!("@{peer}/{}", agent.name),
            remote_line(agent, machine),
        );
    }

    /// Adds `agent` of the desktop connection `connection`.
    pub fn connection(&mut self, connection: &str, agent: &RosterAgent) {
        self.connections.insert(
            format!("@{}@{connection}", agent.name),
            remote_line(agent, connection),
        );
    }
}

fn remote_line(agent: &RosterAgent, machine: &str) -> String {
    let title = agent
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| agent.name.clone());
    about(&title, agent.description.as_deref(), machine)
}

/// Saves `remote` as `store`'s remote roster when it changed; whether it
/// did. Bot Chats see the change as a `roster.update` notice on their
/// next turn.
///
/// # Errors
///
/// Store errors.
pub fn remember_remote(store: &Store, remote: &Remote) -> Result<bool, Error> {
    if read_remote(store) == *remote {
        return Ok(false);
    }
    let text = serde_json::to_string(remote)
        .map_err(|e| Error::new(&codes::RPC_INTERNAL, format!("bot roster: {e}")))?;
    store.set_pref(REMOTE_PREF, &text)?;
    Ok(true)
}

fn read_remote(store: &Store) -> Remote {
    store
        .pref(REMOTE_PREF)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// The teammates `me` sees: the other enabled bots of the home, the
/// agents of each registered peer gateway and of the desktop's other
/// connections (the rows `bot.roster.list` lists). A peer not listed yet
/// shows as one line naming its address form.
fn lines(scope: &ProfileScope, store: &Store, me: &str) -> Lines {
    let mut lines = Lines::new();
    for mate in roster::local(scope.home()).unwrap_or_default() {
        if mate.profile != me && mate.enabled {
            lines.insert(mate.handle(), line(&mate));
        }
    }
    let mut remote = read_remote(store);
    for peer in scope.config().peers.keys() {
        match remote
            .peers
            .remove(peer)
            .filter(|agents| !agents.is_empty())
        {
            Some(agents) => lines.extend(agents),
            None => {
                lines.insert(
                    format!("{peer}/<agent>"),
                    format!("the agents of peer gateway {peer} (machine: {peer})"),
                );
            }
        }
    }
    lines.extend(remote.connections);
    lines
}

fn about(title: &str, role: Option<&str>, machine: &str) -> String {
    let role = role.map(|d| format!(" — {d}")).unwrap_or_default();
    format!("{title}{role} (machine: {machine})")
}

fn line(mate: &Teammate) -> String {
    about(&mate.title, mate.description.as_deref(), "local")
}

/// The protocol section of `scope`'s canonical Bot Chat `session`, and
/// the roster it lists remembered for later notices.
#[must_use]
pub fn section(scope: &ProfileScope, store: &Store, session: &str) -> String {
    let me = roster::of(scope, store);
    let listed = lines(scope, store, &me.profile);
    remember(store, session, &listed);
    let mut text = format!(
        "# Bot protocol\nYou are the bot {} (\"{}\") in Bot Mode. Your teammates:\n",
        me.handle(),
        me.title
    );
    if listed.is_empty() {
        text.push_str("- (none yet)\n");
    }
    for (handle, about) in &listed {
        let _ = writeln!(text, "- {handle}: {about}");
    }
    text.push_str(
        "Message a teammate with the `message_agent` tool (target: its handle without @, a \
         title tag, <peer>/<agent>, or <handle>@<connection>). It returns at once; the reply, \
         or why it failed as [reason: <code>], arrives later in this chat. A message from a \
         teammate starts with \"Message from 🤖 <title> (@<handle>)\": answer it in your reply \
         (it is sent back), or answer [SILENT] to send nothing. Roster changes arrive as \
         roster.update notices.",
    );
    text
}

fn remember(store: &Store, session: &str, lines: &Lines) {
    if let Ok(text) = serde_json::to_string(lines)
        && let Err(err) = store.set_pref(&seen_pref(session), &text)
    {
        tracing::warn!("bot roster not remembered: {}", err.message());
    }
}

/// The `roster.update` notice for `session` when its roster changed since
/// it was last listed (and remembers the new one).
fn roster_update(scope: &ProfileScope, store: &Store, session: &str) -> Option<String> {
    let seen: Lines = store
        .pref(&seen_pref(session))
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())?;
    let me = scope.name().to_string();
    let now = lines(scope, store, &me);
    if now == seen {
        return None;
    }
    let mut changes = Vec::new();
    for (handle, about) in &now {
        match seen.get(handle) {
            None => changes.push(format!("added {handle}: {about}")),
            Some(was) if was != about => changes.push(format!("changed {handle}: {about}")),
            Some(_) => {}
        }
    }
    for handle in seen.keys().filter(|h| !now.contains_key(*h)) {
        changes.push(format!("removed {handle}"));
    }
    remember(store, session, &now);
    Some(format!("[roster.update] {}", changes.join("; ")))
}

/// The `@words` of `text` that may name a bot: not part of an e-mail
/// address, outside nothing else (a mention is a word starting with `@`).
fn mentions(text: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let bytes = text.as_bytes();
    for (at, _) in text.match_indices('@') {
        let before = at.checked_sub(1).map(|i| bytes[i]);
        if before.is_some_and(|b| b.is_ascii_alphanumeric() || b"._%+-@".contains(&b)) {
            continue;
        }
        let rest = &text[at + 1..];
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || "-_/".contains(c)))
            .unwrap_or(rest.len());
        let word = rest[..end].trim_end_matches(['-', '/']);
        let email_like = rest[end..].starts_with('.')
            && rest[end + 1..].starts_with(|c: char| c.is_alphanumeric());
        if !word.is_empty() && !email_like && !found.contains(&word) {
            found.push(word);
        }
    }
    found
}

/// The notice naming each bot `text` mentions.
fn mention_notice(scope: &ProfileScope, text: &str) -> Option<String> {
    let words = mentions(text);
    if words.is_empty() {
        return None;
    }
    let teammates = roster::local(scope.home()).ok()?;
    let peers: Vec<String> = scope.config().peers.keys().cloned().collect();
    let named: Vec<String> = words
        .iter()
        .filter_map(
            |word| match roster::resolve(word, &teammates, &peers).ok()? {
                Resolved::Local(mate) if mate.enabled => Some(format!(
                    "@{word} is the bot profile `{}` (\"{}\") on this machine",
                    mate.profile, mate.title
                )),
                Resolved::Peer { peer, agent } => Some(format!(
                    "@{word} is agent `{agent}` on peer gateway `{peer}`"
                )),
                _ => None,
            },
        )
        .collect();
    (!named.is_empty()).then(|| {
        format!(
            "[mention] {}. Nothing was sent to them: contact them with message_agent if needed.",
            named.join("; ")
        )
    })
}

/// The notices of one session's turns.
pub struct Notes {
    scope: ProfileScope,
    store: Arc<Store>,
    bot_chat: bool,
}

/// The turn notices of a session of `scope` (`bot_chat`: its canonical
/// Bot Chat with the protocol on).
#[must_use]
pub fn notes(scope: &ProfileScope, store: &Arc<Store>, bot_chat: bool) -> Arc<dyn TurnNotes> {
    Arc::new(Notes {
        scope: scope.clone(),
        store: Arc::clone(store),
        bot_chat,
    })
}

impl TurnNotes for Notes {
    fn before_turn(&self, session_id: &str, user_text: &str) -> Vec<String> {
        let mut notices = Vec::new();
        if self.bot_chat
            && let Some(update) = roster_update(&self.scope, &self.store, session_id)
        {
            notices.push(update);
        }
        if let Some(mentioned) = mention_notice(&self.scope, user_text) {
            notices.push(mentioned);
        }
        notices
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::{Home, Launch, Overrides, ProfileName};

    use super::*;

    fn agent(name: &str, title: &str, role: &str) -> RosterAgent {
        RosterAgent {
            name: name.to_owned(),
            title: Some(title.to_owned()),
            description: Some(role.to_owned()),
            accepts_bot_messages: true,
            ..RosterAgent::default()
        }
    }

    /// Scenario: New teammate mid-session, for peer agents: the section
    /// lists the cached peer rows (a peer not listed yet as its address
    /// form), and a peer agent that shows up later arrives as one
    /// `roster.update` notice.
    #[test]
    fn peer_agents_are_listed_and_their_arrival_is_a_notice() {
        let root = tempfile::tempdir().unwrap();
        let home = Home::open(root.path()).unwrap();
        let profile = home.profile(&ProfileName::default_profile()).unwrap();
        std::fs::write(
            profile.path().join("config.toml"),
            "[peers.homelab]\nurl = \"http://127.0.0.1:9\"\nkey_secret = \"HOMELAB_KEY\"\n",
        )
        .unwrap();
        let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
        let store = Store::open(&scope).unwrap();
        let unlisted = section(&scope, &store, "ses_a");
        assert!(
            unlisted.contains("- homelab/<agent>: the agents of peer gateway homelab"),
            "{unlisted}"
        );

        let mut remote = Remote::default();
        remote.peer(
            "homelab",
            &agent("research", "Research", "digs up papers"),
            "nas",
        );
        assert!(remember_remote(&store, &remote).unwrap());
        assert!(!remember_remote(&store, &remote).unwrap(), "unchanged");
        let listed = section(&scope, &store, "ses_b");
        assert!(
            listed.contains("- @homelab/research: Research — digs up papers (machine: nas)"),
            "{listed}"
        );
        assert!(!listed.contains("homelab/<agent>"), "{listed}");
        assert_eq!(roster_update(&scope, &store, "ses_b"), None);

        remote.peer("homelab", &agent("writer", "Writer", "drafts"), "nas");
        remember_remote(&store, &remote).unwrap();
        assert_eq!(
            roster_update(&scope, &store, "ses_b").as_deref(),
            Some("[roster.update] added @homelab/writer: Writer — drafts (machine: nas)")
        );
        assert_eq!(roster_update(&scope, &store, "ses_b"), None, "told once");
    }

    #[test]
    fn mentions_skip_email_addresses_and_keep_handles() {
        assert_eq!(
            mentions("@researcher have a look, and mail bob@example.com or @writer."),
            ["researcher", "writer"]
        );
        assert_eq!(mentions("ping @homelab/research now"), ["homelab/research"]);
        assert!(mentions("write to @example.com").is_empty());
        assert!(mentions("no mention here").is_empty());
    }
}
