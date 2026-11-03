//! Where a webhook run's answer goes (spec: messaging-gateway, Webhook
//! route delivery and safe default toolset): `origin` answers the HTTP
//! caller that is still waiting (or the route's log once it gave up
//! waiting), `log` records it, `github_comment` comments on the payload's
//! pull request or issue, and a send target (plus every `deliver_extra`)
//! gets it through the gateway, mirrored into that chat's session when
//! the route asks for it.

use std::collections::HashMap;
use std::sync::Mutex;

use pmt_gateway::messaging::SendError;
use serde_json::{Value, json};
use tokio::sync::oneshot;

/// The pull request or issue a payload is about: `owner/repo` and its
/// number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GithubThread {
    pub(crate) repo: String,
    pub(crate) number: u64,
}

impl GithubThread {
    /// From a GitHub payload (`repository.full_name` with
    /// `pull_request.number`, `issue.number` or `number`).
    pub(crate) fn of(payload: &Value) -> Option<Self> {
        let repo = payload["repository"]["full_name"].as_str()?.to_owned();
        let number = payload["pull_request"]["number"]
            .as_u64()
            .or_else(|| payload["issue"]["number"].as_u64())
            .or_else(|| payload["number"].as_u64())?;
        Some(Self { repo, number })
    }
}

/// One delivery whose run has not answered yet.
#[derive(Debug, Default)]
struct Open {
    /// The HTTP caller waiting for the answer (`origin`).
    waiting: Option<oneshot::Sender<String>>,
    github: Option<GithubThread>,
}

/// The deliveries whose answers are still to come, by delivery id.
#[derive(Debug, Default)]
pub(crate) struct Deliveries(Mutex<HashMap<String, Open>>);

impl Deliveries {
    /// Opens `delivery`; with `origin` the receiver the HTTP caller waits
    /// on.
    pub(crate) fn open(
        &self,
        delivery: &str,
        origin: bool,
        github: Option<GithubThread>,
    ) -> Option<oneshot::Receiver<String>> {
        let (tx, rx) = oneshot::channel();
        let waiting = origin.then_some(tx);
        self.0
            .lock()
            .expect("deliveries poisoned")
            .insert(delivery.to_owned(), Open { waiting, github });
        origin.then_some(rx)
    }

    /// The waiting HTTP caller of `delivery`, taken (one answer only).
    pub(crate) fn waiting(&self, delivery: &str) -> Option<oneshot::Sender<String>> {
        self.0
            .lock()
            .expect("deliveries poisoned")
            .get_mut(delivery)
            .and_then(|o| o.waiting.take())
    }

    /// The pull request or issue of `delivery`.
    pub(crate) fn github(&self, delivery: &str) -> Option<GithubThread> {
        self.0
            .lock()
            .expect("deliveries poisoned")
            .get(delivery)
            .and_then(|o| o.github.clone())
    }

    /// Forgets `delivery` (its caller stopped waiting).
    pub(crate) fn give_up(&self, delivery: &str) {
        if let Some(open) = self
            .0
            .lock()
            .expect("deliveries poisoned")
            .get_mut(delivery)
        {
            open.waiting = None;
        }
    }
}

/// Posts `body` as a comment on `thread` with `token`.
pub(crate) async fn comment(
    client: &reqwest::Client,
    api: &str,
    token: &str,
    thread: &GithubThread,
    body: &str,
) -> Result<(), SendError> {
    let url = format!(
        "{}/repos/{}/issues/{}/comments",
        api.trim_end_matches('/'),
        thread.repo,
        thread.number
    );
    let response = client
        .post(url)
        .bearer_auth(token)
        .header("accept", "application/vnd.github+json")
        .header("user-agent", "prometheus-agent")
        .header("content-type", "application/json")
        .body(json!({"body": body}).to_string())
        .send()
        .await
        .map_err(|e| SendError::Network(e.without_url().to_string()))?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    match status.as_u16() {
        429 => Err(SendError::RateLimited { retry_after: None }),
        500.. => Err(SendError::Server(format!("GitHub answered {status}"))),
        _ => Err(SendError::Permanent(format!(
            "GitHub refused the comment: {status}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_thread_comes_from_a_pull_request_or_issue_payload() {
        let pr = json!({"repository": {"full_name": "acme/app"}, "pull_request": {"number": 42}});
        assert_eq!(
            GithubThread::of(&pr),
            Some(GithubThread {
                repo: "acme/app".into(),
                number: 42
            })
        );
        let issue = json!({"repository": {"full_name": "acme/app"}, "issue": {"number": 7}});
        assert_eq!(GithubThread::of(&issue).unwrap().number, 7);
        assert_eq!(GithubThread::of(&json!({"number": 1})), None);
    }

    #[test]
    fn an_origin_answer_reaches_its_waiting_caller_once() {
        let open = Deliveries::default();
        let mut rx = open.open("d1", true, None).unwrap();
        open.waiting("d1").unwrap().send("done".into()).unwrap();
        assert_eq!(rx.try_recv().unwrap(), "done");
        assert!(open.waiting("d1").is_none(), "answered once");
        assert!(open.open("d2", false, None).is_none());
    }
}
