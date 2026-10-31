//! The enforcement-strength report: one level per feature (`fs`, `net`,
//! `process`, `syscalls`), as `prometheus doctor security` prints it
//! (spec: security-boundaries, OS sandbox per platform; Hardening
//! downgrades are never silent).

use std::fmt;

use pmt_config::schema::DegradedFeature;
use pmt_core::doctor::{Check, Status};

/// A sandbox feature reported on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// Filesystem access.
    Fs,
    /// Network sockets.
    Net,
    /// Other processes (signals, tracing, spawning).
    Process,
    /// The system-call surface.
    Syscalls,
}

impl Feature {
    /// Every feature, in report order.
    pub const ALL: [Self; 4] = [Self::Fs, Self::Net, Self::Process, Self::Syscalls];

    /// The report name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fs => "fs",
            Self::Net => "net",
            Self::Process => "process",
            Self::Syscalls => "syscalls",
        }
    }
}

/// How strongly a feature is enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Not enforced.
    Unavailable,
    /// Enforced in part.
    Partial,
    /// Fully enforced.
    Enforced,
}

impl Level {
    /// The report name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enforced => "enforced",
            Self::Partial => "partial",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Why a feature is below the platform's full level, and the
/// `security.sandbox.allow_degraded` name that accepts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    /// The hardening feature that is missing.
    pub hardening: DegradedFeature,
    /// Why, for example `landlock not supported by kernel`.
    pub reason: String,
}

impl Gap {
    /// A gap named by `hardening`.
    #[must_use]
    pub fn new(hardening: DegradedFeature, reason: impl Into<String>) -> Self {
        Self {
            hardening,
            reason: reason.into(),
        }
    }

    /// Whether `allowed` (`security.sandbox.allow_degraded`) accepts it.
    #[must_use]
    pub fn is_allowed(&self, allowed: &[DegradedFeature]) -> bool {
        allowed
            .iter()
            .any(|a| *a == self.hardening || *a == DegradedFeature::All)
    }
}

/// One feature's enforcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureReport {
    /// The feature.
    pub feature: Feature,
    /// Its level on this host for this policy.
    pub level: Level,
    /// The mechanism enforcing it, for example `landlock`.
    pub mechanism: &'static str,
    /// Set when the level is below the platform's full level.
    pub gap: Option<Gap>,
}

impl FeatureReport {
    /// A feature at the platform's full level.
    #[must_use]
    pub fn full(feature: Feature, level: Level, mechanism: &'static str) -> Self {
        Self {
            feature,
            level,
            mechanism,
            gap: None,
        }
    }

    /// A feature below the platform's full level.
    #[must_use]
    pub fn degraded(feature: Feature, level: Level, mechanism: &'static str, gap: Gap) -> Self {
        Self {
            feature,
            level,
            mechanism,
            gap: Some(gap),
        }
    }
}

/// Enforcement per feature for one platform and policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrengthReport {
    /// The sandbox in use, for example `landlock+seccomp` or `seatbelt`.
    pub platform: &'static str,
    /// One entry per [`Feature`], in [`Feature::ALL`] order.
    pub features: Vec<FeatureReport>,
}

impl StrengthReport {
    /// Every feature unavailable for one reason (no sandbox for this OS,
    /// `security.sandbox.mode = "off"`).
    #[must_use]
    pub fn none(platform: &'static str, gap: &Gap) -> Self {
        let features = Feature::ALL
            .iter()
            .map(|f| FeatureReport::degraded(*f, Level::Unavailable, platform, gap.clone()))
            .collect();
        Self { platform, features }
    }

    /// The level of `feature`.
    #[must_use]
    pub fn level(&self, feature: Feature) -> Level {
        self.features
            .iter()
            .find(|f| f.feature == feature)
            .map_or(Level::Unavailable, |f| f.level)
    }

    /// Every gap, in feature order, without repeats.
    #[must_use]
    pub fn gaps(&self) -> Vec<&Gap> {
        let mut gaps: Vec<&Gap> = Vec::new();
        for gap in self.features.iter().filter_map(|f| f.gap.as_ref()) {
            if !gaps.iter().any(|g| g.hardening == gap.hardening) {
                gaps.push(gap);
            }
        }
        gaps
    }

