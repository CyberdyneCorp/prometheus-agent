//! The model segment of status lines (spec: model-providers, Context length
//! detection chain; Reasoning effort defaults, overrides and display;
//! Billing source transparency): one line naming the route, its context
//! window (a pin labelled `(pinned)`), the reasoning effort sent and the
//! billing source. The CLI status line, `/model` and `prometheus model
//! info` all render it here.

use crate::metadata::{ModelMetadata, window};
use crate::reasoning::Effort;
use crate::resolve::Route;

/// `openrouter/x · ctx 200000 (pinned) · reasoning high · api_key:openrouter`.
#[must_use]
pub fn line(route: &Route, meta: &ModelMetadata, effort: &Effort) -> String {
    format!(
        "{} · ctx {} · reasoning {} · {}",
        route.label(),
        window::label(meta.context_window, meta.context_source),
        effort.describe(),
        route.billing_source()
    )
}
