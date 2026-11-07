//! The session's hardening downgrades (spec: security-boundaries, Hardening
//! downgrades are never silent): the OS sandbox features missing on this
//! host and the local browser switches (`browser.chromium_args`) that turn
//! off Chromium's sandbox or web security, each allowed by
//! `security.sandbox.allow_degraded`. Every start of a sandboxed process or
//! browser logs them at WARN (pmt-sandbox, pmt-tools); the session records
//! each once in the audit log and its trajectory (`security.downgrade`) and
//! shows a degraded marker in its banner.

use pmt_core::audit::{AuditLog, Event, Kind};
use pmt_obs::trajectory::events::Event as TrajectoryEvent;
use pmt_obs::trajectory::payload::SecurityDowngrade;
use pmt_sandbox::{Gap, Sandbox};

/// What a session runs with below the platform's full hardening.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Downgrades {
    /// The sandbox's banner marker (`sandbox: partial:landlock_net`).
    sandbox_marker: Option<String>,
    /// Allowed sandbox gaps, then allowed browser switches.
    gaps: Vec<Gap>,
    /// How many of `gaps` come from the browser.
    browser: usize,
}

impl Downgrades {
    /// The downgrades of a session whose commands run under `sandbox` and
    /// whose local browsers start with the `browser` downgrades. A sandbox
    /// that refuses to run (a gap not allowed) contributes nothing: it
    /// starts no process.
    #[must_use]
    pub fn of(sandbox: Option<&Sandbox>, browser: Vec<Gap>) -> Self {
        let running = sandbox.filter(|sandbox| sandbox.check().is_ok());
        let mut gaps: Vec<Gap> = running.map(|s| s.downgrades().to_vec()).unwrap_or_default();
        let count = browser.len();
        gaps.extend(browser);
        Self {
            sandbox_marker: running.and_then(Sandbox::banner_marker),
            gaps,
            browser: count,
        }
    }

    /// Every allowed downgrade, sandbox first.
    #[must_use]
    pub fn gaps(&self) -> &[Gap] {
        &self.gaps
    }

    /// The marker every session banner shows while degraded, for example
    /// `sandbox: partial:landlock_net · browser: degraded:web_security`.
    #[must_use]
    pub fn banner_marker(&self) -> Option<String> {
        let browser = &self.gaps[self.gaps.len() - self.browser..];
        let browser = (!browser.is_empty()).then(|| {
            let features: Vec<&str> = browser.iter().map(|g| g.hardening.as_str()).collect();
            format!("browser: degraded:{}", features.join(","))
        });
        let parts: Vec<String> = self.sandbox_marker.iter().cloned().chain(browser).collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    /// Records each downgrade of session `session` in the profile's audit
    /// log.
    pub fn record(&self, audit: &AuditLog, session: &str) {
        for gap in &self.gaps {
            audit.record(
                &Event::new(Kind::Downgrade, gap.hardening.as_str(), "allowed")
                    .in_session(session)
                    .with_detail(gap.reason.clone()),
            );
        }
    }

    /// One `security.downgrade` trajectory event per downgrade.
    #[must_use]
    pub fn events(&self) -> Vec<TrajectoryEvent> {
        self.gaps
            .iter()
            .map(|gap| {
                TrajectoryEvent::SecurityDowngrade(SecurityDowngrade {
                    feature: gap.hardening.as_str().to_owned(),
                    reason: gap.reason.clone(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::{BrowserConfig, DegradedFeature};
    use pmt_tools::browser::options;

    use super::*;

    fn browser(args: &[&str], allowed: &[DegradedFeature]) -> Vec<Gap> {
        let config = BrowserConfig {
            chromium_args: args.iter().map(|a| (*a).to_owned()).collect(),
            ..BrowserConfig::default()
        };
        options::downgrades(&config, allowed)
    }

    /// Scenario: Explicit downgrade (an allowed browser switch).
    #[test]
    fn an_allowed_browser_switch_is_marked_audited_and_traced() {
        let gaps = browser(
            &["--no-sandbox", "--disable-web-security"],
            &[DegradedFeature::All],
        );
        let downgrades = Downgrades::of(None, gaps);
        assert_eq!(
            downgrades.banner_marker().as_deref(),
            Some("browser: degraded:chromium_sandbox,web_security")
        );
        let events = downgrades.events();
        assert_eq!(events.len(), 2);
        let TrajectoryEvent::SecurityDowngrade(first) = &events[0] else {
            panic!("{:?}", events[0]);
        };
        assert_eq!(first.feature, "chromium_sandbox");
        assert!(first.reason.contains("--no-sandbox"), "{}", first.reason);

        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        let audit = AuditLog::of(dir.path());
        downgrades.record(&audit, "ses_1");
        let recorded = audit.entries().unwrap();
        let subjects: Vec<&str> = recorded.iter().map(|r| r.subject.as_str()).collect();
        assert_eq!(subjects, ["chromium_sandbox", "web_security"]);
        assert!(recorded.iter().all(|r| r.kind == Kind::Downgrade
            && r.decision == "allowed"
            && r.session.as_deref() == Some("ses_1")));
    }

    #[test]
    fn full_hardening_has_no_marker() {
        let downgrades = Downgrades::of(None, browser(&["--lang=fr"], &[]));
        assert_eq!(downgrades.banner_marker(), None);
        assert!(downgrades.events().is_empty());
    }
}
