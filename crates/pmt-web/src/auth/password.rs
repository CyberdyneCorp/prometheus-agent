//! Password sign-in (spec: web-dashboard, Non-loopback binds fail closed,
//! "Password mode login"): the password is checked against the argon2id
//! PHC hash in the launch profile's `DASHBOARD_PASSWORD_HASH` secret, and
//! an address with 5 wrong passwords within 60 s is refused (429) for
//! 300 s.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Duration;

use argon2::{Argon2, PasswordVerifier as _};
use tokio::time::Instant;

/// Failures allowed within [`WINDOW`].
const MAX_FAILURES: usize = 5;
/// The window failures are counted in.
const WINDOW: Duration = Duration::from_mins(1);
/// How long an address is refused after too many failures.
pub(crate) const LOCKOUT: Duration = Duration::from_mins(5);

/// Whether `password` matches the PHC string `phc` (argon2id).
pub(crate) fn verify(password: &str, phc: &str) -> bool {
    phc.starts_with("$argon2id$")
        && Argon2::default()
            .verify_password(password.as_bytes(), phc)
            .is_ok()
}

#[derive(Debug, Default)]
struct Record {
    failures: Vec<Instant>,
    locked_until: Option<Instant>,
}

/// Wrong-password counts per client address.
#[derive(Debug, Default)]
pub(crate) struct Limiter {
    by_ip: Mutex<HashMap<IpAddr, Record>>,
}

impl Limiter {
    /// Reserves one attempt for `ip`: `Err(seconds)` while it is locked
    /// out. The attempt counts as a failure from the start, so concurrent
    /// requests cannot all pass before the first failure is recorded; a
    /// correct password clears it with [`Limiter::succeed`]. The attempt
    /// that reaches [`MAX_FAILURES`] still runs and starts the lockout.
    pub(crate) fn attempt(&self, ip: IpAddr) -> Result<(), u64> {
        let mut by_ip = self.by_ip.lock().expect("limiter poisoned");
        let now = Instant::now();
        if let Some(until) = by_ip.get(&ip).and_then(|r| r.locked_until)
            && until > now
        {
            return Err((until - now).as_secs().max(1));
        }
        // Addresses with nothing recent to remember are forgotten, so the
        // table stays as small as the current attackers.
        by_ip.retain(|_, r| {
            r.locked_until.is_some_and(|until| until > now)
                || r.failures.iter().any(|at| now.duration_since(*at) < WINDOW)
        });
        let record = by_ip.entry(ip).or_default();
        record
            .failures
            .retain(|at| now.duration_since(*at) < WINDOW);
        record.failures.push(now);
        if record.failures.len() >= MAX_FAILURES {
            record.locked_until = Some(now + LOCKOUT);
            record.failures.clear();
        }
        Ok(())
    }

    /// Forgets `ip`'s attempts after a correct password.
    pub(crate) fn succeed(&self, ip: IpAddr) {
        self.by_ip.lock().expect("limiter poisoned").remove(&ip);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::net::IpAddr;

    use argon2::{Argon2, PasswordHasher as _};

    use super::{LOCKOUT, Limiter, verify};

    /// An argon2id PHC hash of `password` (cheap parameters).
    pub(crate) fn phc(password: &str) -> String {
        let params = argon2::Params::new(1024, 1, 1, None).unwrap();
        let hasher = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        hasher
            .hash_password_with_salt(password.as_bytes(), b"saltsaltsaltsalt")
            .unwrap()
            .to_string()
    }

    #[test]
    fn only_the_right_password_verifies() {
        let hash = phc("hunter22");
        assert!(verify("hunter22", &hash));
        assert!(!verify("hunter23", &hash));
        assert!(!verify("hunter22", "plain text"));
    }

    /// Scenario: Password mode login (the lockout).
    #[tokio::test(start_paused = true)]
    async fn five_failures_in_a_minute_lock_the_address_out_for_five_minutes() {
        let limiter = Limiter::default();
        let ip: IpAddr = "192.0.2.7".parse().unwrap();
        let other: IpAddr = "192.0.2.8".parse().unwrap();
        for _ in 0..5 {
            assert_eq!(limiter.attempt(ip), Ok(()));
        }
        assert_eq!(limiter.attempt(ip), Err(300));
        assert_eq!(limiter.attempt(other), Ok(()));
        tokio::time::advance(LOCKOUT).await;
        assert_eq!(limiter.attempt(ip), Ok(()));
    }

    #[tokio::test(start_paused = true)]
    async fn failures_spread_over_more_than_a_minute_do_not_lock() {
        let limiter = Limiter::default();
        let ip: IpAddr = "192.0.2.7".parse().unwrap();
        for _ in 0..8 {
            assert_eq!(limiter.attempt(ip), Ok(()));
            tokio::time::advance(std::time::Duration::from_secs(16)).await;
        }
        assert_eq!(limiter.attempt(ip), Ok(()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_correct_password_forgets_the_attempts() {
        let limiter = Limiter::default();
        let ip: IpAddr = "192.0.2.7".parse().unwrap();
        for _ in 0..4 {
            assert_eq!(limiter.attempt(ip), Ok(()));
        }
        limiter.succeed(ip);
        for _ in 0..5 {
            assert_eq!(limiter.attempt(ip), Ok(()));
        }
        assert_eq!(limiter.attempt(ip), Err(300));
    }
}
