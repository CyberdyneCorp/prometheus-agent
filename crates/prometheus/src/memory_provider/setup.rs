//! `prometheus memory setup [provider]` and `prometheus memory off` (spec:
//! memory, Provider setup writes secrets and settings to the right
//! stores).
//!
//! Setup asks for each field of the provider's `config-schema`: a field
//! marked secret goes only to the profile's secrets store (as
//! `<ID>_<KEY>`, upper-cased, the name the plugin is granted), every other
//! field only to `memory.providers.<id>.<key>` in `config.toml`. Then
//! `memory.provider` names the provider and its `post-setup` runs; the
//! change takes effect from the next session. Answers come from the
//! terminal, or one line per field from stdin.

use std::io::{BufRead as _, Write as _};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use pmt_memory::provider::SetupField;
use pmt_plugins::host::CallInfo;

use super::BUILTIN;
use crate::plugins_host::ProfilePlugins;

/// The secrets-store name of `provider`'s secret `key`.
#[must_use]
pub fn secret_name(provider: &str, key: &str) -> String {
    format!("{provider}_{key}")
        .to_ascii_uppercase()
        .replace(['-', '.', ' '], "_")
}

/// Where answers come from.
enum Answers {
    Terminal,
    Lines(std::vec::IntoIter<String>),
}

impl Answers {
    fn open() -> Self {
        if crate::prompt::stdin_is_tty() {
            return Self::Terminal;
        }
        let lines: Vec<String> = std::io::stdin()
            .lock()
            .lines()
            .map_while(Result::ok)
            .collect();
        Self::Lines(lines.into_iter())
    }

    fn next(&mut self, field: &SetupField) -> Result<String, Error> {
        let io = |e: std::io::Error| Error::new(&codes::OUTPUT_IO, e.to_string());
        match self {
            Self::Lines(lines) => Ok(lines.next().unwrap_or_default()),
            Self::Terminal if field.secret => {
                rpassword::prompt_password(format!("{} (input hidden): ", field.description))
                    .map_err(io)
            }
            Self::Terminal => {
                eprint!("{}: ", field.description);
                std::io::stderr().flush().map_err(io)?;
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).map_err(io)?;
                Ok(line.trim_end_matches(['\r', '\n']).to_owned())
            }
        }
    }
}

/// `prometheus memory setup [provider]`; the confirmation line.
///
/// # Errors
///
/// An unknown or unloaded provider, a missing required answer, config or
/// secret store errors.
pub fn setup(
    scope: &ProfileScope,
    plugins: &ProfilePlugins,
    provider: Option<&str>,
) -> Result<String, Error> {
    let id = provider
        .unwrap_or(&scope.config().memory.provider)
        .to_owned();
    if id == BUILTIN {
        return Err(Error::new(
            &codes::CLI_USAGE,
            "name the memory provider to set up: prometheus memory setup <provider>",
        ));
    }
    let mut wanted = scope.config().memory.clone();
    wanted.provider.clone_from(&id);
    super::check_plugins(&wanted, plugins)?;
    let file = scope.profile().config_file();
    // The provider the setup is for, as configured once it is set.
    let probe = scope.patched(|c| {
        c.memory.provider.clone_from(&id);
        c.plugins.disabled.retain(|d| *d != id);
    });
    let backend = super::configured(&probe, plugins, &CallInfo::default())
        .map_err(|(_, message)| Error::new(&codes::CONFIG_INVALID, message))?
        .ok_or_else(|| {
            Error::new(
                &codes::CONFIG_INVALID,
                format!("memory provider '{id}' is not loaded"),
            )
        })?;
    let mut answers = Answers::open();
    for field in backend.config_schema() {
        let answer = answers.next(&field)?;
        let Some(value) = Some(answer)
            .filter(|a| !a.is_empty())
            .or_else(|| field.default.clone())
        else {
            if field.required {
                return Err(Error::new(
                    &codes::CONFIG_INVALID,
                    format!("memory provider '{id}' needs a value for {}", field.key),
                ));
            }
            continue;
        };
        if field.secret {
            scope
                .secrets()
                .store()
                .set(&secret_name(&id, &field.key), &SecretString::new(value))?;
        } else {
            file.set(&format!("memory.providers.{id}.{}", field.key), &value)?;
        }
    }
    file.set("memory.provider", &id)?;
    if let Some(plugin) = plugins
        .current()
        .plugins
        .iter()
        .find(|p| p.manifest.id == id)
        && let Ok(Err(err)) = plugin.instance.memory_post_setup(&CallInfo::default())
    {
        tracing::warn!(provider = %id, "memory provider post-setup failed: {err}");
    }
    Ok(format!(
        "memory provider '{id}' set up; it takes effect from the next session"
    ))
}

/// `prometheus memory off`: back to built-in memory only.
///
/// # Errors
///
/// Config write errors.
pub fn off(scope: &ProfileScope) -> Result<String, Error> {
    scope
        .profile()
        .config_file()
        .set("memory.provider", BUILTIN)?;
    Ok("memory provider off: built-in memory only from the next session".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_names_are_upper_snake_case() {
        assert_eq!(secret_name("honcho", "api_key"), "HONCHO_API_KEY");
        assert_eq!(secret_name("mem-0", "token"), "MEM_0_TOKEN");
        assert!(super::super::check(&pmt_config::schema::MemoryConfig::default(), &[]).is_ok());
    }
}
