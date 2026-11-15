//! Agents across gateways in the daemon (spec: agent-interop, Union roster
//! across gateways; Desktop relay for cross-connection bot messages).
//!
//! The roster merges this daemon's profiles, each registered peer's
//! `/api/agents` (listed again after 300 s, or on demand) and the agents
//! desktops report for their other connections when they drain the relay.
//! A source that cannot be reached keeps its last rows, marked stale.
//!
//! The relay hands a courier the envelopes of a profile's outbox and runs
//! an envelope it delivers as the target's canonical Bot Chat's next turn,
//! once per delivery id; the reply travels back the same way.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use pmt_config::schema::BusyInputMode;
use pmt_core::error::{Error, codes};
use pmt_gateway::Author;
use pmt_rpc::contracts::{
    RelayDeliverParams, RelayDelivered, RelayDrainParams, RelayDrained, RelayEnvelope, RelayReply,
    Roster, RosterAgent, RosterParams, RosterRow, RosterSource, SubmitParams,
};

use super::{DaemonHost, Profile};
use crate::daemon_cmd::peers::client::Peer;

/// How long a peer's listing is used before it is asked again.
const FRESH: Duration = Duration::from_mins(5);
/// How long one peer listing may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// One remote source's last listing.
#[derive(Debug, Clone)]
struct Listing {
    agents: Vec<RosterAgent>,
    host: Option<String>,
    error: Option<String>,
    at: Instant,
    listed_at: Timestamp,
}

