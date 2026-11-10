//! Per-turn memory notifications (spec: memory, Per-turn memory
//! notifications): after an agent or background-review memory change
//! commits, or a skill the agent manages changes, the session's surface
//! shows one line per `memory.notifications` (`on`: `Memory updated`;
//! `verbose`: a compact preview per change; `off`: nothing).
//!
//! Only committed revisions are announced: staged proposals are not
//! revisions and a refused batch writes none. The sink is bound late
//! because the surface that shows the line (the line interface, a daemon
//! session's clients, a messaging chat) exists only after the session's
//! memory does; a notice before that is dropped.

use std::sync::{Arc, Mutex, PoisonError};

use pmt_config::schema::MemoryNotifications;
use pmt_store::{Operation, Revision};

/// Where a session's notices are shown.
pub trait NoticeSink: Send + Sync {
    /// Shows `text` once.
    fn show(&self, text: String);
}

/// The longest preview of an entry, in characters.
const PREVIEW_CHARS: usize = 80;

/// One session's notifier: the mode it was opened with and its sink.
#[derive(Clone)]
pub struct Notices {
    mode: MemoryNotifications,
    sink: Arc<Mutex<Option<Arc<dyn NoticeSink>>>>,
}

impl Notices {
    /// A notifier in `mode` with no sink yet.
    #[must_use]
    pub fn new(mode: MemoryNotifications) -> Self {
        Self {
            mode,
            sink: Arc::default(),
        }
    }

    /// The mode.
    #[must_use]
    pub fn mode(&self) -> MemoryNotifications {
        self.mode
    }

    /// Shows later notices on `sink` (replacing any earlier one).
    pub fn bind(&self, sink: Arc<dyn NoticeSink>) {
        *self.sink.lock().unwrap_or_else(PoisonError::into_inner) = Some(sink);
    }

    fn show(&self, text: String) {
        let sink = self
            .sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(sink) = sink {
            sink.show(text);
        } else {
            tracing::debug!("memory notice dropped (no surface yet): {text}");
        }
    }

    /// Announces the committed `revisions` of one write.
    pub fn memory(&self, revisions: &[Revision]) {
        if let Some(text) = memory_text(self.mode, revisions) {
            self.show(text);
        }
    }

    /// Announces a committed change of skill `name` (`action` as the
    /// skill lifecycle names it: `create`, `edit` or `delete`).
    pub fn skill(&self, name: &str, action: &str) {
        if let Some(text) = skill_text(self.mode, name, action) {
            self.show(text);
        }
    }
}

/// The notice of `revisions` in `mode`; `None` when there is nothing to
/// say.
#[must_use]
pub fn memory_text(mode: MemoryNotifications, revisions: &[Revision]) -> Option<String> {
    if revisions.is_empty() {
        return None;
    }
    match mode {
        MemoryNotifications::Off => None,
        MemoryNotifications::On => Some("Memory updated".to_owned()),
        MemoryNotifications::Verbose => {
            let lines: Vec<String> = revisions.iter().map(preview).collect();
            Some(lines.join("\n"))
        }
    }
}

/// The notice of a skill change in `mode`.
#[must_use]
pub fn skill_text(mode: MemoryNotifications, name: &str, action: &str) -> Option<String> {
    let done = match action {
        "create" => "created",
        "delete" => "deleted",
        _ => "patched",
    };
    (mode != MemoryNotifications::Off).then(|| format!("Skill '{name}' {done}"))
}

/// `Memory + <added text>`, `Memory ~ <new text>`, `Memory - <removed
/// text>`, for either store.
fn preview(revision: &Revision) -> String {
    let (sign, entry) = match revision.operation {
        Operation::Add => ('+', revision.after.as_ref()),
        Operation::Patch | Operation::Update => ('~', revision.after.as_ref()),
        Operation::Remove => ('-', revision.before.as_ref()),
    };
    let text = entry.map(|e| e.content.as_str()).unwrap_or_default();
    format!("Memory {sign} {}", shorten(text))
}

/// `text` on one line, cut to [`PREVIEW_CHARS`].
fn shorten(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= PREVIEW_CHARS {
        return line;
    }
    let cut: String = line.chars().take(PREVIEW_CHARS - 1).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pmt_store::{MemoryEntry, MemoryOrigin, MemoryTarget};

    fn entry(content: &str) -> MemoryEntry {
        let now = jiff::Timestamp::now();
        MemoryEntry {
            id: "m_abcdefghijkl".to_owned(),
            target: MemoryTarget::User,
            kind: pmt_store::MemoryKind::Preference,
            tier: pmt_store::MemoryTier::Core,
            content: content.to_owned(),
            pinned: false,
            status: pmt_store::MemoryStatus::Active,
            expires_at: None,
            created_at: now,
            updated_at: now,
            last_recalled_at: None,
            recall_count: 0,
            origin: MemoryOrigin::BackgroundReview,
            origin_session: None,
        }
    }

    fn added(content: &str) -> Revision {
        Revision {
            rev: 1,
            entry_id: "m_abcdefghijkl".to_owned(),
            target: MemoryTarget::User,
            operation: Operation::Add,
            before: None,
            after: Some(entry(content)),
            origin: MemoryOrigin::BackgroundReview,
            session_id: None,
            at: jiff::Timestamp::now(),
            reviewed: false,
        }
    }

    #[test]
    fn each_mode_says_what_it_promises() {
        let revisions = [added("User prefers terse replies")];
        assert_eq!(
            memory_text(MemoryNotifications::On, &revisions).as_deref(),
            Some("Memory updated")
        );
        assert_eq!(
            memory_text(MemoryNotifications::Verbose, &revisions).as_deref(),
            Some("Memory + User prefers terse replies")
        );
        assert_eq!(memory_text(MemoryNotifications::Off, &revisions), None);
        assert_eq!(memory_text(MemoryNotifications::Verbose, &[]), None);
    }

    #[test]
    fn a_long_entry_is_previewed_on_one_short_line() {
        let long = format!("first line\n{}", "x".repeat(200));
        let text = memory_text(MemoryNotifications::Verbose, &[added(&long)]).unwrap();
        assert!(!text.contains('\n'), "{text}");
        assert!(text.ends_with('…'), "{text}");
        assert!(text.chars().count() < 100, "{text}");
    }

    #[test]
    fn skill_changes_are_named() {
        assert_eq!(
            skill_text(MemoryNotifications::On, "foo", "edit").as_deref(),
            Some("Skill 'foo' patched")
        );
        assert_eq!(skill_text(MemoryNotifications::Off, "foo", "create"), None);
    }

    #[test]
    fn a_notice_reaches_the_sink_bound_later() {
        struct Collect(Mutex<Vec<String>>);
        impl NoticeSink for Collect {
            fn show(&self, text: String) {
                self.0.lock().unwrap().push(text);
            }
        }
        let notices = Notices::new(MemoryNotifications::On);
        notices.memory(&[added("dropped: nobody listens yet")]);
        let sink = Arc::new(Collect(Mutex::default()));
        notices.clone().bind(Arc::clone(&sink) as _);
        notices.memory(&[added("shown")]);
        assert_eq!(*sink.0.lock().unwrap(), vec!["Memory updated".to_owned()]);
    }
}
