//! The Microsoft Graph change-notification listener (spec:
//! platform-adapters, Microsoft Graph change-notification listener): the
//! `msgraph_webhook` adapter, and the `teams_meetings` pipeline built on
//! it, answer Graph on the shared listener with this.
//!
//! A `validationToken` is echoed as `text/plain` 200 (the subscription
//! handshake; a bare GET is 400). A notification batch is admitted item by
//! item: an item for a resource outside `accepted_resources` is dropped,
//! one whose `clientState` differs from the secret named by
//! `client_state_secret` (compared in constant time) fails, and the last
//! `max_seen_receipts` admitted ids are remembered so a redelivery is a
//! duplicate. The batch is answered 202 when anything was admitted or a
//! duplicate, 403 when every item failed `clientState` (a forged batch),
//! else 400; a source address outside `allowed_source_cidrs` is 403
//! before the body is read, and a body that is not `{"value": [...]}` is
//! 400. The source address is the one the shared listener puts in the
//! [`PEER_HEADER`] header (it drops any copy the client sent).

use std::collections::VecDeque;
use std::net::IpAddr;

use serde_json::Value;

use crate::{HttpRequest, HttpResponse, host, policy, respond, sign};

/// The header the shared listener puts the client's address in.
pub const PEER_HEADER: &str = "x-prometheus-peer";

const DEFAULT_MAX_SEEN: usize = 5000;

/// An address range (`10.0.0.0/8`, `2603:1000::/24`, or one address).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    bits: u32,
}

impl Cidr {
    /// Parses `a.b.c.d/n`, `x::/n` or a bare address.
    ///
    /// # Errors
    ///
    /// Text that is not a range.
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = || format!("'{text}' is not an address range");
        let (address, bits) = match text.trim().split_once('/') {
            Some((a, b)) => (a, Some(b.parse::<u32>().map_err(|_| bad())?)),
            None => (text.trim(), None),
        };
        let network: IpAddr = address.parse().map_err(|_| bad())?;
        let max = if network.is_ipv4() { 32 } else { 128 };
        let bits = bits.unwrap_or(max);
        if bits > max {
            return Err(bad());
        }
        Ok(Self { network, bits })
    }

    /// Whether `address` lies in the range.
    #[must_use]
    pub fn contains(&self, address: IpAddr) -> bool {
        let (net, addr, width) = match (self.network, address) {
            (IpAddr::V4(n), IpAddr::V4(a)) => {
                (u128::from(n.to_bits()), u128::from(a.to_bits()), 32)
            }
            (IpAddr::V6(n), IpAddr::V6(a)) => (n.to_bits(), a.to_bits(), 128),
            (IpAddr::V6(n), IpAddr::V4(a)) => (n.to_bits(), a.to_ipv6_mapped().to_bits(), 128),
            (IpAddr::V4(_), IpAddr::V6(a)) => match a.to_ipv4_mapped() {
                Some(v4) => return self.contains(IpAddr::V4(v4)),
                None => return false,
            },
        };
        if self.bits == 0 {
            return true;
        }
        let shift = width - self.bits;
        (net >> shift) == (addr >> shift)
    }
}

/// How one notification of a batch fared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    Admitted,
    Duplicate,
    Auth,
    Other,
}

/// The listener's settings and the receipts it remembers.
#[derive(Debug, Clone)]
pub struct Listener {
    client_state: String,
    accepted: Vec<String>,
    cidrs: Vec<Cidr>,
    max_seen: usize,
    seen: VecDeque<String>,
    /// Notifications admitted so far and duplicates seen.
    pub counts: (u64, u64),
}

impl Listener {
    /// The listener of `options`: `client_state_secret` (the secret's
    /// name, required and set), `accepted_resources`,
    /// `allowed_source_cidrs` (required when the shared listener is
    /// public, `options.listener_public`), `max_seen_receipts`.
    ///
    /// # Errors
    ///
    /// The options or the secret are missing or invalid.
    pub fn from_options(options: &Value) -> Result<Self, String> {
        let name = options["client_state_secret"]
            .as_str()
            .filter(|n| !n.is_empty())
            .ok_or("options.client_state_secret (the secret holding the subscriptions' clientState) is required")?;
        let client_state = host::secret(name)
            .ok_or_else(|| format!("secret {name} (the clientState) is not set"))?;
        let cidrs = policy::strings(options, "allowed_source_cidrs")
            .iter()
            .map(|c| Cidr::parse(c).map_err(|e| format!("options.allowed_source_cidrs: {e}")))
            .collect::<Result<Vec<_>, _>>()?;
        if options["listener_public"] == true && cidrs.is_empty() {
            return Err("options.allowed_source_cidrs must list Microsoft Graph's source ranges when gateway.http.public = true".to_owned());
        }
        let max_seen = options["max_seen_receipts"]
            .as_u64()
            .map_or(DEFAULT_MAX_SEEN, |n| {
                usize::try_from(n.max(1)).unwrap_or(DEFAULT_MAX_SEEN)
            });
        Ok(Self {
            client_state,
            accepted: policy::strings(options, "accepted_resources"),
            cidrs,
            max_seen,
            seen: VecDeque::new(),
            counts: (0, 0),
        })
    }

    fn source_allowed(&self, request: &HttpRequest) -> bool {
        if self.cidrs.is_empty() {
            return true;
        }
        crate::header(request, PEER_HEADER)
            .and_then(|a| a.trim().parse::<IpAddr>().ok())
            .is_some_and(|a| self.cidrs.iter().any(|c| c.contains(a)))
    }

