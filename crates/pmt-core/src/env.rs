//! The launch environment: the process environment, read exactly once at
//! startup into an immutable [`LaunchEnv`] (spec: workspace-architecture,
//! Launch environment is read once; configuration-and-profiles, Environment
//! is secrets-only).
//!
//! Only the home override, the log filter, secrets and the OS directories
//! needed to find the default home are read. Every other `PMT_*` or
//! `PROMETHEUS_*` variable is recorded as ignored so startup can WARN and
//! `prometheus doctor` can report it; it never changes behaviour, because
//! behaviour lives in typed configuration.
//!
//! The remaining variables (neither secrets nor Prometheus-owned) are kept
//! verbatim as [`LaunchEnv::inherited`] values. Prometheus never reads them
//! for its own behaviour; they are the only source a child environment may
//! copy from (its fixed allowlist and `terminal.env_passthrough`), so a
//! child never sees a secret or the parent's whole environment. The one
//! exception is two machine facts sign-in flows need: whether this is an
//! SSH session and whether a graphical display exists.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
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
    /// `VISUAL`, else `EDITOR` (standard terminal variables).
    editor: Option<String>,
    /// Keyed by variable name (`PMT_SECRET_X` or a provider-native name).
    secrets: BTreeMap<String, SecretString>,
    /// Neither secret nor Prometheus-owned: forwardable to children only.
    inherited: BTreeMap<String, OsString>,
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
        let mut visual = None;
        let mut editor = None;
        for (name, value) in vars {
            let name = name.to_string_lossy().into_owned();
            let class = classify(&name, policy);
            if class.is_inheritable() {
                env.inherited.insert(name.clone(), value.clone());
            }
            match class {
                Var::Home => env.home = non_blank(value).map(PathBuf::from),
                Var::UserHome => env.user_home = non_blank(value).map(PathBuf::from),
                Var::LocalAppData => env.local_app_data = non_blank(value).map(PathBuf::from),
                Var::PmtLog => pmt_log = env.unicode(name, value),
                Var::RustLog => rust_log = env.unicode(name, value),
                Var::Visual => visual = env.unicode(name, value).filter(|v| !v.trim().is_empty()),
                Var::Editor => editor = env.unicode(name, value).filter(|v| !v.trim().is_empty()),
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
        env.editor = visual.or(editor);
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

    /// The user's editor command: `VISUAL`, else `EDITOR` (may carry
    /// arguments, for example `code --wait`).
    #[must_use]
    pub fn editor(&self) -> Option<&str> {
        self.editor.as_deref()
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

    /// The launch value of `name` when it is neither a secret nor a
    /// Prometheus-owned variable (`PMT_*`, `PROMETHEUS_*`, the log filters).
    /// Only child-environment builders use it. Names are matched exactly,
    /// except on Windows where environment names are case-insensitive.
    #[must_use]
    pub fn inherited(&self, name: &str) -> Option<&OsStr> {
        if let Some(value) = self.inherited.get(name) {
            return Some(value);
        }
        if cfg!(windows) {
            return self
                .inherited
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_os_str());
        }
        None
    }

    /// Whether this process runs inside an SSH session (`SSH_CONNECTION`,
    /// `SSH_CLIENT` or `SSH_TTY` set): a machine fact sign-in flows use to
    /// print a port-forwarding hint instead of opening a browser.
    #[must_use]
    pub fn is_ssh_session(&self) -> bool {
        ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
            .iter()
            .any(|name| self.inherited(name).is_some_and(|v| !v.is_empty()))
    }

    /// Whether a graphical browser can be opened: always on macOS and
    /// Windows, elsewhere only with `DISPLAY` or `WAYLAND_DISPLAY` set.
    #[must_use]
    pub fn has_graphical_display(&self) -> bool {
        cfg!(any(target_os = "macos", windows))
            || ["DISPLAY", "WAYLAND_DISPLAY"]
                .iter()
                .any(|name| self.inherited(name).is_some_and(|v| !v.is_empty()))
    }

    /// Every [`LaunchEnv::inherited`] variable, sorted by name.
    pub fn inherited_vars(&self) -> impl Iterator<Item = (&str, &OsStr)> {
        self.inherited
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_os_str()))
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
    Visual,
    Editor,
    Secret,
    Ignored(Option<String>),
    Other,
}

impl Var {
    /// Whether a child environment may copy the variable: everything but
    /// secrets and Prometheus' own variables.
    fn is_inheritable(&self) -> bool {
        matches!(
            self,
            Self::UserHome | Self::LocalAppData | Self::Visual | Self::Editor | Self::Other
        )
    }
}

fn classify(name: &str, policy: &EnvPolicy<'_>) -> Var {
    match name {
        HOME_VAR => return Var::Home,
        PMT_LOG => return Var::PmtLog,
        RUST_LOG => return Var::RustLog,
        "LOCALAPPDATA" => return Var::LocalAppData,
        "VISUAL" => return Var::Visual,
        "EDITOR" => return Var::Editor,
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
    fn children_may_inherit_only_non_secret_foreign_variables() {
        let env = LaunchEnv::from_vars(
            vars(&[
                ("PATH", "/usr/bin"),
                ("JAVA_HOME", "/opt/jdk"),
                ("OPENAI_API_KEY", "sk-native"),
                ("PMT_SECRET_TOKEN", "tok-123456"),
                ("PMT_FOO", "bar"),
                ("PROMETHEUS_HOME", "/srv/pmt"),
                ("RUST_LOG", "debug"),
            ]),
            &POLICY,
        );
        let names: Vec<&str> = env.inherited_vars().map(|(name, _)| name).collect();
        assert_eq!(names, ["JAVA_HOME", "PATH"]);
        assert_eq!(env.inherited("JAVA_HOME"), Some("/opt/jdk".as_ref()));
        assert_eq!(env.inherited("OPENAI_API_KEY"), None);
    }

    #[test]
    fn editor_is_visual_then_editor() {
        let both = LaunchEnv::from_vars(
            vars(&[("EDITOR", "vi"), ("VISUAL", "code --wait")]),
            &POLICY,
        );
        assert_eq!(both.editor(), Some("code --wait"));
        let editor_only =
            LaunchEnv::from_vars(vars(&[("EDITOR", "nano"), ("VISUAL", " ")]), &POLICY);
        assert_eq!(editor_only.editor(), Some("nano"));
        assert_eq!(LaunchEnv::from_vars(vars(&[]), &POLICY).editor(), None);
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
