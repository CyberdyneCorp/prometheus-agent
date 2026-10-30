//! The launch environment: the process environment, read exactly once at
//! startup into an immutable [`LaunchEnv`] (spec: workspace-architecture,
//! Launch environment is read once; configuration-and-profiles, Environment
//! is secrets-only).
//!
//! Only the home override, the log filter, secrets and the OS directories
//! needed to find the default home are kept. Every other `PMT_*` or
//! `PROMETHEUS_*` variable is recorded as ignored so startup can WARN and
//! `prometheus doctor` can report it; it never changes behaviour, because
//! behaviour lives in typed configuration.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{Error, codes};
use crate::secret::SecretString;

/// Prefix of generic secret variables: `PMT_SECRET_<NAME>` supplies secret `NAME`.
pub const SECRET_PREFIX: &str = "PMT_SECRET_";

const HOME_VAR: &str = "PROMETHEUS_HOME";
const PMT_LOG: &str = "PMT_LOG";
const RUST_LOG: &str = "RUST_LOG";
const OWNED_PREFIXES: [&str; 2] = ["PMT_", "PROMETHEUS_"];

static READ: AtomicBool = AtomicBool::new(false);

/// What the reader needs to know beyond the fixed variable names.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvPolicy<'a> {
    /// Provider-native credential names declared by registered providers,
    /// for example `OPENAI_API_KEY`.
    pub provider_secret_names: &'a [&'a str],
    /// Known non-secret variables and the config key that replaces each,
    /// for example `("PMT_MAX_TURNS", "agent.max_iterations")`.
    pub replacements: &'a [(&'a str, &'a str)],
}

/// Why a variable was ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IgnoreReason {
    /// A non-secret `PMT_*`/`PROMETHEUS_*` variable; configuration lives in
    /// `config.toml`. Names the replacing config key when one exists.
    NotConfiguration {
        /// The config key that replaces the variable.
        replacement: Option<String>,
    },
    /// An accepted variable whose value is not valid UTF-8.
    NotUnicode,
}

/// A set variable that was ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredVar {
    /// The variable name.
    pub name: String,
    /// Why it was ignored.
    pub reason: IgnoreReason,
}

impl IgnoredVar {
    /// The one-line WARN logged once per process at startup.
    #[must_use]
    pub fn warning(&self) -> String {
        let name = &self.name;
        match &self.reason {
            IgnoreReason::NotConfiguration {
                replacement: Some(key),
            } => format!("{name} is ignored; set {key} in config.toml"),
            IgnoreReason::NotConfiguration { replacement: None } => {
                format!("{name} is ignored; configuration lives in config.toml")
            }
            IgnoreReason::NotUnicode => format!("{name} is ignored; its value is not valid UTF-8"),
        }
    }

    /// The warning `prometheus doctor` prints in its `config` group.
    #[must_use]
    pub fn doctor_message(&self) -> String {
        let name = &self.name;
        match self.reason {
            IgnoreReason::NotConfiguration { .. } => {
                format!("ignored environment variable {name} (configuration lives in config.toml)")
            }
            IgnoreReason::NotUnicode => {
                format!("ignored environment variable {name} (value is not valid UTF-8)")
            }
        }
    }
}

/// The immutable launch configuration read from the process environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchEnv {
    home: Option<PathBuf>,
    log_filter: Option<String>,
    user_home: Option<PathBuf>,
    local_app_data: Option<PathBuf>,
    /// Keyed by variable name (`PMT_SECRET_X` or a provider-native name).
    secrets: BTreeMap<String, SecretString>,
    ignored: Vec<IgnoredVar>,
}

impl LaunchEnv {
    /// Reads the process environment. Call it once, at startup, and pass
    /// the result down.
    ///
    /// # Errors
    ///
    /// `launch_env.reread` on a second call in the same process: a later
    /// read could observe a different environment than the one the process
    /// started with.
    pub fn from_process(policy: &EnvPolicy<'_>) -> Result<Self, Error> {
        if READ.swap(true, Ordering::SeqCst) {
            return Err(Error::new(
                &codes::LAUNCH_ENV_REREAD,
                "the process environment was already read at startup",
            )
            .with_hint("pass the LaunchEnv captured at startup instead of reading again"));
        }
        Ok(Self::from_vars(std::env::vars_os(), policy))
    }

