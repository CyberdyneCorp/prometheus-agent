//! IPv6 and IPv4 raced for every outbound connection (spec:
//! model-providers, Outbound IPv4 preference; workspace-architecture, IPv6
//! reachability check and IPv4-only outbound).
//!
//! A dual-stack host gets one IPv6 attempt first and one IPv4 attempt
//! [`IPV4_DELAY`] later (at once if the IPv6 attempt already failed). The
//! family that answers first is handed to the connector first, so a host
//! whose IPv6 address is blackholed costs 250 ms instead of a connect
//! timeout. The race runs where names are resolved, which does not know
//! the request's port: it probes [`RACE_PORT`], and a refused connection
//! counts as an answer (the path works; only the port is closed). With
//! `network.force_ipv4` the resolver keeps only IPv4 addresses and nothing
//! is raced.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// The head start of the IPv6 attempt.
pub const IPV4_DELAY: Duration = Duration::from_millis(250);

/// The port the race probes (outbound clients speak HTTPS).
pub const RACE_PORT: u16 = 443;

/// How long the race waits for either family; past it the addresses keep
/// IPv6 first and the connector's own fallback applies.
pub const RACE_LIMIT: Duration = Duration::from_secs(1);

/// `addrs` with the family that answered first leading; unchanged when
/// they hold only one family.
pub async fn order<C, F>(addrs: Vec<IpAddr>, attempt: C) -> Vec<IpAddr>
where
    C: Fn(IpAddr) -> F,
    F: Future<Output = bool>,
{
    let (v6, v4): (Vec<IpAddr>, Vec<IpAddr>) = addrs.iter().partition(|ip| ip.is_ipv6());
    let (Some(&first6), Some(&first4)) = (v6.first(), v4.first()) else {
        return addrs;
    };
    let ipv4_first = matches!(
        tokio::time::timeout(RACE_LIMIT, race(attempt(first6), attempt, first4)).await,
        Ok(Some(Family::V4))
    );
    if ipv4_first {
        v4.into_iter().chain(v6).collect()
    } else {
        v6.into_iter().chain(v4).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    V6,
    V4,
}

/// Which family answers first: the IPv6 attempt runs from the start, the
/// IPv4 one from [`IPV4_DELAY`] (or as soon as IPv6 failed); `None` when
/// both fail.
async fn race<C, F>(v6: F, attempt: C, first4: IpAddr) -> Option<Family>
where
    C: Fn(IpAddr) -> F,
    F: Future<Output = bool>,
{
    let mut v6 = std::pin::pin!(v6);
    let head_start = tokio::time::sleep(IPV4_DELAY);
    tokio::select! {
        answered = &mut v6 => {
            if answered {
                return Some(Family::V6);
            }
            return attempt(first4).await.then_some(Family::V4);
        }
        () = head_start => {}
    }
    tracing::debug!(component = "net", "IPv6 slow to answer: racing IPv4");
    let mut v4 = std::pin::pin!(attempt(first4));
    tokio::select! {
        answered = &mut v6 => {
            if answered {
                Some(Family::V6)
            } else {
                v4.await.then_some(Family::V4)
            }
        }
        answered = &mut v4 => {
            if answered {
                Some(Family::V4)
            } else {
                v6.await.then_some(Family::V6)
            }
        }
    }
}

/// Whether `ip` answers a TCP connection on [`RACE_PORT`] (accepted or
/// refused).
pub async fn answers(ip: IpAddr) -> bool {
    match tokio::net::TcpStream::connect(SocketAddr::new(ip, RACE_PORT)).await {
        Ok(_) => true,
        Err(err) => err.kind() == io::ErrorKind::ConnectionRefused,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::time::Instant;

    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// An attempt that never answers over IPv6 and answers at once over
    /// IPv4, recording when each started.
    type Attempt = std::pin::Pin<Box<dyn Future<Output = bool> + Send>>;

    fn blackholed_v6(
        started: &Arc<Mutex<Vec<(IpAddr, Duration)>>>,
        t0: Instant,
    ) -> impl Fn(IpAddr) -> Attempt + use<> {
        let started = Arc::clone(started);
        move |ip: IpAddr| {
            started.lock().unwrap().push((ip, t0.elapsed()));
            Box::pin(async move {
                if ip.is_ipv6() {
                    std::future::pending::<()>().await;
                }
                true
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ipv4_starts_250_ms_after_a_silent_ipv6_attempt_and_leads() {
        let started = Arc::new(Mutex::new(Vec::new()));
        let t0 = Instant::now();
        let ordered = order(
            vec![ip("100::1"), ip("127.0.0.1")],
            blackholed_v6(&started, t0),
        )
        .await;
        assert_eq!(ordered, vec![ip("127.0.0.1"), ip("100::1")]);
        let started = started.lock().unwrap();
        assert_eq!(started[0], (ip("100::1"), Duration::ZERO));
        assert_eq!(started[1], (ip("127.0.0.1"), IPV4_DELAY));
    }

    #[tokio::test(start_paused = true)]
    async fn a_prompt_ipv6_answer_keeps_ipv6_first_without_an_ipv4_attempt() {
        let started = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&started);
        let ordered = order(vec![ip("127.0.0.1"), ip("::1")], move |ip: IpAddr| {
            log.lock().unwrap().push(ip);
            async { true }
        })
        .await;
        assert_eq!(ordered, vec![ip("::1"), ip("127.0.0.1")]);
        assert_eq!(*started.lock().unwrap(), vec![ip("::1")]);
    }

    #[tokio::test(start_paused = true)]
    async fn one_family_is_not_raced() {
        let addrs = vec![ip("127.0.0.1"), ip("127.0.0.2")];
        let calls = Arc::new(Mutex::new(0));
        let counted = Arc::clone(&calls);
        let ordered = order(addrs.clone(), move |_| {
            *counted.lock().unwrap() += 1;
            async { true }
        })
        .await;
        assert_eq!(ordered, addrs);
        assert_eq!(*calls.lock().unwrap(), 0);
    }
}
