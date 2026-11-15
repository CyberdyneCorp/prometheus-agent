//! The kanban dispatcher and notifier inside the gateway daemon (spec:
//! kanban, Single dispatcher per home; Notifications through the owning
//! profile; Subscription delivery modes and inheritance). With
//! `kanban.dispatch = true` (the launch profile's) the daemon is the
//! home's dispatcher: it ticks every `kanban.dispatch_interval_secs` and
//! renews its board leases every 20 s in between; a paused home starts no
//! work. Notifications of every board go out through the subscriber
//! profile's own adapter under that profile; a route that profile cannot
//! reach is skipped with one warning per subscription, never sent
//! through another profile. A `wake` leg starts one turn of the
//! subscriber profile's agent in the chat's session with the event's
//! summary; a wake the chat cannot take now is retried on later notifier
//! ticks, its passive message never sent twice.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_kanban::Boards;
use pmt_kanban::lease::RENEW_MS;
use pmt_kanban::notify::{Leg, Subscription};

use super::DaemonHost;

/// How often the loop wakes.
const WAKE: Duration = Duration::from_secs(1);
/// How often notifications are looked for.
const NOTIFY_EVERY: Duration = Duration::from_secs(2);

/// Runs until the daemon stops.
pub(in crate::daemon_cmd) async fn schedule(host: Arc<DaemonHost>) {
    let Ok(home) = host
        .profile(&host.launch_profile)
        .map(|p| p.scope().home().to_path_buf())
    else {
        return;
    };
    let renew_every = Duration::from_millis(u64::try_from(RENEW_MS).unwrap_or(20_000));
    let mut last_tick: Option<Instant> = None;
    let mut last_renew = Instant::now();
    let mut last_notify = Instant::now();
    loop {
        tokio::time::sleep(WAKE).await;
        let Ok(scope) = host.profile(&host.launch_profile).map(|p| p.scope()) else {
            continue;
        };
        let config = scope.config().kanban.clone();
        if config.dispatch && pmt_core::pause::check(&home).is_ok() {
            let interval = Duration::from_secs(config.dispatch_interval_secs.max(1));
            if last_tick.is_none_or(|t| t.elapsed() >= interval) {
                tick(&host, scope.clone()).await;
                last_tick = Some(Instant::now());
                last_renew = Instant::now();
            } else if last_renew.elapsed() >= renew_every {
                let home = home.clone();
                let _ =
                    pmt_core::spawn::blocking(move || crate::kanban_host::renew(&home, "daemon"))
                        .await;
                last_renew = Instant::now();
            }
        }
        if last_notify.elapsed() >= NOTIFY_EVERY {
            notify(&host, &home).await;
            last_notify = Instant::now();
        }
    }
}

async fn tick(host: &DaemonHost, scope: pmt_config::ProfileScope) {
    let launch = host.launch.clone();
    let result =
        pmt_core::spawn::blocking(move || crate::kanban_host::tick(&scope, "daemon", &launch))
            .await;
    match result {
        Ok(Ok(summaries)) => {
            for s in summaries
                .iter()
                .filter(|s| !s.claimed.is_empty() || !s.reclaimed.is_empty())
            {
                tracing::info!(component = "kanban", board = s.board.as_str(), claimed = ?s.claimed, reclaimed = ?s.reclaimed, "kanban tick");
            }
        }
        Ok(Err(err)) => tracing::debug!(component = "kanban", "kanban tick: {err}"),
        Err(err) => tracing::warn!(component = "kanban", "kanban tick: {err}"),
    }
}

/// One pending notification leg.
struct Pending {
    board: String,
    sub: Subscription,
    event: i64,
    leg: Leg,
    text: String,
}

fn pending(home: &Path) -> Vec<Pending> {
    let Ok(boards) = Boards::open(home) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for slug in boards.live().unwrap_or_default() {
        let Ok(board) = boards.board(&slug) else {
            continue;
        };
        for pending in board.pending_notifications().unwrap_or_default() {
            let event = pending.event;
            let Some(task) = event.task_id.as_deref().and_then(|id| board.task(id).ok()) else {
                continue;
            };
            out.push(Pending {
                text: crate::kanban_host::notice::text(&slug, &task, &event),
                board: slug.clone(),
                sub: pending.sub,
                event: event.id,
                leg: pending.leg,
            });
        }
    }
    out
}

