//! Signed-in browsers (spec: web-dashboard, Loopback authentication with
//! an `HttpOnly` session cookie; CSRF protection on state-changing requests;
//! Remote dashboard users are observers by default): the cookie's token
//! (kept as a hash) maps to the subject, its role and the CSRF token bound
//! to it, and expires after 12 h without a request.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::secret::{hash, random_hex, same};

/// The session cookie's name.
pub(crate) const COOKIE: &str = "prometheus_session";
/// Idle time after which a session ends.
pub(crate) const IDLE: Duration = Duration::from_hours(12);

/// What a signed-in browser may do in sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// Submits, interrupts and answers.
    Driver,
    /// Watches only.
    Observer,
}

impl Role {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Driver => "driver",
            Self::Observer => "observer",
        }
    }
}

/// How a caller authenticated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Via {
    /// A browser's session cookie (an ambient credential: CSRF applies).
    Cookie,
    /// The token mode's deployment credential.
    Deployment,
    /// A paired device's token.
    Device,
}

/// A signed-in caller.
#[derive(Debug, Clone)]
pub(crate) struct Signed {
    /// Who: `local` for a login link, `password`, an OIDC or portal
    /// subject, `token`, or `device:<name>`.
    pub subject: String,
    pub role: Role,
    /// The CSRF token every state-changing cookie request must echo.
    pub csrf: String,
    pub via: Via,
    /// The profiles it may use (`None`: every served profile).
    pub profiles: Option<Vec<String>>,
    /// Cancelled when the credential ends (sign-out, revocation): its
    /// sockets close with 4401.
    pub ended: CancellationToken,
}

impl Signed {
    /// Whether `profile` is one this caller may use.
    pub(crate) fn may_use(&self, profile: &str) -> bool {
        self.profiles
            .as_ref()
            .is_none_or(|list| list.iter().any(|p| p == profile))
    }
}

#[derive(Debug)]
struct Entry {
    signed: Signed,
    last_seen: Instant,
}

/// The listener's browser sessions.
#[derive(Debug, Default)]
pub(crate) struct Sessions {
    live: Mutex<HashMap<String, Entry>>,
}

impl Sessions {
    /// Signs a browser in; the cookie token and its session.
    pub(crate) fn create(&self, subject: &str, role: Role) -> (String, Signed) {
        let token = random_hex(32);
        let signed = Signed {
            subject: subject.to_owned(),
            role,
            csrf: random_hex(24),
            via: Via::Cookie,
            profiles: None,
            ended: CancellationToken::new(),
        };
        let now = Instant::now();
        let mut live = self.live.lock().expect("sessions poisoned");
        live.retain(|_, e| {
            let fresh = now.duration_since(e.last_seen) < IDLE;
            if !fresh {
                e.signed.ended.cancel();
            }
            fresh
        });
        live.insert(
            hash(&token),
            Entry {
                signed: signed.clone(),
                last_seen: now,
            },
        );
        (token, signed)
    }

    /// The session of cookie token `token`, refreshing its idle timer.
    pub(crate) fn get(&self, token: &str) -> Option<Signed> {
        let mut live = self.live.lock().expect("sessions poisoned");
        let key = hash(token);
        let entry = live.get_mut(&key)?;
        if entry.last_seen.elapsed() >= IDLE {
            entry.signed.ended.cancel();
            live.remove(&key);
            return None;
        }
        entry.last_seen = Instant::now();
        Some(entry.signed.clone())
    }

    /// Signs the browser of `token` out, ending its sockets.
    pub(crate) fn remove(&self, token: &str) {
        let removed = self
            .live
            .lock()
            .expect("sessions poisoned")
            .remove(&hash(token));
        if let Some(entry) = removed {
            entry.signed.ended.cancel();
        }
    }

    /// Signs every browser out (`prometheus webapp --sign-out`), ending
    /// their sockets; returns how many sessions ended.
    pub(crate) fn sign_out_all(&self) -> usize {
        let ended: Vec<Entry> = self
            .live
            .lock()
            .expect("sessions poisoned")
            .drain()
            .map(|(_, e)| e)
            .collect();
        for entry in &ended {
            entry.signed.ended.cancel();
        }
        ended.len()
    }
}

/// The session token in a `Cookie` header.
pub(crate) fn token_of(cookie_header: &str) -> Option<&str> {
    cookie_header.split(';').find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == COOKIE && !value.is_empty()).then_some(value)
    })
}

/// The CSRF check: `presented` equals the session's token.
pub(crate) fn csrf_ok(signed: &Signed, presented: Option<&str>) -> bool {
    presented.is_some_and(|p| same(p, &signed.csrf))
}

/// The `Set-Cookie` value that signs a browser in.
pub(crate) fn set_cookie(token: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{secure}",
        IDLE.as_secs()
    )
}

/// The `Set-Cookie` value that clears it.
pub(crate) fn clear_cookie() -> String {
    format!("{COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::{IDLE, Role, Sessions, csrf_ok, set_cookie, token_of};

    #[tokio::test(start_paused = true)]
    async fn a_session_ends_after_twelve_idle_hours() {
        let sessions = Sessions::default();
        let (token, signed) = sessions.create("local", Role::Driver);
        tokio::time::advance(IDLE / 2).await;
        assert_eq!(sessions.get(&token).map(|s| s.csrf), Some(signed.csrf));
        tokio::time::advance(IDLE / 2).await;
        assert!(
            sessions.get(&token).is_some(),
            "the last request reset the timer"
        );
        tokio::time::advance(IDLE).await;
        assert!(sessions.get(&token).is_none());
        assert!(signed.ended.is_cancelled(), "its sockets end with it");
    }

    #[test]
    fn signing_everyone_out_ends_every_session_and_its_sockets() {
        let sessions = Sessions::default();
        let tabs: Vec<_> = (0..3)
            .map(|_| sessions.create("local", Role::Driver))
            .collect();
        assert_eq!(sessions.sign_out_all(), 3);
        for (token, signed) in &tabs {
            assert!(sessions.get(token).is_none());
            assert!(signed.ended.is_cancelled());
        }
    }

    #[test]
    fn the_cookie_is_http_only_strict_and_root_scoped() {
        let cookie = set_cookie("t0k", false);
        assert!(cookie.starts_with("prometheus_session=t0k; HttpOnly; SameSite=Strict; Path=/;"));
        assert!(!cookie.contains("Secure"));
        assert!(set_cookie("t0k", true).ends_with("; Secure"));
        assert_eq!(token_of("a=1; prometheus_session=t0k; b=2"), Some("t0k"));
        assert_eq!(token_of("prometheus_session="), None);
    }

    #[test]
    fn only_the_bound_csrf_token_passes() {
        let sessions = Sessions::default();
        let (_, signed) = sessions.create("local", Role::Driver);
        let (_, other) = sessions.create("local", Role::Driver);
        assert!(csrf_ok(&signed, Some(&signed.csrf)));
        assert!(!csrf_ok(&signed, Some(&other.csrf)));
        assert!(!csrf_ok(&signed, None));
    }
}
