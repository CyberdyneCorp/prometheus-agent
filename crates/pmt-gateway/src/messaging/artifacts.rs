//! Kanban artifacts (spec: platform-adapters, Response-wide delivery
//! modifiers and kanban artifacts): a completed task's notification
//! uploads each artifact path recorded on the task natively to the
//! subscribed chat, skips missing files and names the skipped ones.

use std::path::{Path, PathBuf};

use pmt_core::error::Error;
use pmt_store::gateway::outbox::Outbound;

use super::Gateway;
use super::deliverable;

impl Gateway {
    /// Queues `text` for `target` with each existing file of `artifacts`
    /// attached as the kind its extension delivers as (anything else as a
    /// document); missing files are named in the text.
    ///
    /// # Errors
    ///
    /// As [`Gateway::send`].
    pub async fn deliver_artifacts(
        &self,
        profile: &str,
        target: &str,
        text: &str,
        artifacts: &[PathBuf],
    ) -> Result<Outbound, Error> {
        let resolved = self.resolve_target(profile, target).await?;
        let media_out = self
            .shared
            .adapters
            .descriptor(profile, &resolved.platform)
            .map(|d| d.media_out)
            .unwrap_or_default();
        let (present, missing): (Vec<&PathBuf>, Vec<&PathBuf>) =
            artifacts.iter().partition(|p| p.is_file());
        let files: Vec<_> = present
            .iter()
            .map(|path| {
                let shown = path.to_string_lossy();
                deliverable::delivery(&shown, &media_out).unwrap_or_else(|| {
                    let mut file = super::adapter::OutboundAttachment::new(&shown);
                    file.as_document = true;
                    file
                })
            })
            .collect();
        let mut body = text.to_owned();
        if !missing.is_empty() {
            use std::fmt::Write as _;
            let names: Vec<String> = missing
                .iter()
                .map(|p| {
                    Path::new(p).file_name().map_or_else(
                        || p.display().to_string(),
                        |n| n.to_string_lossy().into_owned(),
                    )
                })
                .collect();
            let names = names.join(", ");
            let note = pmt_i18n::t!(
                self.shared.strings(profile),
                "gateway.artifacts_skipped",
                names = names
            );
            let _ = write!(body, "\n\n{note}");
        }
        self.enqueue(profile, &resolved, &body, &files)
    }
}
