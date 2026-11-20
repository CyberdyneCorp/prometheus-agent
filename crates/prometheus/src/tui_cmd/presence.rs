//! Shared live sessions in the TUI (spec: tui, Shared live sessions): the
//! other clients attached to the shown session (a status item, and a
//! notice as each joins or leaves), whether this TUI only observes it, and
//! each input labelled with the client the gateway stamped it with.

use pmt_i18n::t;
use pmt_rpc::contracts::{AttachRole, PresenceClient, SessionPresencePayload};
use serde_json::Value;

use super::screen::Tui;

impl Tui<'_> {
    /// `session.presence`: the clients now attached to a live session.
    pub(super) fn on_presence(&mut self, data: &Value) {
        let Ok(presence) = serde_json::from_value::<SessionPresencePayload>(data.clone()) else {
            return;
        };
        if !self.link.live.contains(&presence.session_id) {
            return;
        }
        let before = self
            .presence
            .insert(presence.session_id.clone(), presence.clients.clone())
            .unwrap_or_default();
        if presence.session_id == self.link.session {
            self.announce(&before, &presence.clients);
            self.sync_presence();
        }
    }

    /// Notices for the other clients that joined or left.
    fn announce(&mut self, before: &[PresenceClient], after: &[PresenceClient]) {
        let me = self.link.me.clone();
        let other = |c: &&PresenceClient| Some(&c.client_id) != me.as_ref();
        let strings = self.strings();
        let mut lines = Vec::new();
        for client in after.iter().filter(other) {
            if !before.iter().any(|b| b.client_id == client.client_id) {
                lines.push(t!(
                    strings,
                    "tui.joined",
                    0 = client.client_id,
                    1 = role_name(client.role)
                ));
            }
        }
        for client in before.iter().filter(other) {
            if !after.iter().any(|a| a.client_id == client.client_id) {
                lines.push(t!(strings, "tui.left", 0 = client.client_id));
            }
        }
        for line in lines {
            self.notice(line);
        }
    }

    /// The status item: who else is on the shown session, and whether
    /// this TUI observes it.
    pub(super) fn sync_presence(&mut self) {
        let strings = self.strings();
        let me = self.link.me.as_deref();
        let others: Vec<String> = self
            .presence
            .get(&self.link.session)
            .into_iter()
            .flatten()
            .filter(|c| Some(c.client_id.as_str()) != me)
            .map(|c| match c.role {
                AttachRole::Driver => c.client_id.clone(),
                AttachRole::Observer => format!("{} ({})", c.client_id, role_name(c.role)),
            })
            .collect();
        let mut items = Vec::new();
        if self.link.role == AttachRole::Observer {
            items.push(strings.get("tui.observing").to_owned());
        }
        if !others.is_empty() {
            items.push(t!(strings, "tui.shared", 0 = others.join(", ")));
        }
        self.app.status.shared = (!items.is_empty()).then(|| items.join(" · "));
    }

    /// Whether another client shares the shown session.
    fn shared(&self) -> bool {
        self.presence
            .get(&self.link.session)
            .is_some_and(|clients| clients.len() > 1)
    }

    /// An input as the transcript shows it: labelled with its author when
    /// another client sent it or the session is shared.
    pub(super) fn input_line(&self, text: &str, author: Option<&str>) -> String {
        let Some(author) = author else {
            return text.to_owned();
        };
        let mine = self.link.me.as_deref() == Some(author);
        if mine && !self.shared() {
            return text.to_owned();
        }
        let label = if mine {
            self.strings().get("tui.you").to_owned()
        } else {
            author.to_owned()
        };
        format!("[{label}] {text}")
    }
}

/// A role as the screen names it.
fn role_name(role: AttachRole) -> &'static str {
    match role {
        AttachRole::Driver => "driver",
        AttachRole::Observer => "observer",
    }
}
