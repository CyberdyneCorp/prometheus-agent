//! Signed-in browsers (spec: web-dashboard, Loopback authentication with
//! an `HttpOnly` session cookie; CSRF protection on state-changing requests;
//! Remote dashboard users are observers by default): the cookie's token
//! (kept as a hash) maps to the subject, its role and the CSRF token bound
//! to it, and expires after 12 h without a request.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

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

/// A signed-in browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Signed {
    /// Who: `local` for a login link, `password`, or an OIDC subject.
    pub subject: String,
    pub role: Role,
    /// The CSRF token every state-changing request must echo.
    pub csrf: String,
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
        };
        let now = Instant::now();
        let mut live = self.live.lock().expect("sessions poisoned");
        live.retain(|_, e| now.duration_since(e.last_seen) < IDLE);
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
            live.remove(&key);
            return None;
        }
        entry.last_seen = Instant::now();
        Some(entry.signed.clone())
    }

    /// Signs the browser of `token` out.
    pub(crate) fn remove(&self, token: &str) {
        self.live
            .lock()
            .expect("sessions poisoned")
            .remove(&hash(token));
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
        assert_eq!(sessions.get(&token), Some(signed));
        tokio::time::advance(IDLE / 2).await;
        assert!(
            sessions.get(&token).is_some(),
            "the last request reset the timer"
        );
        tokio::time::advance(IDLE).await;
        assert_eq!(sessions.get(&token), None);
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
