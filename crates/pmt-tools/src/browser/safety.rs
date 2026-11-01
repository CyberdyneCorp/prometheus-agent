//! Browser URL safety (spec: execution-environments, Browser URL safety).
//!
//! Before any navigation: a URL carrying a secret-shaped or known secret
//! value (raw or percent-decoded) is refused, cloud metadata endpoints are
//! always refused, and private or internal addresses are refused unless the
//! session's egress policy allows them.

use std::net::IpAddr;
use std::sync::{Arc, LazyLock};

use pmt_core::secret::Redactor;
use pmt_net::DestinationPolicy;
use pmt_net::destination::RefusalReason;
use pmt_net::resolve::Lookup;
use regex::Regex;

/// The refusal of a URL holding a credential.
pub const SECRET: &str = "Blocked: URL contains what appears to be an API key or token.";
/// The refusal of a cloud metadata endpoint.
pub const METADATA: &str = "Blocked: URL targets a cloud metadata endpoint";
/// The refusal of a private address.
pub const PRIVATE: &str = "Blocked: URL targets a private or internal address";

/// Well-known credential shapes.
static SECRET_SHAPES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(sk-(ant-|proj-)?[A-Za-z0-9_\-]{20,})",
        r"|(gh[pousr]_[A-Za-z0-9]{30,})|(github_pat_[A-Za-z0-9_]{30,})",
        r"|(xox[abposr]-[A-Za-z0-9\-]{10,})|(AKIA[0-9A-Z]{16})|(AIza[0-9A-Za-z_\-]{35})",
        r"|(glpat-[A-Za-z0-9_\-]{20,})|(eyJ[A-Za-z0-9_\-]{10,}\.eyJ[A-Za-z0-9_\-]{10,}\.)",
        r"|([?&](api[_-]?key|apikey|access[_-]?token|auth[_-]?token|token|secret|client[_-]?secret|password|passwd)=[^&#]{8,})",
    ))
    .expect("valid secret pattern")
});

/// Checks navigations of one session.
#[derive(Clone)]
pub struct UrlSafety {
    /// The session's destination policy (`security.egress`, grants).
    pub destinations: DestinationPolicy,
    /// Resolves host names for the address check.
    pub lookup: Arc<dyn Lookup>,
    /// The session's known secret values.
    pub secrets: Redactor,
}

impl UrlSafety {
    /// `Ok` when `url` may be opened, else the refusal text.
    ///
    /// # Errors
    ///
    /// [`SECRET`], [`METADATA`] or [`PRIVATE`].
    pub async fn check(&self, url: &str) -> Result<(), &'static str> {
        let decoded = percent_encoding::percent_decode_str(url).decode_utf8_lossy();
        for text in [url, decoded.as_ref()] {
            if SECRET_SHAPES.is_match(text) || self.secrets.redact(text) != text {
                return Err(SECRET);
            }
        }
        let Ok(parsed) = url::Url::parse(url) else {
            return Ok(());
        };
        if !matches!(parsed.scheme(), "http" | "https" | "ws" | "wss") {
            return Ok(());
        }
        let Some(host) = parsed.host_str() else {
            return Ok(());
        };
        let port = parsed.port_or_known_default();
        let literal = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = literal.parse::<IpAddr>() {
            return verdict(self.destinations.check_addr(None, ip, port));
        }
        if self.destinations.check_host(host).is_err() {
            return Err(METADATA);
        }
        // An unresolvable host is left to the browser (it fails there).
        let Ok(addresses) = self.lookup.lookup(host).await else {
            return Ok(());
        };
        for ip in addresses {
            verdict(self.destinations.check_addr(Some(host), ip, port))?;
        }
        Ok(())
    }
}

fn verdict(checked: Result<(), pmt_net::destination::Refusal>) -> Result<(), &'static str> {
    match checked {
        Ok(()) => Ok(()),
        Err(refusal) => match refusal.reason {
            RefusalReason::Metadata => Err(METADATA),
            RefusalReason::Private => Err(PRIVATE),
            RefusalReason::Unresolved => Ok(()),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::net::IpAddr;
    use std::sync::Arc;

    use pmt_core::secret::{Redactor, SecretString};
    use pmt_net::DestinationPolicy;
    use pmt_net::resolve::{Lookup, LookupFuture};

    use super::{METADATA, PRIVATE, SECRET, UrlSafety};

    struct Fixed;

    impl Lookup for Fixed {
        fn lookup(&self, host: &str) -> LookupFuture {
            let answer: io::Result<Vec<IpAddr>> = match host {
                "intranet.test" => Ok(vec!["10.0.0.7".parse().unwrap()]),
                "public.test" => Ok(vec!["93.184.216.34".parse().unwrap()]),
                _ => Err(io::Error::new(io::ErrorKind::NotFound, "nx")),
            };
            Box::pin(async move { answer })
        }
    }

    fn safety(allow_private: bool) -> UrlSafety {
        UrlSafety {
            destinations: DestinationPolicy::new(allow_private, &[]).unwrap(),
            lookup: Arc::new(Fixed),
            secrets: Redactor::new([&SecretString::new("hunter2-known-value")]),
        }
    }

    #[tokio::test]
    async fn metadata_is_refused_even_where_private_addresses_are_allowed() {
        let open = safety(true);
        assert_eq!(
            open.check("http://169.254.169.254/latest/meta-data").await,
            Err(METADATA)
        );
        assert_eq!(
            open.check("http://metadata.google.internal/").await,
            Err(METADATA)
        );
        assert_eq!(open.check("http://intranet.test/wiki").await, Ok(()));
        assert_eq!(open.check("http://127.0.0.1:3000/").await, Ok(()));
    }

    #[tokio::test]
    async fn private_addresses_need_the_egress_policy() {
        let closed = safety(false);
        assert_eq!(
            closed.check("http://intranet.test/wiki").await,
            Err(PRIVATE)
        );
        assert_eq!(closed.check("http://192.168.1.1/").await, Err(PRIVATE));
        assert_eq!(closed.check("https://public.test/").await, Ok(()));
        assert_eq!(closed.check("about:blank").await, Ok(()));
    }

    #[tokio::test]
    async fn secrets_in_urls_are_refused_raw_or_encoded() {
        let s = safety(true);
        for url in [
            "https://public.test/?api_key=abcdef0123456789",
            "https://public.test/cb#sk-proj-abcdefghijklmnopqrstuvwx",
            "https://public.test/p?q=hunter2-known-value",
            "https://public.test/p?q=hunter2%2Dknown%2Dvalue",
            "https://public.test/x/ghp_abcdefghijklmnopqrstuvwxyz0123456789",
        ] {
            assert_eq!(s.check(url).await, Err(SECRET), "{url}");
        }
        assert_eq!(s.check("https://public.test/search?q=rust").await, Ok(()));
    }
}
