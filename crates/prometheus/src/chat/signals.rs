//! SIGINT and SIGTERM while a non-interactive run works: the turn is
//! stopped (its partial transcript kept) and the process exits 130 or 143
//! (spec: cli, Stable exit codes).

/// The exit status of the next SIGINT (130) or SIGTERM (143).
#[cfg(unix)]
pub(super) async fn next() -> u8 {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut int), Ok(mut term)) = (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = int.recv() => super::oneshot::INTERRUPTED,
        _ = term.recv() => super::oneshot::TERMINATED,
    }
}

/// The exit status of the next Ctrl-C (130).
#[cfg(not(unix))]
pub(super) async fn next() -> u8 {
    if tokio::signal::ctrl_c().await.is_err() {
        return std::future::pending().await;
    }
    super::oneshot::INTERRUPTED
}
