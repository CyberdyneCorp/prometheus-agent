//! Optional surfaces and the error for one missing from this build
//! (spec: workspace-architecture, Version and build identity).

use pmt_core::error::{Error, codes};

/// The optional surfaces, in `version --verbose` order, with whether each
/// is compiled into this build (cargo features of the same name).
pub const SURFACES: [(&str, bool); 7] = [
    ("tui", cfg!(feature = "tui")),
    ("gateway", cfg!(feature = "gateway")),
    ("dashboard", cfg!(feature = "dashboard")),
    ("acp", cfg!(feature = "acp")),
    ("mcp", cfg!(feature = "mcp")),
    ("evolve", cfg!(feature = "evolve")),
    ("eval", cfg!(feature = "eval")),
];

/// The gate every surface command calls first.
///
/// # Errors
///
/// `surface.not_included` (exit 69) when `name` is not compiled in.
pub fn require(name: &str) -> Result<(), Error> {
    let included = SURFACES.iter().any(|&(surface, on)| surface == name && on);
    if included {
        Ok(())
    } else {
        Err(not_included(name))
    }
}

/// `surface '<name>' is not included in this build`.
#[must_use]
pub fn not_included(name: &str) -> Error {
    Error::new(
        &codes::SURFACE_NOT_INCLUDED,
        format!("surface '{name}' is not included in this build"),
    )
    .with_hint("install a full build of prometheus")
}

#[cfg(test)]
mod tests {
    use super::{SURFACES, not_included, require};

    #[test]
    fn missing_surface_exits_69_with_the_spec_message() {
        let err = not_included("dashboard");
        assert_eq!(err.exit_code(), 69);
        assert_eq!(
            err.message(),
            "surface 'dashboard' is not included in this build"
        );
    }

    #[test]
    fn gate_follows_the_compiled_features() {
        for (name, on) in SURFACES {
            assert_eq!(require(name).is_ok(), on, "{name}");
        }
        assert!(require("nonexistent").is_err());
    }
}