    /// Builds the launch configuration from explicit variables; this is the
    /// whole reader, `from_process` only feeds it the real environment.
    #[must_use]
    pub fn from_vars(
        vars: impl IntoIterator<Item = (OsString, OsString)>,
        policy: &EnvPolicy<'_>,
    ) -> Self {
        let mut env = Self::default();
        let mut pmt_log = None;
        let mut rust_log = None;
        for (name, value) in vars {
            let name = name.to_string_lossy().into_owned();
            match classify(&name, policy) {
                Var::Home => env.home = non_blank(value).map(PathBuf::from),
                Var::UserHome => env.user_home = non_blank(value).map(PathBuf::from),
                Var::LocalAppData => env.local_app_data = non_blank(value).map(PathBuf::from),
                Var::PmtLog => pmt_log = env.unicode(name, value),
                Var::RustLog => rust_log = env.unicode(name, value),
                Var::Secret => {
                    if let Some(value) = env.unicode(name.clone(), value) {
                        env.secrets.insert(name, SecretString::new(value));
                    }
                }
                Var::Ignored(replacement) => env.ignored.push(IgnoredVar {
                    name,
                    reason: IgnoreReason::NotConfiguration { replacement },
                }),
                Var::Other => {}
            }
        }
        env.log_filter = pmt_log.or(rust_log);
        env.ignored.sort_by(|a, b| a.name.cmp(&b.name));
        env
    }

    /// The value as UTF-8, or `None` with the variable recorded as ignored.
    fn unicode(&mut self, name: String, value: OsString) -> Option<String> {
        if let Ok(value) = value.into_string() {
            return Some(value);
        }
        self.ignored.push(IgnoredVar {
            name,
            reason: IgnoreReason::NotUnicode,
        });
        None
    }

    /// `PROMETHEUS_HOME` when set and not blank (`~` is expanded by home
    /// resolution, not here).
    #[must_use]
    pub fn home(&self) -> Option<&PathBuf> {
        self.home.as_ref()
    }

    /// The log filter: `PMT_LOG`, else `RUST_LOG`.
    #[must_use]
    pub fn log_filter(&self) -> Option<&str> {
        self.log_filter.as_deref()
    }

    /// The user's home directory (`HOME`, or `USERPROFILE` on Windows).
    #[must_use]
    pub fn user_home(&self) -> Option<&PathBuf> {
        self.user_home.as_ref()
    }

    /// `LOCALAPPDATA` (Windows default home parent).
    #[must_use]
    pub fn local_app_data(&self) -> Option<&PathBuf> {
        self.local_app_data.as_ref()
    }

    /// The environment-supplied value of secret `name`: `PMT_SECRET_<name>`
    /// first, then a provider-native variable called `name`.
    #[must_use]
    pub fn secret(&self, name: &str) -> Option<&SecretString> {
        self.secrets
            .get(&format!("{SECRET_PREFIX}{name}"))
            .or_else(|| self.secrets.get(name))
    }

    /// Every environment-supplied secret, keyed by variable name (for
    /// building the known-secret redactor).
    pub fn secrets(&self) -> impl Iterator<Item = (&str, &SecretString)> {
        self.secrets.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Variables that were set but ignored, sorted by name.
    #[must_use]
    pub fn ignored(&self) -> &[IgnoredVar] {
        &self.ignored
    }
}

enum Var {
    Home,
    UserHome,
    LocalAppData,
    PmtLog,
    RustLog,
    Secret,
    Ignored(Option<String>),
    Other,
}

fn classify(name: &str, policy: &EnvPolicy<'_>) -> Var {
    match name {
        HOME_VAR => return Var::Home,
        PMT_LOG => return Var::PmtLog,
        RUST_LOG => return Var::RustLog,
        "LOCALAPPDATA" => return Var::LocalAppData,
        _ => {}
    }
    if is_user_home_var(name) {
        return Var::UserHome;
    }
    let generic_secret = name
        .strip_prefix(SECRET_PREFIX)
        .is_some_and(|rest| !rest.is_empty());
    if generic_secret || policy.provider_secret_names.contains(&name) {
        return Var::Secret;
    }
    if OWNED_PREFIXES.iter().any(|p| name.starts_with(p)) {
        let replacement = policy
            .replacements
            .iter()
            .find(|(var, _)| *var == name)
            .map(|(_, key)| (*key).to_owned());
        return Var::Ignored(replacement);
    }
    Var::Other
}

fn is_user_home_var(name: &str) -> bool {
    if cfg!(windows) {
        name == "USERPROFILE"
    } else {
        name == "HOME"
    }
}

fn non_blank(value: OsString) -> Option<OsString> {
    let blank = value.to_str().is_some_and(|s| s.trim().is_empty());
    (!value.is_empty() && !blank).then_some(value)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{EnvPolicy, IgnoreReason, IgnoredVar, LaunchEnv};

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    const POLICY: EnvPolicy<'static> = EnvPolicy {
        provider_secret_names: &["OPENAI_API_KEY"],
        replacements: &[("PMT_MAX_TURNS", "agent.max_iterations")],
    };