    /// The summary for `prometheus doctor terminal` and session banners.
    #[must_use]
    pub fn enforcement(&self) -> Enforcement {
        if self.features.iter().all(|f| f.level == Level::Unavailable) {
            return Enforcement::None;
        }
        let missing: Vec<DegradedFeature> = self.gaps().iter().map(|g| g.hardening).collect();
        if missing.is_empty() {
            Enforcement::Full
        } else {
            Enforcement::Partial(missing)
        }
    }

    /// One `security` doctor check per feature: `ok` at full level, `warn`
    /// when degraded.
    #[must_use]
    pub fn doctor_checks(&self) -> Vec<Check> {
        self.features.iter().map(feature_check).collect()
    }
}

fn feature_check(f: &FeatureReport) -> Check {
    let name = match f.feature {
        Feature::Fs => "sandbox_fs",
        Feature::Net => "sandbox_net",
        Feature::Process => "sandbox_process",
        Feature::Syscalls => "sandbox_syscalls",
    };
    let message = format!(
        "{}: {} ({})",
        f.feature.as_str(),
        f.level.as_str(),
        f.mechanism
    );
    match &f.gap {
        None => Check::new("security", name, Status::Ok, message),
        Some(gap) => Check::new(
            "security",
            name,
            Status::Warn,
            format!("{message}: {}", gap.reason),
        )
        .with_hint(allow_hint(gap.hardening)),
    }
}

/// The remedy named in refusals and doctor output.
pub(crate) fn allow_hint(hardening: DegradedFeature) -> String {
    format!(
        "set security.sandbox.allow_degraded = [\"{}\"] to run without it",
        hardening.as_str()
    )
}

/// The overall level for `prometheus doctor terminal` and session banners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enforcement {
    /// Every feature at the platform's full level.
    Full,
    /// Some hardening missing.
    Partial(Vec<DegradedFeature>),
    /// No OS sandbox.
    None,
}

impl fmt::Display for Enforcement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => f.write_str("full"),
            Self::None => f.write_str("none"),
            Self::Partial(missing) => {
                let names: Vec<&str> = missing.iter().map(|m| m.as_str()).collect();
                write!(f, "partial:{}", names.join(","))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(net: Level, gap: Option<Gap>) -> StrengthReport {
        let mut features = vec![FeatureReport::full(
            Feature::Fs,
            Level::Enforced,
            "landlock",
        )];
        features.push(FeatureReport {
            feature: Feature::Net,
            level: net,
            mechanism: "landlock",
            gap,
        });
        features.push(FeatureReport::full(
            Feature::Process,
            Level::Enforced,
            "seccomp",
        ));
        features.push(FeatureReport::full(
            Feature::Syscalls,
            Level::Enforced,
            "seccomp",
        ));
        StrengthReport {
            platform: "landlock+seccomp",
            features,
        }
    }

    #[test]
    fn enforcement_summary() {
        assert_eq!(
            report(Level::Enforced, None).enforcement().to_string(),
            "full"
        );
        let gap = Gap::new(DegradedFeature::LandlockNet, "no tcp rules");
        let partial = report(Level::Unavailable, Some(gap));
        assert_eq!(partial.enforcement().to_string(), "partial:landlock_net");
        assert_eq!(partial.level(Feature::Net), Level::Unavailable);
        let off = StrengthReport::none("off", &Gap::new(DegradedFeature::All, "off"));
        assert_eq!(off.enforcement().to_string(), "none");
        assert_eq!(off.gaps().len(), 1);
    }

    #[test]
    fn doctor_lists_every_feature_with_its_level() {
        let gap = Gap::new(DegradedFeature::LandlockNet, "no tcp rules");
        let checks = report(Level::Unavailable, Some(gap)).doctor_checks();
        let lines: Vec<&str> = checks.iter().map(|c| c.message.as_str()).collect();
        assert_eq!(
            lines,
            [
                "fs: enforced (landlock)",
                "net: unavailable (landlock): no tcp rules",
                "process: enforced (seccomp)",
                "syscalls: enforced (seccomp)"
            ]
        );
        assert_eq!(checks[1].status, Status::Warn);
    }

    #[test]
    fn all_allows_every_gap() {
        let gap = Gap::new(DegradedFeature::Seccomp, "x");
        assert!(gap.is_allowed(&[DegradedFeature::All]));
        assert!(gap.is_allowed(&[DegradedFeature::Seccomp]));
        assert!(!gap.is_allowed(&[DegradedFeature::Landlock]));
    }
}