async fn notify(host: &DaemonHost, home: &Path) {
    let home_dir: PathBuf = home.to_path_buf();
    let Ok(items) = pmt_core::spawn::blocking(move || pending(&home_dir)).await else {
        return;
    };
    if items.is_empty() {
        return;
    }
    let Some(gateway) = host.gateway.get() else {
        return;
    };
    for item in items {
        let outcome = match item.leg {
            Leg::Notify => deliver(host, gateway, &item).await.map(|()| true),
            Leg::Wake => wake(host, gateway, &item).await,
        };
        let home = home.to_path_buf();
        let _ = pmt_core::spawn::blocking(move || settle(&home, &item, outcome)).await;
    }
}

/// Starts the subscriber agent's turn for a wake leg: `Ok(false)` when the
/// chat cannot take it now (retried on a later tick), `Err` when the
/// subscriber profile can never reach the chat.
async fn wake(
    host: &DaemonHost,
    gateway: &pmt_gateway::messaging::Gateway,
    item: &Pending,
) -> Result<bool, String> {
    let sub = &item.sub;
    let profile = sub.profile.as_str();
    if host.profile(profile).is_err() {
        return Err(format!(
            "profile '{profile}' is not served by this daemon; serve it to wake its agent for kanban events"
        ));
    }
    let session = sub.session_id.clone().or_else(|| {
        gateway.session_of_chat(
            profile,
            &sub.adapter,
            sub.chat_type.as_deref(),
            &sub.chat,
            sub.thread.as_deref(),
        )
    });
    let Some(session) = session else {
        // The chat has no conversation yet: try again later.
        return Ok(false);
    };
    let (started, ran) = tokio::sync::oneshot::channel();
    let automated = pmt_gateway::messaging::automation::Automated {
        origin: "kanban".to_owned(),
        text: format!(
            "[kanban notification] {}\nReact if it needs you (inspect it with the kanban tools or `prometheus kanban show`), otherwise answer briefly.",
            item.text
        ),
        notice: None,
        started: Some(started),
    };
    if gateway.automate(profile, &session, automated)
        == pmt_gateway::messaging::adapter::Admission::Dropped
    {
        return Ok(false);
    }
    Ok(tokio::time::timeout(Duration::from_secs(10), ran)
        .await
        .is_ok_and(|r| r.unwrap_or(false)))
}

/// Sends one notification through its subscriber profile; `Err` names
/// why that profile cannot reach the route.
async fn deliver(
    host: &DaemonHost,
    gateway: &pmt_gateway::messaging::Gateway,
    item: &Pending,
) -> Result<(), String> {
    let profile = item.sub.profile.as_str();
    if host.profile(profile).is_err() {
        return Err(format!(
            "profile '{profile}' is not served by this daemon; serve it (`prometheus daemon run`) to deliver its kanban notifications"
        ));
    }
    let mut target = format!("{}:{}", item.sub.adapter, item.sub.chat);
    if let Some(thread) = &item.sub.thread {
        target = format!("{target}:{thread}");
    }
    let to = gateway
        .resolve_target(profile, &target)
        .await
        .map_err(|err| {
            format!(
                "{} (enable the {} adapter for profile '{profile}', or unsubscribe {})",
                err.message(),
                item.sub.adapter,
                item.sub.id
            )
        })?;
    gateway
        .enqueue_notifying(profile, &to, &item.text, &[], true)
        .map(|_| ())
        .map_err(|err| err.message().to_owned())
}

/// Checkpoints a delivered (or never deliverable) leg; a wake the chat
/// could not take yet (`Ok(false)`) stays pending.
fn settle(home: &Path, item: &Pending, outcome: Result<bool, String>) {
    if matches!(outcome, Ok(false)) {
        return;
    }
    let Ok(mut board) = Boards::open(home).and_then(|b| b.board(&item.board)) else {
        return;
    };
    if let Err(reason) = outcome
        && board.warn_once(&item.sub.id).unwrap_or(false)
    {
        tracing::warn!(
            component = "kanban",
            subscription = item.sub.id.as_str(),
            task = item.sub.task_id.as_str(),
            "kanban notification skipped: {reason}"
        );
    }
    let _ = board.checkpoint(&item.sub.id, item.leg, item.event);
}
