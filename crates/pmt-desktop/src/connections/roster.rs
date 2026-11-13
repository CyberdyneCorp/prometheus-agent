//! The union agent roster (spec: desktop-app, Agents across connections):
//! every profile of every registered connection is an agent. A handle is
//! `@<profile>`, or `@<profile>-<connection name>` when that profile name
//! exists on several connections; a connection that could not be listed
//! is reported on its own row without breaking the roster. A handle only
//! names an agent: nothing routes a mention to another machine.

use serde::Serialize;

/// What listing one connection found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// The connection's id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// Its profiles, or why they could not be listed.
    pub profiles: Result<Vec<String>, String>,
}

/// One agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Agent {
    /// The connection it runs on.
    pub connection_id: String,
    /// That connection's name.
    pub connection: String,
    /// The profile.
    pub profile: String,
    /// `@profile` or `@profile-connection`.
    pub handle: String,
}

/// One connection's row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectionRow {
    /// The connection's id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// Whether its profiles were listed.
    pub reachable: bool,
    /// Why not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Roster {
    /// Every agent of every reachable connection.
    pub agents: Vec<Agent>,
    /// Every connection, reachable or not.
    pub connections: Vec<ConnectionRow>,
}

/// A connection name as a handle suffix: lowercase, runs of other
/// characters as one `-`.
#[must_use]
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
}

/// The roster of what each connection listed.
#[must_use]
pub fn build(listed: &[Listed]) -> Roster {
    let shared = |profile: &str| {
        listed
            .iter()
            .filter(|l| {
                l.profiles
                    .as_ref()
                    .is_ok_and(|p| p.iter().any(|x| x == profile))
            })
            .count()
            > 1
    };
    let mut agents = Vec::new();
    let mut connections = Vec::new();
    for l in listed {
        connections.push(ConnectionRow {
            id: l.id.clone(),
            name: l.name.clone(),
            reachable: l.profiles.is_ok(),
            error: l.profiles.as_ref().err().cloned(),
        });
        for profile in l.profiles.iter().flatten() {
            let handle = if shared(profile) {
                format!("@{profile}-{}", slug(&l.name))
            } else {
                format!("@{profile}")
            };
            agents.push(Agent {
                connection_id: l.id.clone(),
                connection: l.name.clone(),
                profile: profile.clone(),
                handle,
            });
        }
    }
    Roster {
        agents,
        connections,
    }
}

#[cfg(test)]
mod tests {
    use super::{Listed, build, slug};

    fn listed(id: &str, name: &str, profiles: &[&str]) -> Listed {
        Listed {
            id: id.into(),
            name: name.into(),
            profiles: Ok(profiles.iter().map(|p| (*p).to_owned()).collect()),
        }
    }

    /// Scenario: Same profile name on two machines.
    #[test]
    fn a_profile_on_two_machines_gets_connection_handles() {
        let roster = build(&[
            listed("a", "Homelab", &["research", "home"]),
            listed("b", "Laptop", &["research"]),
            Listed {
                id: "c".into(),
                name: "VPS".into(),
                profiles: Err("unreachable: connection refused".into()),
            },
        ]);
        let handles: Vec<&str> = roster.agents.iter().map(|a| a.handle.as_str()).collect();
        assert_eq!(handles, ["@research-homelab", "@home", "@research-laptop"]);
        let vps = &roster.connections[2];
        assert!(!vps.reachable);
        assert_eq!(
            vps.error.as_deref(),
            Some("unreachable: connection refused")
        );
    }

    #[test]
    fn connection_names_become_handle_suffixes() {
        assert_eq!(slug("Home Lab #2"), "home-lab-2");
        assert_eq!(slug("VPS"), "vps");
    }
}
