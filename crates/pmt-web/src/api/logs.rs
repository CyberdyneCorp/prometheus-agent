//! `GET /api/logs?file=<name>&lines=<n>` (spec: web-dashboard, Logs page is
//! redacted and bounded): the last records of one log of the registry: a
//! daemon-wide component (`gateway`, `update`, `migrate`) or
//! `<profile>/<component>` of a served profile. A name is looked up, never
//! used as a path; every record is redacted before it leaves the daemon.

use std::path::PathBuf;

use pmt_obs::logs::{self, COMPONENTS, HOME_COMPONENTS};
use serde_json::{Value, json};

use crate::Shared;
use crate::http::{Reply, Req, error, from_error, json_reply};

/// The most lines one call returns.
pub(crate) const MAX_LINES: usize = 5000;

/// One log of the registry: its name, file and owning profile.
struct Entry {
    name: String,
    file: PathBuf,
    component: &'static str,
    profile: Option<String>,
}

/// Every log the dashboard may tail.
fn registry(shared: &Shared) -> Vec<Entry> {
    let mut entries: Vec<Entry> = HOME_COMPONENTS
        .iter()
        .filter_map(|component| {
            let home = shared.server.home()?;
            Some(Entry {
                name: (*component).to_owned(),
                file: logs::view::current_file(home, home, component),
                component,
                profile: None,
            })
        })
        .collect();
    for profile in shared.server.profiles() {
        let Some(scope) = shared.host.scope(&profile) else {
            continue;
        };
        for component in COMPONENTS.iter().filter(|c| !HOME_COMPONENTS.contains(c)) {
            entries.push(Entry {
                name: format!("{profile}/{component}"),
                file: logs::view::current_file(scope.home(), scope.dir(), component),
                component,
                profile: Some(profile.clone()),
            });
        }
    }
    entries
}

pub(crate) fn tail(shared: &Shared, req: &Req) -> Reply {
    let entries = registry(shared);
    let Some(name) = req.param("file") else {
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        return json_reply(200, &json!({"files": names}));
    };
    let Some(entry) = entries.into_iter().find(|e| e.name == name) else {
        return error(400, "unknown_log_file", "unknown log file");
    };
    let lines = req
        .param("lines")
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(200)
        .clamp(1, MAX_LINES);
    let filter = logs::view::Filter {
        component: entry.component.to_owned(),
        ..logs::view::Filter::default()
    };
    let records = match logs::view::tail(&entry.file, &filter, lines) {
        Ok((records, _)) => records,
        Err(err) => return from_error(&err),
    };
    let redacted: Vec<Value> = records
        .iter()
        .filter_map(|record| {
            let text = serde_json::to_string(record).ok()?;
            serde_json::from_str(&shared.host.redact(entry.profile.as_deref(), &text)).ok()
        })
        .collect();
    json_reply(200, &json!({"file": entry.name, "records": redacted}))
}
