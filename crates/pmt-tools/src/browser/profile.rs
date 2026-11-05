//! Which profile a local browser launches on (spec: execution-environments,
//! Local browser isolation; Real-profile browsing).
//!
//! The user's real browser profile is used only when the configuration
//! asks for it (`browser.use_real_profile`, which the real-profile mode
//! adds) AND the session holds the `browser.real_profile` grant from
//! `grants.toml`; every other launch gets a throwaway profile under
//! `<profile>/cache/browser/profiles/`. The real-profile mode calls
//! [`choose`] before touching the user's profile; until it ships every
//! session launches on a throwaway profile ([`super::session`]).

use pmt_core::capability::{Capability, CapabilitySet};

/// The profile a local browser launches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileSource {
    /// A fresh directory removed when the browser ends.
    Throwaway,
    /// A managed copy of the user's real browser profile.
    Real,
}

/// The profile of a session whose configuration asks for the real one
/// (`wanted`) and whose effective capabilities are `granted`.
#[must_use]
pub fn choose(wanted: bool, granted: &CapabilitySet) -> ProfileSource {
    if wanted && granted.allows(&Capability::BrowserRealProfile) {
        ProfileSource::Real
    } else {
        ProfileSource::Throwaway
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(capabilities: &[&str]) -> CapabilitySet {
        capabilities
            .iter()
            .map(|c| Capability::parse(c).unwrap())
            .collect()
    }

    #[test]
    fn the_real_profile_needs_both_the_setting_and_the_grant() {
        let granted = set(&["browser.real_profile"]);
        let other = set(&["fs.read:<workspace>/**"]);
        assert_eq!(choose(true, &granted), ProfileSource::Real);
        assert_eq!(choose(true, &other), ProfileSource::Throwaway);
        assert_eq!(choose(false, &granted), ProfileSource::Throwaway);
    }
}
