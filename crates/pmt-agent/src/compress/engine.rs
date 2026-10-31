//! Context engine selection (spec: context-compression, Context engine
//! selection).
//!
//! `context.engine` names the engine: `builtin` (the default) or an
//! engine registered by an installed and enabled plugin of the profile.
//! The names form a closed registry, so an unknown name fails with
//! `unknown context engine '<name>'` (exit 78) instead of falling back,
//! and a plugin engine is never activated without the key naming it.
//! Session start and `config validate` both select through [`select`].

use pmt_config::schema::ContextConfig;
use pmt_core::error::Error;
use pmt_core::registry::Registry;

/// The built-in engine's name.
pub const BUILTIN: &str = "builtin";

/// A selectable context engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Engine {
    /// Prune, summarize and commit in place (this module).
    Builtin,
    /// An engine a plugin registered (run by the plugin host, M2).
    Plugin(String),
}

/// The engines of a profile: the built-in one plus `plugin_engines`
/// (names registered by its installed and enabled plugins).
///
/// # Errors
///
/// `registry.duplicate_name` when a plugin registers `builtin` or a name
/// twice.
pub fn registry(plugin_engines: &[String]) -> Result<Registry<Engine>, Error> {
    let mut engines = Registry::from_entries("context engine", [(BUILTIN, Engine::Builtin)])?;
    for name in plugin_engines {
        engines.register(name.clone(), Engine::Plugin(name.clone()))?;
    }
    Ok(engines)
}

/// The engine `config` selects among the built-in one and
/// `plugin_engines`.
///
/// # Errors
///
/// `registry.unknown_name` (exit 78) `unknown context engine '<name>'`;
/// `registry.duplicate_name`.
pub fn select(config: &ContextConfig, plugin_engines: &[String]) -> Result<Engine, Error> {
    registry(plugin_engines)?.get(&config.engine).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(engine: &str) -> ContextConfig {
        ContextConfig {
            engine: engine.to_owned(),
        }
    }

    #[test]
    fn an_unknown_engine_is_a_config_error_not_a_fallback() {
        let err = select(&context("lcm"), &[]).unwrap_err();
        assert_eq!(err.message(), "unknown context engine 'lcm'");
        assert_eq!(err.exit_code(), 78);
    }

    #[test]
    fn a_plugin_engine_is_used_only_when_named() {
        let plugins = vec!["lcm".to_owned()];
        assert_eq!(
            select(&ContextConfig::default(), &plugins).unwrap(),
            Engine::Builtin
        );
        assert_eq!(
            select(&context("lcm"), &plugins).unwrap(),
            Engine::Plugin("lcm".into())
        );
    }

    #[test]
    fn a_plugin_cannot_take_the_builtin_name() {
        let err = registry(&["builtin".to_owned()]).unwrap_err();
        assert_eq!(err.code().code, "registry.duplicate_name");
    }
}
