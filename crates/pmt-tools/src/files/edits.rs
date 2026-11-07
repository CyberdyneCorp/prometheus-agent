//! The seam through which file-tool changes are recorded (spec:
//! execution-environments, File snapshots and undo; Snapshot triggers,
//! caps and per-run toggle).
//!
//! Before `write_file` or `patch` changes a file, [`EditRecorder::before_change`]
//! runs (it may checkpoint the file's project); after the change,
//! [`EditRecorder::changed`] gets the prior and new content. The model
//! call behind the change is the [`EditCall`] the tool handler set with
//! [`with_call`]: file operations run synchronously on one blocking
//! thread, so a thread-local scope identifies the call exactly.

use std::cell::RefCell;
use std::path::Path;

/// The model call a file change belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditCall {
    /// The model's call id.
    pub call_id: String,
    /// The tool (`write_file`, `patch`).
    pub tool: String,
}

thread_local! {
    static CURRENT: RefCell<Option<EditCall>> = const { RefCell::new(None) };
}

/// Runs `operation` with `call` as the current call of this thread.
pub fn with_call<R>(call: EditCall, operation: impl FnOnce() -> R) -> R {
    let previous = CURRENT.with(|current| current.replace(Some(call)));
    let result = operation();
    CURRENT.with(|current| *current.borrow_mut() = previous);
    result
}

/// The call [`with_call`] set on this thread.
#[must_use]
pub fn current() -> Option<EditCall> {
    CURRENT.with(|current| current.borrow().clone())
}

/// Where a session's file-tool changes are recorded. Recording never
/// fails or blocks the change: failures are logged by the recorder.
pub trait EditRecorder: Send + Sync {
    /// A file tool is about to change `path`.
    fn before_change(&self, path: &Path, call: Option<&EditCall>);
    /// A file tool changed `path` from `before` (`None`: it did not exist)
    /// to `after` (`None`: it was removed).
    fn changed(
        &self,
        path: &Path,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
        call: Option<&EditCall>,
    );
}
