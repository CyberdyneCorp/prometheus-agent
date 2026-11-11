//! Account usage windows (spec: model-providers, Account usage windows
//! command): a subscription's plan and usage windows as `prometheus auth
//! usage` and `/usage` show them, with a JSON shape scripts rely on (keys
//! stable, only extended by addition).

use std::fmt::Write as _;

use serde::Serialize;

use crate::quota::Quota;

/// One account's usage.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountUsage {
    /// Provider id.
    pub provider: String,
    /// The credential the usage belongs to (`subscription:<p>/<label>`).
    pub source: String,
    /// What the account is.
    pub title: String,
    /// The plan, when reported.
    pub plan: Option<String>,
    /// When the provider answered.
    pub fetched_at: Option<String>,
    /// The usage windows.
    pub windows: Vec<UsageWindow>,
    /// Further lines the provider reports.
    pub details: Vec<String>,
    /// Why there is nothing to show.
    pub unavailable_reason: Option<String>,
}

/// One usage window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UsageWindow {
    /// `5h`, `weekly`...
    pub label: String,
    /// Share used, 0-100.
    pub used_percent: f64,
    /// When the window resets.
    pub resets_at: Option<String>,
    /// A readable description.
    pub detail: String,
}

/// The account title of `provider`.
#[must_use]
pub fn title(provider: &str) -> String {
    match provider {
        "openai-codex" => "ChatGPT plan (OpenAI Codex)".to_owned(),
        "anthropic" => "Claude subscription".to_owned(),
        "nous" => "Nous Portal subscription".to_owned(),
        "xai-oauth" => "SuperGrok subscription".to_owned(),
        other => format!("{other} account"),
    }
}

impl AccountUsage {
    /// The usage of `provider`'s credential `source` from `quota`.
    #[must_use]
    pub fn from_quota(provider: &str, source: &str, quota: &Quota) -> Self {
        let windows = quota
            .windows
            .iter()
            .map(|w| UsageWindow {
                label: w.name.clone(),
                used_percent: w.used_percent,
                resets_at: w.resets_at.clone(),
                detail: match &w.resets_at {
                    Some(at) => format!(
                        "{} window, {:.0}% used, resets {at}",
                        w.name, w.used_percent
                    ),
                    None => format!("{} window, {:.0}% used", w.name, w.used_percent),
                },
            })
            .collect();
        Self {
            provider: provider.to_owned(),
            source: source.to_owned(),
            title: title(provider),
            plan: quota.plan.clone(),
            fetched_at: Some(quota.fetched_at.clone()),
            windows,
            details: Vec::new(),
            unavailable_reason: None,
        }
    }

    /// Nothing to show for `provider`, because of `reason`.
    #[must_use]
    pub fn unavailable(provider: &str, reason: &str) -> Self {
        Self {
            provider: provider.to_owned(),
            source: String::new(),
            title: title(provider),
            plan: None,
            fetched_at: None,
            windows: Vec::new(),
            details: Vec::new(),
            unavailable_reason: Some(reason.to_owned()),
        }
    }

    /// The text block in English (`prometheus auth usage`): title and
    /// plan, then one line per window.
    #[must_use]
    pub fn render(&self) -> String {
        self.render_in(&*pmt_i18n::Strings::english(pmt_i18n::Surface::Core))
    }

    /// The text block worded with `text` (`/usage`); the JSON fields stay
    /// English.
    #[must_use]
    pub fn render_in(&self, catalog: &dyn pmt_core::text::Catalog) -> String {
        let known = ["openai-codex", "anthropic", "nous", "xai-oauth"];
        let mut text = if known.contains(&self.provider.as_str()) {
            let id = self.provider.replace('-', "_");
            catalog.text(&format!("slash.account.title.{id}"), &[])
        } else {
            catalog.text("slash.account.title_other", &[("provider", &self.provider)])
        };
        if let Some(plan) = &self.plan {
            let _ = write!(text, " ({plan})");
        }
        if let Some(reason) = &self.unavailable_reason {
            let _ = write!(text, ": {reason}");
            return text;
        }
        for window in &self.windows {
            let percent = format!("{:.0}", window.used_percent);
            let args: [(&str, &dyn std::fmt::Display); 2] =
                [("label", &window.label), ("percent", &percent)];
            let line = match &window.resets_at {
                Some(at) => {
                    let mut all = args.to_vec();
                    all.push(("at", at));
                    catalog.text("slash.account.window_resets", &all)
                }
                None => catalog.text("slash.account.window", &args),
            };
            let _ = write!(text, "\n  {line}");
        }
        for line in &self.details {
            let _ = write!(text, "\n  {line}");
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::Window;

    #[test]
    fn windows_keep_their_percent_and_reset() {
        let quota = Quota {
            plan: Some("pro".into()),
            windows: vec![
                Window {
                    name: "5h".into(),
                    used_percent: 42.0,
                    resets_at: Some("2026-11-08T12:00:00Z".into()),
                },
                Window {
                    name: "weekly".into(),
                    used_percent: 7.5,
                    resets_at: None,
                },
            ],
            fetched_at: "2026-11-08T10:00:00Z".into(),
        };
        let usage =
            AccountUsage::from_quota("openai-codex", "subscription:openai-codex/default", &quota);
        let json = serde_json::to_value(&usage).unwrap();
        assert_eq!(json["windows"][0]["used_percent"], 42.0);
        assert_eq!(json["windows"][0]["resets_at"], "2026-11-08T12:00:00Z");
        for key in [
            "provider",
            "source",
            "title",
            "plan",
            "fetched_at",
            "windows",
            "details",
            "unavailable_reason",
        ] {
            assert!(json.get(key).is_some(), "{key}");
        }
        assert!(usage.render().contains("5h window, 42% used"));
    }
}