    #[test]
    fn non_secret_pmt_var_has_no_effect_and_is_reported() {
        let base = [("PROMETHEUS_HOME", "/srv/pmt"), ("PMT_LOG", "debug")];
        let without = LaunchEnv::from_vars(vars(&base), &POLICY);
        let mut with_foo = base.to_vec();
        with_foo.push(("PMT_FOO", "bar"));
        let with = LaunchEnv::from_vars(vars(&with_foo), &POLICY);

        let ignored = [IgnoredVar {
            name: "PMT_FOO".to_owned(),
            reason: IgnoreReason::NotConfiguration { replacement: None },
        }];
        assert_eq!(with.ignored(), ignored);
        let mut stripped = with.clone();
        stripped.ignored.clear();
        assert_eq!(stripped, without, "PMT_FOO changed the launch config");
        assert_eq!(
            with.ignored()[0].doctor_message(),
            "ignored environment variable PMT_FOO (configuration lives in config.toml)"
        );
    }

    #[test]
    fn behaviour_var_warning_names_the_replacing_key() {
        let env = LaunchEnv::from_vars(vars(&[("PMT_MAX_TURNS", "5")]), &POLICY);
        assert_eq!(
            env.ignored()[0].warning(),
            "PMT_MAX_TURNS is ignored; set agent.max_iterations in config.toml"
        );
    }

    #[test]
    fn prometheus_prefixed_vars_are_ignored_too() {
        let env = LaunchEnv::from_vars(vars(&[("PROMETHEUS_MODEL", "x")]), &POLICY);
        assert_eq!(env.ignored()[0].name, "PROMETHEUS_MODEL");
    }

    #[test]
    fn only_accepted_values_are_kept() {
        let env = LaunchEnv::from_vars(
            vars(&[
                ("PROMETHEUS_HOME", "/srv/pmt"),
                ("RUST_LOG", "info"),
                ("PMT_SECRET_GITHUB_TOKEN", "ghp_abcdef"),
                ("OPENAI_API_KEY", "sk-native"),
                ("ANTHROPIC_API_KEY", "sk-unregistered"),
                ("EDITOR", "vi"),
            ]),
            &POLICY,
        );
        assert_eq!(env.home().unwrap().to_str(), Some("/srv/pmt"));
        assert_eq!(env.log_filter(), Some("info"));
        assert_eq!(env.secret("GITHUB_TOKEN").unwrap().expose(), "ghp_abcdef");
        assert_eq!(env.secret("OPENAI_API_KEY").unwrap().expose(), "sk-native");
        assert!(env.secret("ANTHROPIC_API_KEY").is_none());
        assert!(env.ignored().is_empty());
    }

    #[test]
    fn pmt_log_wins_over_rust_log() {
        let env =
            LaunchEnv::from_vars(vars(&[("RUST_LOG", "info"), ("PMT_LOG", "trace")]), &POLICY);
        assert_eq!(env.log_filter(), Some("trace"));
    }

    #[test]
    fn generic_secret_wins_over_provider_native_name() {
        let env = LaunchEnv::from_vars(
            vars(&[
                ("OPENAI_API_KEY", "native"),
                ("PMT_SECRET_OPENAI_API_KEY", "generic"),
            ]),
            &POLICY,
        );
        assert_eq!(env.secret("OPENAI_API_KEY").unwrap().expose(), "generic");
    }

    #[test]
    fn blank_home_counts_as_unset_and_empty_secret_name_is_ignored() {
        let env = LaunchEnv::from_vars(
            vars(&[("PROMETHEUS_HOME", "  "), ("PMT_SECRET_", "x")]),
            &POLICY,
        );
        assert!(env.home().is_none());
        assert_eq!(env.ignored()[0].name, "PMT_SECRET_");
    }

    #[test]
    fn secrets_never_appear_in_debug_output() {
        let env = LaunchEnv::from_vars(vars(&[("PMT_SECRET_X", "super-secret-value")]), &POLICY);
        assert!(!format!("{env:?}").contains("super-secret-value"));
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_secret_is_ignored_with_reason() {
        use std::os::unix::ffi::OsStringExt;
        let env = LaunchEnv::from_vars(
            [(
                OsString::from("PMT_SECRET_X"),
                OsString::from_vec(vec![0xff, 0xfe]),
            )],
            &POLICY,
        );
        assert!(env.secret("X").is_none());
        assert_eq!(env.ignored()[0].reason, IgnoreReason::NotUnicode);
    }

    #[test]
    fn process_environment_is_read_only_once() {
        // The only test that calls `from_process`: the guard is process-wide.
        assert!(LaunchEnv::from_process(&POLICY).is_ok());
        let err = LaunchEnv::from_process(&POLICY).unwrap_err();
        assert_eq!(err.code().code, "launch_env.reread");
    }
}