/// The remote sources by (profile, kind, device).
#[derive(Default)]
pub(super) struct RosterCache {
    listings: Mutex<HashMap<(String, &'static str, String), Listing>>,
}

impl RosterCache {
    fn get(&self, key: &(String, &'static str, String)) -> Option<Listing> {
        self.listings
            .lock()
            .expect("roster poisoned")
            .get(key)
            .cloned()
    }

    fn put(&self, key: (String, &'static str, String), listing: Listing) {
        self.listings
            .lock()
            .expect("roster poisoned")
            .insert(key, listing);
    }

    /// The connections a desktop of `profile` reported (they replace the
    /// last report).
    fn report(&self, profile: &str, connections: &[pmt_rpc::contracts::RelayConnection]) {
        let mut listings = self.listings.lock().expect("roster poisoned");
        listings.retain(|(p, kind, _), _| !(p == profile && *kind == "connection"));
        for connection in connections {
            listings.insert(
                (profile.to_owned(), "connection", connection.label.clone()),
                Listing {
                    agents: connection.agents.clone(),
                    host: None,
                    error: None,
                    at: Instant::now(),
                    listed_at: Timestamp::now(),
                },
            );
        }
    }

    fn of_kind(&self, profile: &str, kind: &'static str) -> Vec<(String, Listing)> {
        let listings = self.listings.lock().expect("roster poisoned");
        let mut out: Vec<(String, Listing)> = listings
            .iter()
            .filter(|((p, k, _), _)| p == profile && *k == kind)
            .map(|((_, _, device), l)| (device.clone(), l.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// One row before handles are assigned.
struct Candidate {
    agent: RosterAgent,
    source: &'static str,
    device: String,
    stale: bool,
    error: Option<String>,
}

impl DaemonHost {
    /// `bot.roster.list`.
    pub(super) async fn roster(&self, params: RosterParams) -> Result<Roster, Error> {
        let profile = self.profile(&params.profile)?;
        let mut candidates: Vec<Candidate> = self
            .all_profiles()
            .iter()
            .map(|(_, p)| Candidate {
                agent: local_agent(p),
                source: "local",
                device: "local".to_owned(),
                stale: false,
                error: None,
            })
            .collect();
        let mut sources = vec![RosterSource {
            source: "local".to_owned(),
            device: "local".to_owned(),
            ok: true,
            error: None,
            listed_at: Some(Timestamp::now().to_string()),
        }];
        if params.include_remote {
            self.refresh_peers(&profile, &params.profile, params.refresh)
                .await;
            for kind in ["peer", "connection"] {
                for (device, listing) in self.roster.of_kind(&params.profile, kind) {
                    sources.push(RosterSource {
                        source: kind.to_owned(),
                        device: device.clone(),
                        ok: listing.error.is_none(),
                        error: listing.error.clone(),
                        listed_at: Some(listing.listed_at.to_string()),
                    });
                    candidates.extend(remote_rows(kind, &device, &listing));
                }
            }
        }
        Ok(Roster {
            rows: handles(candidates),
            sources,
        })
    }

    /// Lists every registered peer of `profile` whose listing is older
    /// than [`FRESH`] (all of them with `force`), at once.
    async fn refresh_peers(&self, profile: &Arc<Profile>, name: &str, force: bool) {
        let scope = profile.scope();
        let peers: Vec<String> = scope.config().peers.keys().cloned().collect();
        let due: Vec<String> = peers
            .into_iter()
            .filter(|peer| {
                force
                    || self
                        .roster
                        .get(&(name.to_owned(), "peer", peer.clone()))
                        .is_none_or(|l| l.at.elapsed() >= FRESH)
            })
            .collect();
        let lists = due.iter().map(|peer| {
            let scope = scope.clone();
            async move { (peer.clone(), list_peer(&scope, peer).await) }
        });
        for (peer, listed) in futures_util::future::join_all(lists).await {
            let key = (name.to_owned(), "peer", peer);
            let listing = match listed {
                Ok((host, agents)) => Listing {
                    agents,
                    host,
                    error: None,
                    at: Instant::now(),
                    listed_at: Timestamp::now(),
                },
                // The last rows stay, marked stale with the error.
                Err(error) => match self.roster.get(&key) {
                    Some(last) => Listing {
                        error: Some(error),
                        at: Instant::now(),
                        ..last
                    },
                    None => Listing {
                        agents: Vec::new(),
                        host: None,
                        error: Some(error),
                        at: Instant::now(),
                        listed_at: Timestamp::now(),
                    },
                },
            };
            self.roster.put(key, listing);
        }
    }

    /// `bot.relay.drain`.
    pub(super) fn drain(&self, params: &RelayDrainParams) -> Result<RelayDrained, Error> {
        let profile = self.profile(&params.profile)?;
        self.roster.report(&params.profile, &params.connections);
        let timing = crate::a2a_host::relay::timing(&profile.scope().config().bot_mode);
        let now = Timestamp::now();
        let timed_out = profile.store.relay_time_out(now, timing)?;
        if !timed_out.is_empty() {
            tracing::warn!(deliveries = ?timed_out, "relayed bot messages timed out (delivery_timeout)");
        }
        let drained = profile.store.relay_drain(now, timing)?;
        for id in &drained.expired {
            tracing::warn!(
                delivery = id,
                "relayed bot message expired before a courier drained it (queued_expired)"
            );
        }
        Ok(RelayDrained {
            envelopes: drained
                .offered
                .into_iter()
                .map(|e| RelayEnvelope {
                    delivery_id: e.delivery_id,
                    sender: e.sender,
                    target: e.target,
                    message: e.message,
                    signature: e.signature,
                    created_at: e.created_at.to_string(),
                })
                .collect(),
            deadline_secs: crate::a2a_host::relay::DELIVERY_DEADLINE_SECS,
        })
    }

    /// `bot.relay.deliver`.
    pub(super) async fn deliver(
        &self,
        params: RelayDeliverParams,
    ) -> Result<RelayDelivered, Error> {
        let profile = self.profile(&params.profile)?;
        match (params.envelope, params.reply) {
            (Some(envelope), None) => Box::pin(self.run_envelope(&profile, envelope)).await,
            (None, Some(reply)) => settle(&profile, &reply),
            _ => Err(Error::new(
                &codes::RPC_INVALID_PARAMS,
                "bot.relay.deliver takes exactly one of envelope and reply",
            )),
        }
    }

    /// Runs `envelope` as the profile's Bot Chat's next turn, unless its
    /// delivery id ran before (then that run's outcome is reported).
    async fn run_envelope(
        &self,
        profile: &Arc<Profile>,
        envelope: RelayEnvelope,
    ) -> Result<RelayDelivered, Error> {
        let (session, _) = self.ensure_bot_chat(profile).await?;
        let id = envelope.delivery_id.clone();
        if let Some(seen) = profile.store.relay_receive(&id, &session)? {
            return Ok(RelayDelivered {
                delivery_id: id,
                outcome: seen.state,
                reply: seen.reply,
                reason: seen.reason,
            });
        }
        let params = SubmitParams {
            session_id: session.clone(),
            text: format!(
                "Message from 🤖 {}\n\n{}",
                envelope.sender, envelope.message
            ),
            images: Vec::new(),
            files: Vec::new(),
            run_budget_secs: None,
            input_id: None,
            author: None,
        };
        let author = Author {
            client_kind: "relay".to_owned(),
            client_id: envelope.sender.clone(),
            principal: profile.scope().name().to_string(),
            sent_at: Some(Timestamp::now().to_string()),
            prefix: None,
            origin: None,
        };
        let Some(gateway) = self.gateway.get() else {
            return Err(Error::new(
                &codes::RPC_NOT_SERVED,
                "the gateway is not running",
            ));
        };
        let link = gateway.link(&session);
        let timeout =
            Duration::from_secs(profile.scope().config().bot_mode.relay_turn_timeout_secs);
        let outcome = match self
            .submit_turn(
                profile,
                params,
                (author, link),
                Some(BusyInputMode::Queue),
                None,
            )
            .await
        {
            Ok(submitted) => super::turn_results::wait(&submitted.turn_id, timeout)
                .await
                .unwrap_or_else(|| Err("delivery_timeout".to_owned())),
            Err(err) => Err(format!("runtime_offline: {}", err.message())),
        };
        let (state, reply, reason) = match &outcome {
            Ok(reply) => ("delivered", Some(reply.clone()), None),
            Err(why) => ("failed", None, Some(reason_of(why))),
        };
        profile.store.relay_received(
            &id,
            outcome
                .as_deref()
                .map_err(|_| reason.as_deref().unwrap_or("unknown")),
        )?;
        Ok(RelayDelivered {
            delivery_id: id,
            outcome: state.to_owned(),
            reply,
            reason,
        })
    }
}

/// The failure reason code of a failed relayed turn.
fn reason_of(why: &str) -> String {
    let code = why.split(':').next().unwrap_or_default();
    match code {
        "delivery_timeout" | "runtime_offline" => code.to_owned(),
        _ => "unknown".to_owned(),
    }
}

/// Settles a reply in the sender's outbox.
fn settle(profile: &Profile, reply: &RelayReply) -> Result<RelayDelivered, Error> {
    let outcome = match (&reply.reply, &reply.reason) {
        (Some(text), _) => Ok(text.as_str()),
        (None, reason) => Err(reason.as_deref().unwrap_or("unknown")),
    };
    let settled = profile
        .store
        .relay_settle(&reply.delivery_id, &reply.signature, outcome)?;
    Ok(RelayDelivered {
        delivery_id: reply.delivery_id.clone(),
        outcome: if settled { "settled" } else { "ignored" }.to_owned(),
        reply: reply.reply.clone(),
        reason: reply.reason.clone(),
    })
}

/// A local profile as a bot.
fn local_agent(profile: &Profile) -> RosterAgent {
    let scope = profile.scope();
    let config = scope.config();
    RosterAgent {
        name: scope.name().to_string(),
        title: None,
        description: config.a2a.server.description.clone(),
        avatar: None,
        model: config.model.default.clone(),
        accepts_bot_messages: true,
    }
}

/// `GET /api/agents` of `peer`: its host name and agents.
async fn list_peer(
    scope: &pmt_config::ProfileScope,
    peer: &str,
) -> Result<(Option<String>, Vec<RosterAgent>), String> {
    let client = Peer::of(scope, peer).map_err(|e| e.message().to_owned())?;
    let listed = tokio::time::timeout(LIST_TIMEOUT, client.agents())
        .await
        .map_err(|_| {
            format!(
                "peer {peer} did not answer within {} s",
                LIST_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| e.into_error(peer).message().to_owned())?;
    let agents = listed["data"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| serde_json::from_value::<AgentRow>(row.clone()).ok())
                .map(|row| RosterAgent {
                    name: row.name,
                    title: row.title,
                    description: row.description,
                    avatar: row.avatar,
                    model: row.model,
                    accepts_bot_messages: row.accepts_bot_messages.unwrap_or(true),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok((listed["host"].as_str().map(str::to_owned), agents))
}

/// A row of a peer's `/api/agents`, read leniently.
#[derive(serde::Deserialize)]
struct AgentRow {
    name: String,
    title: Option<String>,
    description: Option<String>,
    avatar: Option<String>,
    model: Option<String>,
    accepts_bot_messages: Option<bool>,
}

/// The rows of one remote listing; a remote `default` is listed under its
/// title (or `<hostname>@<source>`), never as a second primary bot.
fn remote_rows(kind: &'static str, device: &str, listing: &Listing) -> Vec<Candidate> {
    listing
        .agents
        .iter()
        .map(|agent| {
            let mut agent = agent.clone();
            if agent.name == "default" {
                let host = listing.host.clone().unwrap_or_else(|| device.to_owned());
                agent.name = agent
                    .title
                    .clone()
                    .unwrap_or_else(|| format!("{host}@{device}"));
            }
            Candidate {
                agent,
                source: kind,
                device: device.to_owned(),
                stale: listing.error.is_some(),
                error: listing.error.clone(),
            }
        })
        .collect()
}

/// A name as a handle: lowercase, runs of other characters as one `-`.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

/// Assigns handles: `@<name>` for a name one source serves,
/// `@<name>-<device>` when several do.
fn handles(candidates: Vec<Candidate>) -> Vec<RosterRow> {
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    for c in &candidates {
        *count.entry(slug(&c.agent.name)).or_default() += 1;
    }
    candidates
        .into_iter()
        .map(|c| {
            let name = slug(&c.agent.name);
            let handle = if count.get(&name).copied().unwrap_or(0) > 1 {
                format!("@{name}-{}", slug(&c.device))
            } else {
                format!("@{name}")
            };
            RosterRow {
                handle,
                agent: c.agent,
                source: c.source.to_owned(),
                device: c.device,
                stale: c.stale,
                error: c.error,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str, source: &'static str, device: &str) -> Candidate {
        Candidate {
            agent: RosterAgent {
                name: name.to_owned(),
                accepts_bot_messages: true,
                ..RosterAgent::default()
            },
            source,
            device: device.to_owned(),
            stale: false,
            error: None,
        }
    }

    /// Scenario: Same name on two machines.
    #[test]
    fn a_name_on_two_sources_gets_device_handles() {
        let rows = handles(vec![
            candidate("research", "local", "local"),
            candidate("default", "local", "local"),
            candidate("research", "peer", "homelab"),
            candidate("writer", "peer", "homelab"),
        ]);
        let handles: Vec<&str> = rows.iter().map(|r| r.handle.as_str()).collect();
        assert_eq!(
            handles,
            [
                "@research-local",
                "@default",
                "@research-homelab",
                "@writer"
            ]
        );
    }

    #[test]
    fn a_remote_default_is_named_by_title_or_host() {
        let listing = Listing {
            agents: vec![RosterAgent {
                name: "default".into(),
                ..RosterAgent::default()
            }],
            host: Some("spark-box".into()),
            error: None,
            at: Instant::now(),
            listed_at: Timestamp::now(),
        };
        let rows = remote_rows("peer", "spark", &listing);
        assert_eq!(rows[0].agent.name, "spark-box@spark");
        assert_eq!(slug("spark-box@spark"), "spark-box-spark");
    }
}
