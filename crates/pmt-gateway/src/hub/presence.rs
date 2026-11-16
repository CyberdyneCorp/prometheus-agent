//! Who is attached to a session (spec: tui, Shared live sessions): every
//! attached client receives `session.presence` listing all of them each
//! time one joins, leaves or changes its role. It goes out as a
//! daemon-level event (outside the session's numbered log), so it never
//! shifts the sequence numbers clients compare for the turn's events.

use pmt_rpc::contracts::{Event as _, PresenceClient, SessionPresence, SessionPresencePayload};

use super::{ConnId, Hub, Log, Peer};

/// Records `conn` joining the session as `peer`; `true` when that changes
/// who is attached (a new client, or a new role).
pub(super) fn join(log: &mut Log, conn: ConnId, peer: &Peer) -> bool {
    let changed = log
        .attached
        .get(&conn)
        .is_none_or(|before| before.role != peer.role);
    if !log.joined.contains_key(&conn) {
        log.joins += 1;
        log.joined
            .insert(conn, (log.joins, jiff::Timestamp::now().to_string()));
    }
    changed
}

/// The kind part of a `<kind>-<connection>` client id.
fn kind_of(client_id: &str) -> &str {
    client_id
        .rsplit_once('-')
        .map_or(client_id, |(kind, _)| kind)
}

impl Hub {
    /// Sends the session's attached clients to each of them.
    pub(super) fn presence(&self, session_id: &str) {
        let (peers, clients) = {
            let state = self.state();
            let Some(log) = state.sessions.get(session_id) else {
                return;
            };
            let mut listed: Vec<(u64, PresenceClient)> = log
                .attached
                .iter()
                .map(|(conn, peer)| {
                    let (order, since) = log.joined.get(conn).cloned().unwrap_or_default();
                    let client = PresenceClient {
                        client_id: peer.client_id.clone(),
                        client_kind: kind_of(&peer.client_id).to_owned(),
                        role: peer.role,
                        since,
                    };
                    (order, client)
                })
                .collect();
            listed.sort_by_key(|(order, _)| *order);
            let peers: Vec<Peer> = log.attached.values().cloned().collect();
            (peers, listed.into_iter().map(|(_, c)| c).collect())
        };
        let payload = SessionPresencePayload {
            session_id: session_id.to_owned(),
            clients,
        };
        let data = serde_json::to_value(payload).unwrap_or_default();
        self.notify(&peers, SessionPresence::NAME, data);
    }
}

#[cfg(test)]
mod tests {
    use super::kind_of;

    #[test]
    fn the_kind_is_the_client_id_without_its_connection_number() {
        assert_eq!(kind_of("desktop-12"), "desktop");
        assert_eq!(kind_of("tui-3"), "tui");
        assert_eq!(kind_of("gateway"), "gateway");
    }
}
