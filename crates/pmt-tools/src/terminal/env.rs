//! Child environments built from an allowlist (spec: execution-environments,
//! Child environments are built from an allowlist; Non-secret environment
//! passthrough).
//!
//! A child never starts from a copy of the parent's environment: it gets
//! the fixed allowlist and the `terminal.env_passthrough` names, both taken
//! from [`LaunchEnv::inherited`] (which never holds a secret), plus the
//! secrets the session was granted AND the target declares, plus what the
//! session sets itself (`TMPDIR`, the CA bundle). Every child (terminal,
//! background process, code execution, MCP stdio server, Python bridge,
//! hook script) should be started with [`ChildEnv::apply`].

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;

use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_core::secret::SecretString;

/// Variables every child receives when they are set at launch.
pub const ALLOWLIST: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "TERM", "SHELL", "TMPDIR", "TZ",
];

/// Prefixes of allowlisted families (`LC_ALL`, `LC_CTYPE`...).
pub const ALLOWLIST_PREFIXES: &[&str] = &["LC_"];

/// Platform location variables: where the OS keeps per-user and system
/// directories, without which common tools misbehave.
#[cfg(windows)]
pub const PLATFORM: &[&str] = &[
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "USERNAME",
    "USERDOMAIN",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "COMMONPROGRAMFILES",
    "COMMONPROGRAMFILES(X86)",
    "COMMONPROGRAMW6432",
    "PUBLIC",
    "TEMP",
    "TMP",
    "OS",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "COMPUTERNAME",
];

/// Platform location variables: the XDG base directories.
#[cfg(not(windows))]
pub const PLATFORM: &[&str] = &[
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
];

/// What a child environment is built from.
#[derive(Debug, Default)]
pub struct EnvRequest<'a> {
    /// `terminal.env_passthrough` (validated not to name a secret when the
    /// profile was opened).
    pub passthrough: &'a [String],
    /// The secrets this child receives: [`granted_secrets`] of the
    /// session's grants and the target's declared names.
    pub secrets: Vec<(String, SecretString)>,
    /// Set last, overriding everything (for example `TMPDIR`).
    pub overrides: Vec<(String, OsString)>,
}

/// The complete environment of a child process.
#[derive(Clone, Default)]
pub struct ChildEnv {
    vars: BTreeMap<String, OsString>,
    secret_names: Vec<String>,
}

impl std::fmt::Debug for ChildEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Values can be secrets: show names only.
        f.debug_struct("ChildEnv")
            .field("names", &self.vars.keys().collect::<Vec<_>>())
            .field("secret_names", &self.secret_names)
            .finish()
    }
}

impl ChildEnv {
    /// The environment for `request` over the launch environment.
    #[must_use]
    pub fn build(launch: &LaunchEnv, request: EnvRequest<'_>) -> Self {
        let mut env = Self::default();
        for (name, value) in launch.inherited_vars() {
            if is_allowlisted(name) {
                env.set(name, value);
            }
        }
        for name in request.passthrough {
            if let Some(value) = launch.inherited(name) {
                env.set(name, value);
            }
        }
        for (name, value) in request.secrets {
            env.set(&name, value.expose());
            env.secret_names.push(name);
        }
        for (name, value) in request.overrides {
            env.set(&name, &value);
        }
        env
    }

    /// A copy with `secrets` added (names recorded as secret names).
    #[must_use]
    pub fn with_secrets(&self, secrets: Vec<(String, SecretString)>) -> Self {
        let mut env = self.clone();
        for (name, value) in secrets {
            env.set(&name, value.expose());
            if !env.secret_names.contains(&name) {
                env.secret_names.push(name);
            }
        }
        env
    }

    /// Points `TMPDIR` (and `TEMP`/`TMP` on Windows) at `dir`, the session
    /// scratch directory the sandbox lets the child write.
    pub fn set_temp_dir(&mut self, dir: &Path) {
        self.set("TMPDIR", dir.as_os_str());
        if cfg!(windows) {
            self.set("TEMP", dir.as_os_str());
            self.set("TMP", dir.as_os_str());
        }
    }

    /// Hands the child the CA bundle written by
    /// [`pmt_net::tls::write_child_bundle`] (once per session).
    pub fn set_ca_bundle(&mut self, bundle: &Path) {
        for (name, path) in pmt_net::tls::child_ca_env(bundle) {
            self.set(name, path.as_os_str());
        }
    }

