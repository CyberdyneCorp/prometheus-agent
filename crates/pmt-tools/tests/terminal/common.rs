//! A session on the local backend over a temporary user home, workspace,
//! scratch and profile directory, all under the target directory.

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::schema::{SandboxConfig, TerminalConfig};
use pmt_core::env::{EnvPolicy, LaunchEnv};
use pmt_core::secret::{Redactor, SecretString};
use pmt_sandbox::{Options, Paths, Policy, Sandbox, StrengthReport};
use pmt_tools::terminal::env::{ChildEnv, EnvRequest};
use pmt_tools::terminal::guard::GuardChain;
use pmt_tools::terminal::process::{Clock, Completion};
use pmt_tools::terminal::{LocalSetup, LocalTerminal, TerminalArgs, TerminalResult, shell, spill};
use tokio::sync::mpsc;

/// A secret the parent holds; it must never reach a child.
pub(crate) const BOT_TOKEN: &str = "123456:telegram-bot-token-value";
/// A secret the redactor knows.
pub(crate) const KNOWN_SECRET: &str = "hunter2-known-secret";

pub(crate) struct Fixture {
    _root: tempfile::TempDir,
    pub(crate) home: PathBuf,
    pub(crate) work: PathBuf,
    pub(crate) outside: PathBuf,
    pub(crate) scratch: PathBuf,
    pub(crate) profile: PathBuf,
    pub(crate) paths: Paths,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let base = fs::canonicalize(root.path()).unwrap();
        let home = base.join("home");
        let work = home.join("work");
        let outside = base.join("outside");
        let profile = home.join(".prometheus/profiles/default");
        let scratch = profile.join("cache/scratch/ses_1");
        // The scratch directory exists before the sandbox is built: Landlock
        // rules need existing paths.
        for dir in [&work, &outside, &profile, &scratch] {
            fs::create_dir_all(dir).unwrap();
        }
        let paths = Paths {
            user_home: home.clone(),
            prometheus_home: home.join(".prometheus"),
            workspace: work.clone(),
            scratch: vec![scratch.clone()],
        };
        Self {
            _root: root,
            home,
            work,
            outside,
            scratch,
            profile,
            paths,
        }
    }

    /// The launch environment: the test's real `PATH` plus parent secrets.
    pub(crate) fn launch(&self, extra: &[(&str, &str)]) -> LaunchEnv {
        let mut vars: Vec<(OsString, OsString)> = vec![
            ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
            ("HOME".into(), self.home.clone().into()),
            ("LANG".into(), "C".into()),
            ("TELEGRAM_BOT_TOKEN".into(), BOT_TOKEN.into()),
            (
                "OPENAI_API_KEY".into(),
                "sk-parent-openai-key-123456".into(),
            ),
            ("PMT_SECRET_GITHUB".into(), "ghp_parent_secret_value".into()),
        ];
        vars.extend(
            extra
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        );
        LaunchEnv::from_vars(
            vars,
            &EnvPolicy {
                provider_secret_names: &["OPENAI_API_KEY"],
                replacements: &[],
            },
        )
    }

    pub(crate) fn policy(&self, extra: &[&str]) -> Policy {
        let caps: Vec<&str> = pmt_sandbox::BASELINE
            .iter()
            .copied()
            .chain(extra.iter().copied())
            .collect();
        Policy::from_capabilities(&caps, &self.paths).unwrap()
    }

    pub(crate) fn sandbox(&self, extra: &[&str], report: Option<StrengthReport>) -> Arc<Sandbox> {
        let options = Options {
            launcher: None,
            report,
        };
        Arc::new(
            Sandbox::with_options(self.policy(extra), &SandboxConfig::default(), options).unwrap(),
        )
    }

    pub(crate) fn setup(
        &self,
        terminal: TerminalConfig,
        env: ChildEnv,
        sandbox: Arc<Sandbox>,
    ) -> LocalSetup {
        let path = env.get("PATH").map(ToOwned::to_owned);
        LocalSetup {
            terminal,
            max_chars: 50_000,
            kill_grace: Duration::from_secs(2),
            sandbox,
            env,
            shell: shell::resolve_unix(path.as_deref()),
            scratch: self.scratch.clone(),
            spill_dir: spill::session_dir(&self.profile, "ses_1"),
            start_cwd: self.work.clone(),
            user_home: Some(self.home.clone()),
            redactor: Redactor::new([&SecretString::new(KNOWN_SECRET)]),
            hooks: Vec::new(),
            guards: GuardChain::default(),
            notices: Vec::new(),
            notify: None,
            clock: None,
        }
    }

    /// A session with the default terminal settings and the baseline
    /// capabilities.
    pub(crate) fn session(&self) -> LocalTerminal {
        self.session_with(TerminalConfig::default(), &[])
    }

    pub(crate) fn session_with(&self, terminal: TerminalConfig, caps: &[&str]) -> LocalTerminal {
        let env = self.child_env(&terminal.env_passthrough, &[]);
        LocalTerminal::new(self.setup(terminal, env, self.sandbox(caps, None))).unwrap()
    }

    pub(crate) fn child_env(&self, passthrough: &[String], extra: &[(&str, &str)]) -> ChildEnv {
        let mut env = ChildEnv::build(
            &self.launch(extra),
            EnvRequest {
                passthrough,
                ..EnvRequest::default()
            },
        );
        env.set_temp_dir(&self.scratch);
        env
    }
}

/// A session wired to a completion channel and an optional manual clock.
pub(crate) fn with_notifications(
    fixture: &Fixture,
    clock: Option<Clock>,
) -> (LocalTerminal, mpsc::UnboundedReceiver<Completion>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let env = fixture.child_env(&[], &[]);
    let mut setup = fixture.setup(TerminalConfig::default(), env, fixture.sandbox(&[], None));
    setup.notify = Some(tx);
    setup.clock = clock;
    (LocalTerminal::new(setup).unwrap(), rx)
}

pub(crate) async fn run(session: &LocalTerminal, command: &str) -> TerminalResult {
    session.run(TerminalArgs::new(command)).await
}

/// A clock the test moves forward by hand.
#[derive(Clone)]
pub(crate) struct ManualClock {
    base: std::time::Instant,
    offset: Arc<std::sync::Mutex<Duration>>,
}

impl ManualClock {
    pub(crate) fn new() -> Self {
        Self {
            base: std::time::Instant::now(),
            offset: Arc::default(),
        }
    }

    pub(crate) fn advance(&self, by: Duration) {
        *self.offset.lock().unwrap() += by;
    }

    pub(crate) fn clock(&self) -> Clock {
        let this = self.clone();
        Arc::new(move || this.base + *this.offset.lock().unwrap())
    }
}
