//! Reachability of local endpoints (spec: model-providers, Local endpoints
//! never fall back to remote providers; Auto provider detection): a TCP
//! connect to the endpoint's host and port, never a model request.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// A boxed future (object-safe async trait methods).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Answers whether an endpoint accepts connections.
pub trait EndpointProbe: Send + Sync {
    /// Whether `base_url`'s host and port accept a connection.
    fn reachable<'a>(&'a self, base_url: &'a str) -> BoxFuture<'a, bool>;
}

/// The real probe: TCP connect with a short timeout.
#[derive(Debug, Clone, Copy)]
pub struct TcpProbe {
    /// Connect timeout.
    pub timeout: Duration,
}

impl Default for TcpProbe {
    fn default() -> Self {
        Self {
            timeout: Duration::from_millis(500),
        }
    }
}

impl EndpointProbe for TcpProbe {
    fn reachable<'a>(&'a self, base_url: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let Some(address) = socket_address(base_url) else {
                return false;
            };
            matches!(
                tokio::time::timeout(self.timeout, tokio::net::TcpStream::connect(address)).await,
                Ok(Ok(_))
            )
        })
    }
}

/// `host:port` of an `http(s)` URL (default ports 80/443).
fn socket_address(url: &str) -> Option<String> {
    let host = pmt_config::provider_urls::host_of(url)?;
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let port = authority
        .rsplit_once(':')
        .filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty())
        .map_or_else(
            || {
                if url.starts_with("https://") {
                    "443"
                } else {
                    "80"
                }
                .to_owned()
            },
            |(_, p)| p.to_owned(),
        );
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    Some(format!("{host}:{port}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_take_explicit_or_scheme_ports() {
        assert_eq!(
            socket_address("http://127.0.0.1:11434/v1").as_deref(),
            Some("127.0.0.1:11434")
        );
        assert_eq!(
            socket_address("https://x.test/v1").as_deref(),
            Some("x.test:443")
        );
        assert_eq!(
            socket_address("http://[::1]:8080").as_deref(),
            Some("[::1]:8080")
        );
    }

    #[tokio::test]
    async fn a_listening_port_is_reachable_and_a_closed_one_is_not() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let probe = TcpProbe::default();
        assert!(
            probe
                .reachable(&format!("http://127.0.0.1:{port}/v1"))
                .await
        );
        drop(listener);
        assert!(
            !probe
                .reachable(&format!("http://127.0.0.1:{port}/v1"))
                .await
        );
    }
}