    /// Sets one variable (replacing a case-variant on Windows).
    pub fn set(&mut self, name: &str, value: impl AsRef<OsStr>) {
        if cfg!(windows) {
            self.vars.retain(|k, _| !k.eq_ignore_ascii_case(name));
        }
        self.vars.insert(name.to_owned(), value.as_ref().to_owned());
    }

    /// The value of `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&OsStr> {
        self.vars
            .iter()
            .find(|(k, _)| k.as_str() == name || (cfg!(windows) && k.eq_ignore_ascii_case(name)))
            .map(|(_, v)| v.as_os_str())
    }

    /// Every variable name, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.vars.keys().map(String::as_str)
    }

    /// Names of the secrets this environment carries (kept out of shell
    /// snapshots and redacted from output).
    #[must_use]
    pub fn secret_names(&self) -> &[String] {
        &self.secret_names
    }

    /// Replaces `command`'s environment with exactly this one.
    pub fn apply(&self, command: &mut std::process::Command) {
        command.env_clear();
        command.envs(&self.vars);
    }
}

fn is_allowlisted(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let exact = |list: &[&str]| {
        list.iter().any(|n| {
            if cfg!(windows) {
                n.eq_ignore_ascii_case(name)
            } else {
                *n == name
            }
        })
    };
    exact(ALLOWLIST)
        || exact(PLATFORM)
        || ALLOWLIST_PREFIXES
            .iter()
            .any(|p| name.starts_with(p) || (cfg!(windows) && upper.starts_with(p)))
}

/// The secrets a child receives: names the target declares AND the session
/// holds a `secrets.read:<NAME>` grant for (`is_granted`), resolved from the
/// session's profile.
///
/// # Errors
///
/// The first resolution error (`secret.missing`...).
pub fn granted_secrets(
    declared: &[String],
    is_granted: impl Fn(&str) -> bool,
    resolve: impl Fn(&str) -> Result<SecretString, Error>,
) -> Result<Vec<(String, SecretString)>, Error> {
    declared
        .iter()
        .filter(|name| is_granted(name))
        .map(|name| Ok((name.clone(), resolve(name)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use pmt_core::env::{EnvPolicy, LaunchEnv};
    use pmt_core::secret::SecretString;

    use super::{ChildEnv, EnvRequest, granted_secrets};

    fn launch() -> LaunchEnv {
        let vars = [
            ("PATH", "/usr/bin"),
            ("LANG", "C.UTF-8"),
            ("LC_ALL", "C.UTF-8"),
            ("JAVA_HOME", "/opt/jdk"),
            ("TELEGRAM_BOT_TOKEN", "123:bot-token"),
            ("OPENAI_API_KEY", "sk-launch-123456"),
            ("PMT_SECRET_GH", "ghp_secret"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        LaunchEnv::from_vars(
            vars,
            &EnvPolicy {
                provider_secret_names: &["OPENAI_API_KEY"],
                replacements: &[],
            },
        )
    }

    #[test]
    fn only_allowlisted_and_passthrough_names_are_copied() {
        let passthrough = ["JAVA_HOME".to_owned(), "UNSET".to_owned()];
        let env = ChildEnv::build(
            &launch(),
            EnvRequest {
                passthrough: &passthrough,
                ..EnvRequest::default()
            },
        );
        let names: Vec<&str> = env.names().collect();
        assert_eq!(names, ["JAVA_HOME", "LANG", "LC_ALL", "PATH"]);
    }

    #[test]
    fn a_secret_needs_both_a_grant_and_a_declaration() {
        let declared = ["OPENAI_API_KEY".to_owned(), "ANTHROPIC_API_KEY".to_owned()];
        let secrets = granted_secrets(
            &declared,
            |name| name == "OPENAI_API_KEY" || name == "GH",
            |name| Ok(SecretString::new(format!("value-of-{name}"))),
        )
        .unwrap();
        let env = ChildEnv::build(
            &launch(),
            EnvRequest {
                secrets,
                ..EnvRequest::default()
            },
        );
        assert_eq!(
            env.get("OPENAI_API_KEY").unwrap(),
            "value-of-OPENAI_API_KEY"
        );
        assert!(env.get("ANTHROPIC_API_KEY").is_none() && env.get("GH").is_none());
        assert_eq!(env.secret_names(), ["OPENAI_API_KEY"]);
        assert!(!format!("{env:?}").contains("value-of"));
    }
}
