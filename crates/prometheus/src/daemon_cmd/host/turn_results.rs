//! How recent turns ended, by turn id, for code in the daemon that submits
//! a turn and needs its reply (a relayed bot message): the result is kept
//! a few minutes, so a turn that ends before anyone waits is still found.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use pmt_rpc::contracts::TurnFinished;
use tokio::sync::oneshot;

/// How long a result is kept.
const KEEP: Duration = Duration::from_mins(10);

/// A turn's reply, or why it produced none.
pub(super) type Ended = Result<String, String>;

#[derive(Default)]
struct Results {
    done: HashMap<String, (Instant, Ended)>,
    waiters: HashMap<String, Vec<oneshot::Sender<Ended>>>,
}

static RESULTS: LazyLock<Mutex<Results>> = LazyLock::new(Mutex::default);

/// Records how turn `finished.turn_id` ended and wakes its waiters.
pub(super) fn record(finished: &TurnFinished) {
    let ended = match (&finished.final_response, finished.exit_reason.as_str()) {
        (Some(reply), "completed" | "iteration_limit") => Ok(reply.clone()),
        _ => Err(finished.error.as_ref().map_or_else(
            || format!("the turn ended: {}", finished.exit_reason),
            |e| e.message.clone(),
        )),
    };
    let mut results = RESULTS.lock().expect("turn results poisoned");
    let now = Instant::now();
    results
        .done
        .retain(|_, (at, _)| now.duration_since(*at) < KEEP);
    for waiter in results
        .waiters
        .remove(&finished.turn_id)
        .unwrap_or_default()
    {
        let _ = waiter.send(ended.clone());
    }
    results.done.insert(finished.turn_id.clone(), (now, ended));
}

/// Waits up to `timeout` for turn `turn_id` to end; `None` when it did
/// not.
pub(super) async fn wait(turn_id: &str, timeout: Duration) -> Option<Ended> {
    let rx = {
        let mut results = RESULTS.lock().expect("turn results poisoned");
        if let Some((_, ended)) = results.done.get(turn_id) {
            return Some(ended.clone());
        }
        let (tx, rx) = oneshot::channel();
        results
            .waiters
            .entry(turn_id.to_owned())
            .or_default()
            .push(tx);
        rx
    };
    let ended = tokio::time::timeout(timeout, rx)
        .await
        .ok()
        .and_then(Result::ok);
    if ended.is_none() {
        let mut results = RESULTS.lock().expect("turn results poisoned");
        if let Some(waiters) = results.waiters.get_mut(turn_id) {
            waiters.retain(|w| !w.is_closed());
        }
    }
    ended
}