    fn resource_accepted(&self, resource: &str) -> bool {
        if self.accepted.is_empty() {
            return true;
        }
        let resource = resource.trim().trim_matches('/');
        self.accepted.iter().any(|pattern| {
            let pattern = pattern.trim().trim_matches('/');
            match pattern.strip_suffix('*') {
                Some(prefix) => resource.starts_with(prefix),
                None => resource == pattern,
            }
        })
    }

    fn fate(&mut self, item: &Value) -> Fate {
        if !item.is_object() || !self.resource_accepted(item["resource"].as_str().unwrap_or("")) {
            return Fate::Other;
        }
        let given = item["clientState"].as_str().unwrap_or_default();
        if !sign::same(given.as_bytes(), self.client_state.as_bytes()) {
            return Fate::Auth;
        }
        let key = receipt(item);
        if self.seen.contains(&key) {
            self.counts.1 += 1;
            return Fate::Duplicate;
        }
        self.seen.push_back(key);
        while self.seen.len() > self.max_seen {
            self.seen.pop_front();
        }
        self.counts.0 += 1;
        Fate::Admitted
    }

    /// Answers one request; the notifications it admitted.
    pub fn handle(&mut self, request: &HttpRequest) -> (HttpResponse, Vec<Value>) {
        if !self.source_allowed(request) {
            return (respond(403, "text/plain", ""), Vec::new());
        }
        if let Some((_, token)) = crate::query(&request.path)
            .into_iter()
            .find(|(k, _)| k == "validationToken")
        {
            return (respond(200, "text/plain", token), Vec::new());
        }
        if request.method != "POST" {
            return (respond(400, "text/plain", ""), Vec::new());
        }
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let Some(items) = body["value"].as_array() else {
            return (respond(400, "text/plain", ""), Vec::new());
        };
        let mut admitted = Vec::new();
        let (mut duplicates, mut auth, mut other) = (0, 0, 0);
        for item in items {
            match self.fate(item) {
                Fate::Admitted => admitted.push(item.clone()),
                Fate::Duplicate => duplicates += 1,
                Fate::Auth => auth += 1,
                Fate::Other => other += 1,
            }
        }
        let status = if !admitted.is_empty() || duplicates > 0 {
            202
        } else if auth > 0 && other == 0 {
            403
        } else {
            400
        };
        (respond(status, "text/plain", ""), admitted)
    }
}

/// The id a notification is remembered by: Graph's `id`, else a digest
/// of the item.
#[must_use]
pub fn receipt(item: &Value) -> String {
    match item["id"].as_str().filter(|id| !id.is_empty()) {
        Some(id) => format!("id:{id}"),
        None => format!(
            "sha:{}",
            sign::hex(&sign::sha256(item.to_string().as_bytes()))
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn listener(accepted: &[&str]) -> Listener {
        Listener {
            client_state: "s3cret".into(),
            accepted: accepted.iter().map(|s| (*s).to_owned()).collect(),
            cidrs: vec![Cidr::parse("52.96.0.0/14").unwrap()],
            max_seen: 2,
            seen: VecDeque::new(),
            counts: (0, 0),
        }
    }

    fn post(body: &Value, peer: &str) -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            path: "/".into(),
            headers: vec![(PEER_HEADER.into(), peer.into())],
            body: body.to_string().into_bytes(),
        }
    }

    #[test]
    fn ranges_match_both_families() {
        let range = Cidr::parse("10.1.0.0/16").unwrap();
        assert!(range.contains("10.1.200.3".parse().unwrap()));
        assert!(!range.contains("10.2.0.1".parse().unwrap()));
        assert!(range.contains("::ffff:10.1.0.9".parse().unwrap()));
        assert!(
            Cidr::parse("2603:1000::/24")
                .unwrap()
                .contains("2603:10ff::1".parse().unwrap())
        );
        assert!(Cidr::parse("10.0.0.0/33").is_err());
    }

    #[test]
    fn a_batch_is_admitted_once_and_forgeries_are_refused() {
        let mut graph = listener(&["communications/onlineMeetings/*"]);
        let good = json!({"id": "n1", "clientState": "s3cret", "resource": "communications/onlineMeetings/m1/transcripts/t1"});
        let body = json!({"value": [good]});
        let (answer, admitted) = graph.handle(&post(&body, "52.97.1.1"));
        assert_eq!((answer.status, admitted.len()), (202, 1));
        let (answer, admitted) = graph.handle(&post(&body, "52.97.1.1"));
        assert_eq!((answer.status, admitted.len()), (202, 0));
        let forged = json!({"value": [{"id": "n2", "clientState": "nope", "resource": "communications/onlineMeetings/m1"}]});
        assert_eq!(graph.handle(&post(&forged, "52.97.1.1")).0.status, 403);
        let elsewhere = json!({"value": [{"id": "n3", "clientState": "s3cret", "resource": "users/u1/messages"}]});
        assert_eq!(graph.handle(&post(&elsewhere, "52.97.1.1")).0.status, 400);
        assert_eq!(graph.handle(&post(&body, "8.8.8.8")).0.status, 403);
        assert_eq!(
            graph
                .handle(&post(&json!({"nope": 1}), "52.97.1.1"))
                .0
                .status,
            400
        );
    }
}
