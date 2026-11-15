//! Whole-config checks of `[a2a]` and `[peers]` the schema cannot express
//! (spec: agent-interop): peer gateway names follow
//! `^[a-z][a-z0-9-]{0,31}$`, A2A peer identities are unique and non-empty,
//! and every URL is an absolute `http(s)` URL.

use pmt_core::error::{Error, codes};

use crate::schema::{Config, valid_peer_name};

/// Checks `[a2a]` and `[peers]`.
///
/// # Errors
///
/// `config.invalid` (exit 78) naming the offending key.
pub(crate) fn check(config: &Config) -> Result<(), Error> {
    for (name, peer) in &config.peers {
        if !valid_peer_name(name) {
            return Err(invalid(
                &format!("peers.{name}"),
                "a peer name matches ^[a-z][a-z0-9-]{0,31}$",
            ));
        }
        check_url(&format!("peers.{name}.url"), &peer.url)?;
    }
    let server = &config.a2a.server;
    let mut seen = std::collections::BTreeSet::new();
    for (i, peer) in server.peers.iter().enumerate() {
        let key = format!("a2a.server.peers[{i}].name");
        if peer.name.trim().is_empty() {
            return Err(invalid(&key, "a peer identity cannot be empty"));
        }
        if !seen.insert(peer.name.as_str()) {
            return Err(invalid(&key, &format!("'{}' is listed twice", peer.name)));
        }
    }
    if let Some(url) = &server.public_url {
        check_url("a2a.server.public_url", url)?;
    }
    for (name, agent) in &config.a2a.agents {
        check_url(&format!("a2a.agents.{name}.url"), &agent.url)?;
    }
    Ok(())
}

fn check_url(key: &str, url: &str) -> Result<(), Error> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    let host = rest.map(|r| r.split(['/', '?', '#']).next().unwrap_or_default());
    match host {
        Some(host) if !host.is_empty() && !host.contains(char::is_whitespace) => Ok(()),
        _ => Err(invalid(key, &format!("'{url}' is not an http(s) URL"))),
    }
}

fn invalid(key: &str, why: &str) -> Error {
    Error::new(&codes::CONFIG_INVALID, format!("{key}: {why}"))
}

#[cfg(test)]
mod tests {
    use super::check;
    use crate::schema::Config;

    fn config(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn peer_names_and_urls_are_checked() {
        let ok = config(
            "[peers.spark]\nurl = \"http://spark.lan:8642\"\nkey_secret = \"PEER_SPARK_KEY\"\n",
        );
        assert!(check(&ok).is_ok());
        let bad = config(
            "[peers.Spark]\nurl = \"http://spark.lan:8642\"\nkey_secret = \"PEER_SPARK_KEY\"\n",
        );
        assert!(check(&bad).unwrap_err().message().contains("peers.Spark"));
        let url = config("[peers.spark]\nurl = \"spark.lan\"\nkey_secret = \"PEER_SPARK_KEY\"\n");
        assert!(
            check(&url)
                .unwrap_err()
                .message()
                .contains("not an http(s) URL")
        );
    }

    #[test]
    fn duplicate_a2a_identities_are_refused() {
        let text = "[[a2a.server.peers]]\nname = \"alice\"\ntoken_secret = \"A\"\n\
                    [[a2a.server.peers]]\nname = \"alice\"\ntoken_secret = \"B\"\n";
        assert!(
            check(&config(text))
                .unwrap_err()
                .message()
                .contains("listed twice")
        );
    }
}
