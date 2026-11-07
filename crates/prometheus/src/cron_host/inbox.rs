//! The suggested automations inbox as the background review sees it
//! (spec: scheduled-jobs, Suggested automations inbox): a proposal is a
//! `usage` suggestion, checked like a job (key, schedule grammar, prompt
//! safety scan) and offered under the inbox's rules (a dismissed key is
//! never offered again, at most five pending).

use std::sync::Arc;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_config::ProfileScope;
use pmt_cron::suggest::{self, Source, Suggestion};
use pmt_memory::review::AutomationSuggestions;
use pmt_store::Store;

/// One profile's inbox.
struct Inbox {
    store: Arc<Store>,
    zone: TimeZone,
}

impl AutomationSuggestions for Inbox {
    fn offer(&self, key: &str, title: &str, schedule: &str, prompt: &str) -> Result<bool, String> {
        let suggestion = Suggestion {
            key: key.to_owned(),
            source: Source::Usage,
            title: title.to_owned(),
            schedule: schedule.to_owned(),
            prompt: prompt.to_owned(),
        };
        suggestion
            .check(Timestamp::now(), &self.zone)
            .map_err(|e| e.message().to_owned())?;
        suggest::offer(&self.store, &suggestion).map_err(|e| e.message().to_owned())
    }
}

/// The inbox of `scope`'s profile.
#[must_use]
pub fn inbox(scope: &ProfileScope, store: Arc<Store>) -> Arc<dyn AutomationSuggestions> {
    Arc::new(Inbox {
        store,
        zone: super::profile_zone(scope).unwrap_or(TimeZone::UTC),
    })
}
