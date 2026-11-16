//! Live child transcripts (spec: tui, Split panes for subagents and tool
//! output): what each child said and which tools it ran, as it happens.
//! A client watching a child subscribes and gets what the child did so
//! far, then every new item, until the child finishes. The items are
//! the client's view only; they never reach a model's context.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock, PoisonError};

use tokio::sync::mpsc;

/// Items kept per child for subscribers that come later.
const KEPT_ITEMS: usize = 512;
/// Finished children whose transcripts stay readable.
const KEPT_FINISHED: usize = 64;
/// The longest tool output an item carries, in bytes.
const MAX_OUTPUT: usize = 64 * 1024;

/// One item of a child's transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildItem {
    /// Its assistant text.
    Assistant(String),
    /// A tool call: the tool and what it works on.
    ToolCall {
        /// The tool.
        tool: String,
        /// Its path, command or query.
        hint: String,
    },
    /// A tool call's output (or error).
    ToolResult {
        /// The tool.
        tool: String,
        /// The output or the error message.
        output: String,
    },
    /// It ended with this status.
    Finished(String),
}

/// What a subscription starts with.
#[derive(Debug)]
pub struct Subscription {
    /// How the child ended, `None` while it runs.
    pub status: Option<String>,
    /// Its transcript so far.
    pub backlog: Vec<ChildItem>,
    /// What follows; `None` once it has finished.
    pub live: Option<mpsc::UnboundedReceiver<ChildItem>>,
}

#[derive(Debug, Default)]
struct Feed {
    items: VecDeque<ChildItem>,
    subscribers: Vec<mpsc::UnboundedSender<ChildItem>>,
    status: Option<String>,
}

/// Every child's feed in this process.
#[derive(Debug, Default)]
pub struct Feeds {
    feeds: Mutex<HashMap<String, Feed>>,
    finished: Mutex<VecDeque<String>>,
}

impl Feeds {
    /// The process's feeds.
    #[must_use]
    pub fn process() -> &'static Self {
        static FEEDS: OnceLock<Feeds> = OnceLock::new();
        FEEDS.get_or_init(Self::default)
    }

    fn feeds(&self) -> std::sync::MutexGuard<'_, HashMap<String, Feed>> {
        self.feeds.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts `subagent_id`'s feed (before its first item).
    pub fn open(&self, subagent_id: &str) {
        self.feeds().entry(subagent_id.to_owned()).or_default();
    }

    /// Adds an item and sends it to the child's subscribers.
    pub fn push(&self, subagent_id: &str, item: ChildItem) {
        let item = capped(item);
        let mut feeds = self.feeds();
        let feed = feeds.entry(subagent_id.to_owned()).or_default();
        if feed.items.len() == KEPT_ITEMS {
            feed.items.pop_front();
        }
        feed.subscribers.retain(|tx| tx.send(item.clone()).is_ok());
        feed.items.push_back(item);
    }

    /// The child ended with `status`: its subscribers get that last item
    /// and their streams end.
    pub fn finish(&self, subagent_id: &str, status: &str) {
        self.push(subagent_id, ChildItem::Finished(status.to_owned()));
        if let Some(feed) = self.feeds().get_mut(subagent_id) {
            feed.status = Some(status.to_owned());
            feed.subscribers.clear();
        }
        let evicted = {
            let mut finished = self.finished.lock().unwrap_or_else(PoisonError::into_inner);
            finished.push_back(subagent_id.to_owned());
            (finished.len() > KEPT_FINISHED)
                .then(|| finished.pop_front())
                .flatten()
        };
        if let Some(old) = evicted {
            self.feeds().remove(&old);
        }
    }

    /// Subscribes to `subagent_id`; `None` for a child this process does
    /// not know (or forgot).
    #[must_use]
    pub fn subscribe(&self, subagent_id: &str) -> Option<Subscription> {
        let mut feeds = self.feeds();
        let feed = feeds.get_mut(subagent_id)?;
        let live = feed.status.is_none().then(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            feed.subscribers.push(tx);
            rx
        });
        Some(Subscription {
            status: feed.status.clone(),
            backlog: feed.items.iter().cloned().collect(),
            live,
        })
    }
}

/// `item` with a tool output cut to [`MAX_OUTPUT`] bytes.
fn capped(item: ChildItem) -> ChildItem {
    match item {
        ChildItem::ToolResult { tool, mut output } if output.len() > MAX_OUTPUT => {
            let mut end = MAX_OUTPUT;
            while !output.is_char_boundary(end) {
                end -= 1;
            }
            output.truncate(end);
            output.push_str("\n[output cut]");
            ChildItem::ToolResult { tool, output }
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::{ChildItem, Feeds};

    #[tokio::test]
    async fn a_late_subscriber_gets_the_backlog_then_live_items_until_the_end() {
        let feeds = Feeds::default();
        feeds.open("sa-1");
        feeds.push("sa-1", ChildItem::Assistant("looking".into()));
        let mut sub = feeds.subscribe("sa-1").unwrap();
        assert_eq!(sub.backlog, [ChildItem::Assistant("looking".into())]);
        assert_eq!(sub.status, None);
        let call = ChildItem::ToolCall {
            tool: "read_file".into(),
            hint: "a.txt".into(),
        };
        feeds.push("sa-1", call.clone());
        feeds.finish("sa-1", "completed");
        let live = sub.live.as_mut().unwrap();
        assert_eq!(live.recv().await, Some(call));
        assert_eq!(
            live.recv().await,
            Some(ChildItem::Finished("completed".into()))
        );
        assert_eq!(live.recv().await, None);
        let after = feeds.subscribe("sa-1").unwrap();
        assert_eq!(after.status.as_deref(), Some("completed"));
        assert!(after.live.is_none());
        assert!(feeds.subscribe("sa-2").is_none());
    }
}
