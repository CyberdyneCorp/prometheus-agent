//! The per-job notepad (spec: scheduled-jobs, Per-job notepad): durable
//! key-value notes a run reads at the top of its prompt and updates with
//! `prometheus cron notepad <job> set <key> <value>`.

use std::fmt::Write as _;

use pmt_core::error::{Error, codes};
use pmt_store::Store;

/// A key at most, in characters.
pub const MAX_KEY_CHARS: usize = 128;
/// A value at most, in bytes.
pub const MAX_VALUE_BYTES: usize = 16 * 1024;
/// A notepad at most, keys and values, in bytes.
pub const MAX_NOTEPAD_BYTES: usize = 64 * 1024;

fn exceeded(what: &str) -> Error {
    Error::new(
        &codes::CRON_NOTEPAD_LIMIT,
        format!("notepad limit exceeded: {what}"),
    )
}

/// Checks a notepad as it would be after a write.
///
/// # Errors
///
/// `cron.notepad_limit` (65) naming the limit.
pub fn check(notes: &[(String, String)]) -> Result<(), Error> {
    if let Some((key, _)) = notes
        .iter()
        .find(|(k, _)| k.is_empty() || k.chars().count() > MAX_KEY_CHARS)
    {
        return Err(exceeded(&format!(
            "key '{key}' must be 1 to {MAX_KEY_CHARS} characters"
        )));
    }
    if let Some((key, _)) = notes.iter().find(|(_, v)| v.len() > MAX_VALUE_BYTES) {
        return Err(exceeded(&format!(
            "the value of '{key}' is over {MAX_VALUE_BYTES} bytes"
        )));
    }
    let total: usize = notes.iter().map(|(k, v)| k.len() + v.len()).sum();
    if total > MAX_NOTEPAD_BYTES {
        return Err(exceeded(&format!(
            "the notepad would hold {total} bytes (at most {MAX_NOTEPAD_BYTES})"
        )));
    }
    Ok(())
}

/// Sets `key` of `job_id`'s notepad.
///
/// # Errors
///
/// `cron.notepad_limit`, store errors.
pub fn set(store: &Store, job_id: &str, key: &str, value: &str) -> Result<(), Error> {
    store
        .cron_notepad_write(job_id, key, Some(value), check)
        .map(drop)
}

/// Deletes `key`; whether it existed.
///
/// # Errors
///
/// Store errors.
pub fn delete(store: &Store, job_id: &str, key: &str) -> Result<bool, Error> {
    store.cron_notepad_write(job_id, key, None, |_| Ok(()))
}

/// The `## Job notepad` section of a non-empty notepad.
#[must_use]
pub fn section(notes: &[(String, String)]) -> Option<String> {
    if notes.is_empty() {
        return None;
    }
    let mut text = String::from(
        "## Job notepad\n\nNotes this job keeps between runs (update them with `prometheus cron \
         notepad <job> set <key> <value>`):\n",
    );
    for (key, value) in notes {
        let _ = write!(text, "\n- {key}: {value}");
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::{check, section};

    #[test]
    fn limits_and_rendering() {
        assert!(check(&[("last_id".into(), "4812".into())]).is_ok());
        let long = "k".repeat(129);
        assert!(
            check(&[(long, "v".into())])
                .unwrap_err()
                .message()
                .starts_with("notepad limit exceeded")
        );
        assert!(check(&[("k".into(), "v".repeat(16 * 1024 + 1))]).is_err());
        let many: Vec<_> = (0..5)
            .map(|i| (format!("k{i}"), "v".repeat(16 * 1024 - 2)))
            .collect();
        assert!(check(&many).is_err());
        assert_eq!(section(&[]), None);
        assert!(
            section(&[("last_id".into(), "4812".into())])
                .unwrap()
                .contains("- last_id: 4812")
        );
    }
}
