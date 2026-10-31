//! The doctor checks of the install and its runtimes (spec:
//! distribution-and-update, Doctor reports distribution health): the
//! `distribution` group (owner, channel, version, release-key
//! fingerprints, last update receipt, pending follow-ups) and the
//! `runtimes` group (store, policy and the verification state of every
//! installed version), also printed by `prometheus runtimes doctor`.

use std::fmt::Write as _;
use std::path::Path;

use pmt_config::daemon::DaemonConfig;
use pmt_core::doctor::{Check, Status};
use pmt_core::error::Error;
use pmt_update::TrustRoot;
use pmt_update::flow::{self, Install};
use pmt_update::receipts::{self, Outcome, UpdateReceipt};
use pmt_update::runtimes::Store;
use serde_json::json;

use crate::output::{document, print_raw};

const DISTRIBUTION: &str = "distribution";
const RUNTIMES: &str = "runtimes";

/// The `distribution` group.
#[must_use]
pub fn distribution(install: &Install) -> Vec<Check> {
    let mut checks = Vec::new();
    match flow::status(install) {
        Ok(status) => checks.push(Check::new(
            DISTRIBUTION,
            "install",
            Status::Ok,
            format!(
                "owner {}, channel {}, version {} ({})",
                status.owner.as_str(),
                status.channel,
                status.version,
                status.target
            ),
        )),
        Err(err) => checks.push(
            Check::new(
                DISTRIBUTION,
                "install",
                Status::Fail,
                err.message().to_owned(),
            )
            .with_hint(format!(
                "fix or remove the install receipt {}",
                pmt_update::install::beside(&install.binary).display()
            )),
        ),
    }
    let keys: Vec<String> = TrustRoot::embedded()
        .keys()
        .iter()
        .map(|k| format!("{} {}", k.role(), k.id()))
        .collect();
    checks.push(Check::new(
        DISTRIBUTION,
        "release_keys",
        Status::Ok,
        format!("release keys: {}", keys.join(", ")),
    ));
    checks.extend(last_update(install.home.root()));
    checks
}

/// The last update receipt and its pending follow-ups.
fn last_update(root: &Path) -> Vec<Check> {
    let last = match receipts::last_update(root) {
        Ok(last) => last,
        Err(err) => {
            return vec![Check::new(
                DISTRIBUTION,
                "last_update",
                Status::Fail,
                err.message().to_owned(),
            )];
        }
    };
    let Some(receipt) = last else {
        return vec![Check::new(
            DISTRIBUTION,
            "last_update",
            Status::Ok,
            "no update has run",
        )];
    };
    let mut checks = vec![receipt_check(&receipt)];
    for follow_up in &receipt.follow_ups {
        checks.push(
            Check::new(DISTRIBUTION, "follow_up", Status::Warn, follow_up.clone())
                .with_hint("see prometheus update --status"),
        );
    }
    checks
}

fn receipt_check(receipt: &UpdateReceipt) -> Check {
    let stage = receipt.stopped_at().unwrap_or("unknown");
    match receipt.effective_outcome() {
        Outcome::Interrupted => Check::new(
            DISTRIBUTION,
            "last_update",
            Status::Fail,
            format!("last update interrupted at stage {stage}"),
        )
        .with_hint(
            "run 'prometheus update' again; the binary before the swap is kept as prometheus.prev ('prometheus update --rollback' restores it)",
        ),
        Outcome::Failed => Check::new(
            DISTRIBUTION,
            "last_update",
            Status::Fail,
            format!(
                "last update failed at stage {stage}: {}",
                receipt.errors.first().map_or("no error recorded", String::as_str)
            ),
        )
        .with_hint("the installed binary is unchanged; fix the cause and run 'prometheus update' again"),
        outcome => Check::new(
            DISTRIBUTION,
            "last_update",
            Status::Ok,
            format!(
                "last update {} ({} → {}, {})",
                outcome.as_str(),
                receipt.from,
                receipt.to.as_deref().unwrap_or("-"),
                receipt.started_at
            ),
        ),
    }
}

/// The `runtimes` group.
#[must_use]
pub fn runtimes(root: &Path) -> Vec<Check> {
    let store = Store::open(root);
    let mut checks = Vec::new();
    match DaemonConfig::load(root) {
        Ok(config) => {
            let rt = &config.runtimes;
            let mut policy = format!(
                "store {} ({}), lazy installs {}",
                store.root().display(),
                store.platform(),
                if rt.allow_lazy_install {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            if !rt.exclude.is_empty() {
                let _ = write!(policy, ", excluded: {}", rt.exclude.join(", "));
            }
            if let Some(mirror) = &rt.mirrors.runtimes {
                let _ = write!(policy, ", mirror {mirror}");
            }
            checks.push(Check::new(RUNTIMES, "policy", Status::Ok, policy));
        }
        Err(err) => checks.push(Check::new(
            RUNTIMES,
            "policy",
            Status::Fail,
            err.message().to_owned(),
        )),
    }
    let verified = store.verify();
    if verified.is_empty() {
        checks.push(Check::new(
            RUNTIMES,
            "managed",
            Status::Ok,
            "no managed runtimes installed",
        ));
    }
    for v in verified {
        checks.push(match v.mismatch {
            None => Check::new(
                RUNTIMES,
                "verified",
                Status::Ok,
                format!("{} {} verified", v.name, v.dir),
            ),
            Some(path) => Check::new(
                RUNTIMES,
                "verified",
                Status::Fail,
                format!(
                    "{}: realized bytes do not match recorded digest ({path} in {})",
                    v.name, v.dir
                ),
            )
            .with_hint(format!(
                "prometheus runtimes remove {0} && prometheus runtimes install {0}",
                v.name
            )),
        });
    }
    checks
}

/// `prometheus runtimes doctor`: the `runtimes` group; exit 1 on a failure.
///
/// # Errors
///
/// `output.io`.
pub fn print_runtimes(root: &Path, json: bool) -> Result<u8, Error> {
    let checks = runtimes(root);
    let failed = checks.iter().any(|c| c.status == Status::Fail);
    if json {
        document(
            "runtimes doctor",
            &json!({ "checks": checks, "failed": failed }),
        )?;
    } else {
        let mut text = String::new();
        for c in &checks {
            let status = if c.status == Status::Fail {
                "fail"
            } else {
                "ok  "
            };
            let _ = write!(text, "{status} {}", c.message);
            if let (Some(hint), Status::Fail) = (&c.hint, c.status) {
                let _ = write!(text, " (hint: {hint})");
            }
            text.push('\n');
        }
        print_raw(&text)?;
    }
    Ok(u8::from(failed))
}
