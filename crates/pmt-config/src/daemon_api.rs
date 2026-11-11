//! `daemon.toml [api_server]` (spec: api-server, Opt-in listener that
//! fails closed; Browser access, Host checks and security headers): the
//! daemon-wide listener every profile with `api_server.enabled` shares.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `daemon.toml [api_server]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ApiListener {
    /// Where the API server listens.
    pub listen: String,
    /// Allow a non-loopback `listen` address (logged at WARN on every
    /// start).
    pub public: bool,
    /// Exact browser origins (`https://chat.example.com`) allowed to call
    /// the API; empty sends no CORS headers and refuses every request
    /// carrying an `Origin`.
    pub cors_origins: Vec<String>,
    /// A host name accepted in the `Host` header besides the loopback
    /// names (a reverse proxy's name for a loopback listener).
    pub public_host: Option<String>,
}

impl Default for ApiListener {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8642".to_owned(),
            public: false,
            cors_origins: Vec::new(),
            public_host: None,
        }
    }
}

impl ApiListener {
    /// The parsed listen address.
    ///
    /// # Errors
    ///
    /// The validation message for a malformed address or a non-loopback
    /// one without `public = true`.
    pub fn listen_addr(&self) -> Result<std::net::SocketAddr, String> {
        let addr: std::net::SocketAddr = self.listen.parse().map_err(|_| {
            format!(
                "api_server.listen: '{}' is not an address such as 127.0.0.1:8642",
                self.listen
            )
        })?;
        if addr.ip().is_loopback() || self.public {
            Ok(addr)
        } else {
            Err("non-loopback api_server.listen requires api_server.public = true".to_owned())
        }
    }

    /// Checks every key.
    ///
    /// # Errors
    ///
    /// The validation message of the first bad key.
    pub fn validate(&self) -> Result<(), String> {
        self.listen_addr()?;
        for origin in &self.cors_origins {
            if origin == "*" {
                return Err(
                    "api_server.cors_origins: '*' is not allowed; list exact origins".to_owned(),
                );
            }
            if !is_origin(origin) {
                return Err(format!(
                    "api_server.cors_origins: '{origin}' is not an origin such as https://chat.example.com"
                ));
            }
        }
        Ok(())
    }
}

/// `http(s)://host[:port]` with nothing after the authority.
fn is_origin(text: &str) -> bool {
    let Some(authority) = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))
    else {
        return false;
    };
    !authority.is_empty()
        && authority
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
}

#[cfg(test)]
mod tests {
    use super::ApiListener;

    /// Scenario: Public bind needs explicit opt-in.
    #[test]
    fn a_public_bind_needs_public_true() {
        let listener = ApiListener {
            listen: "0.0.0.0:8642".to_owned(),
            ..ApiListener::default()
        };
        assert_eq!(
            listener.validate().unwrap_err(),
            "non-loopback api_server.listen requires api_server.public = true"
        );
        let public = ApiListener {
            public: true,
            ..listener
        };
        assert!(public.validate().is_ok());
    }

    #[test]
    fn a_wildcard_or_malformed_origin_fails() {
        for bad in ["*", "chat.example.com", "https://chat.example.com/"] {
            let listener = ApiListener {
                cors_origins: vec![bad.to_owned()],
                ..ApiListener::default()
            };
            assert!(listener.validate().is_err(), "{bad}");
        }
        let good = ApiListener {
            cors_origins: vec!["http://localhost:3000".to_owned()],
            ..ApiListener::default()
        };
        assert!(good.validate().is_ok());
    }
}
