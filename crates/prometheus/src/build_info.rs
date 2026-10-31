//! The build identity baked in by `build.rs`
//! (spec: workspace-architecture, Version and build identity; Supported
//! targets and support tiers).

use std::sync::LazyLock;

/// Semantic version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Target triple.
pub const TARGET: &str = env!("PMT_BUILD_TARGET");
/// Short git commit, or `unknown`.
pub const COMMIT: &str = env!("PMT_BUILD_COMMIT");
/// Cargo profile (`debug` or `release`).
pub const PROFILE: &str = env!("PMT_BUILD_PROFILE");
/// UTC build time, RFC 3339.
pub const BUILD_DATE: &str = env!("PMT_BUILD_DATE");
/// Hash unique to this binary; the daemon's stale-build check compares it.
pub const BUILD_ID: &str = env!("PMT_BUILD_ID");

/// Text after the program name in the identity line
/// `prometheus <semver> (<channel>, <target>, <commit>, <build date>)`, shared
/// by `--version`, `prometheus version` and `/version`. Built once at first
/// use because the channel is derived from the version by [`channel`], which
/// stays the single place that rule lives.
#[must_use]
pub fn identity() -> &'static str {
    static IDENTITY: LazyLock<String> = LazyLock::new(|| {
        format!(
            "{VERSION} ({}, {TARGET}, {COMMIT}, {BUILD_DATE})",
            channel(VERSION)
        )
    });
    &IDENTITY
}

/// Tier 1 targets: full test suite on every merge.
const TIER_1: [&str; 6] = [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
];

/// Tier 2 targets: build plus the offline smoke suite on every merge.
const TIER_2: [&str; 3] = [
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "aarch64-linux-android",
];

/// Support tier of a target triple; `None` for an unsupported target.
#[must_use]
pub fn tier(target: &str) -> Option<u8> {
    if TIER_1.contains(&target) {
        Some(1)
    } else if TIER_2.contains(&target) {
        Some(2)
    } else {
        None
    }
}

/// Release channel of a version: canary builds are semver prereleases
/// `X.Y.Z-canary.<UTC timestamp>`; everything else is `stable`.
#[must_use]
pub fn channel(version: &str) -> &'static str {
    match version.split_once('-') {
        Some((_, pre)) if pre.starts_with("canary.") => "canary",
        _ => "stable",
    }
}

#[cfg(test)]
mod tests {
    use super::{channel, tier};

    #[test]
    fn every_listed_target_has_its_tier() {
        assert_eq!(tier("aarch64-apple-darwin"), Some(1));
        assert_eq!(tier("x86_64-unknown-linux-musl"), Some(1));
        assert_eq!(tier("aarch64-linux-android"), Some(2));
        assert_eq!(tier("x86_64-pc-windows-msvc"), Some(2));
        assert_eq!(tier("riscv64gc-unknown-linux-gnu"), None);
    }

    #[test]
    fn canary_prereleases_are_the_canary_channel() {
        assert_eq!(channel("1.4.0-canary.20261030T120000Z"), "canary");
        assert_eq!(channel("1.4.0"), "stable");
        assert_eq!(channel("1.4.0-rc.1"), "stable");
    }
}
