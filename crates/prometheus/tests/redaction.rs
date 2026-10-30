//! Known-secret redaction everywhere (spec: security-boundaries; task 8.5):
//! a value in the profile's secrets store, and its URL-encoded and base64
//! forms, never reach the model (tool results), the session store, the
//! log, the trajectory or a printed error; each holds
//! `[REDACTED:<NAME>]` instead.

mod common;

use std::path::Path;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::secret::{Redactor, SecretString};
use pmt_store::{NewMessage, NewSession, Role, Source, Store, Visibility};

/// A key with characters that URL-encoding and base64 change.
const KEY: &str = "sk-proj-Secret+Value/0123456789";
const MARKER: &str = "[REDACTED:OPENAI_API_KEY]";

fn scope(home: &Path) -> ProfileScope {
    let home = Home::open(home).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    let scope = ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap();
    scope
        .secrets()
        .store()
        .set("OPENAI_API_KEY", &SecretString::new(KEY))
        .unwrap();
    scope
}

fn url_encoded() -> String {
    KEY.replace('+', "%2B").replace('/', "%2F")
}

fn base64() -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(KEY)
}

/// Text holding the key in every form a command could print it.
fn leaked() -> String {
    format!("plain {KEY}\nurl {}\nb64 {}\n", url_encoded(), base64())
}

fn assert_clean(sink: &str, text: &str) {
    for form in [KEY.to_owned(), url_encoded(), base64()] {
        assert!(!text.contains(&form), "{sink} leaks {form}: {text}");
    }
    assert_eq!(text.matches(MARKER).count(), 3, "{sink}: {text}");
}

/// Spec scenario "Secret echoed by a command": the terminal tool result the
/// model receives is redacted.
#[cfg(unix)]
#[tokio::test]
async fn a_command_printing_the_key_reaches_the_model_redacted() {
    use std::sync::Arc;
    use std::time::Duration;

    use pmt_config::schema::{SandboxConfig, TerminalConfig};
    use pmt_core::capability::BASELINE;
    use pmt_core::env::{EnvPolicy, LaunchEnv};
    use pmt_sandbox::{Paths, Policy, Sandbox};
    use pmt_tools::terminal::env::{ChildEnv, EnvRequest};
    use pmt_tools::terminal::guard::GuardChain;
    use pmt_tools::terminal::{LocalSetup, LocalTerminal, TerminalArgs, shell, spill};

    let root = common::temp_home();
    let base = std::fs::canonicalize(root.path()).unwrap();
    let scope = scope(&base.join(".prometheus"));
    let work = base.join("work");
    let scratch = scope.path("cache/scratch/ses_1");
    for dir in [&work, &scratch] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let paths = Paths {
        user_home: base.clone(),
        prometheus_home: base.join(".prometheus"),
        workspace: work.clone(),
        scratch: vec![scratch.clone()],
    };
    let policy = Policy::from_capabilities(BASELINE, &paths).unwrap();
    let sandbox = Arc::new(Sandbox::new(policy, &SandboxConfig::default()).unwrap());
    let launch = LaunchEnv::from_vars(
        [("PATH".into(), std::env::var_os("PATH").unwrap_or_default())],
        &EnvPolicy::default(),
    );
    // The session granted `secrets.read:OPENAI_API_KEY` and the command
    // declared it, so the child holds the real value.
    let env = ChildEnv::build(
        &launch,
        EnvRequest {
            secrets: vec![("OPENAI_API_KEY".into(), SecretString::new(KEY))],
            ..EnvRequest::default()
        },
    );
    let terminal = LocalTerminal::new(LocalSetup {
        terminal: TerminalConfig::default(),
        max_chars: 50_000,
        kill_grace: Duration::from_secs(2),
        sandbox,
        shell: shell::resolve_unix(env.get("PATH")),
        env,
        scratch: scratch.clone(),
        spill_dir: spill::session_dir(scope.dir(), "ses_1"),
        start_cwd: work,
        user_home: Some(base.clone()),
        redactor: scope.secrets().redactor().unwrap(),
        hooks: Vec::new(),
        guards: GuardChain::default(),
        notices: Vec::new(),
        notify: None,
        clock: None,
    })
    .unwrap();
    let command = format!(
        "echo \"plain $OPENAI_API_KEY\"; echo 'url {}'; printf 'b64 '; printf '%s' \"$OPENAI_API_KEY\" | base64",
        url_encoded()
    );
    let result = terminal.run(TerminalArgs::new(&command)).await;
    assert_clean("tool result", &result.output);
    terminal.close().await;
}

/// The session store, the log and the trajectory get the same treatment
/// before anything is written.
#[test]
fn store_log_and_trajectory_hold_the_marker() {
    use pmt_obs::logs::{self, LogSetup};
    use pmt_obs::trajectory::payload::ToolResult;
    use pmt_obs::trajectory::writer::{Emit, open_for_session};
    use pmt_obs::trajectory::{Event, read};
    use tracing_subscriber::layer::SubscriberExt as _;

    let root = common::temp_home();
    let scope = scope(root.path());

    let store = Store::open(&scope).unwrap();
    let session = store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id;
    store
        .append_message(session, &NewMessage::new(Role::User, leaked()))
        .unwrap();
    let stored = store.messages(session, Visibility::All).unwrap();
    assert_clean("session store", stored[0].content.as_deref().unwrap());

    let (layer, handle) = logs::layer(LogSetup {
        home: Some(root.path().to_owned()),
        env_filter: None,
        redactor: Redactor::default(),
        stderr: false,
    });
    handle.register_profile("default", scope.secrets().redactor().unwrap());
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
        let span = tracing::info_span!("command", profile = "default");
        let _span = span.enter();
        tracing::info!("{}", leaked());
    });
    let log =
        std::fs::read_to_string(root.path().join("profiles/default/logs/prometheus.log")).unwrap();
    let record: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
    assert_clean("log", record["msg"].as_str().unwrap());

    let id = session.to_string();
    let writer = open_for_session(&scope, &id, &id).unwrap().unwrap();
    writer
        .blocking_record(Emit::now(Event::ToolResult(ToolResult {
            call_id: "tc_1".into(),
            tool: "terminal".into(),
            ok: true,
            duration_ms: None,
            output: Some(leaked()),
            error: None,
        })))
        .unwrap();
    let path = writer.close().unwrap();
    let lines = read::read_file(&path).unwrap();
    assert_clean("trajectory", lines[0].payload["output"].as_str().unwrap());
}

/// An error a command prints (here an id echoed back by `id.malformed`)
/// is redacted too, as text and as the `--json` document.
#[test]
fn a_printed_error_holds_the_marker() {
    let home = common::temp_home();
    let home = home.path();
    common::ok(&common::run_with_stdin(
        home,
        &["secrets", "set", "OPENAI_API_KEY"],
        &format!("{KEY}\n"),
    ));
    for json in [false, true] {
        let mut args = vec!["sessions", "show", KEY];
        if json {
            args.push("--json");
        }
        let out = common::run(home, &args);
        assert!(!out.status.success());
        let stderr = common::stderr(&out);
        assert!(!stderr.contains(KEY), "{stderr}");
        assert!(stderr.contains(MARKER), "{stderr}");
    }
}

/// `security.redact_secrets = false` is logged at WARN on every start.
#[test]
fn disabling_redaction_warns_on_every_start() {
    let home = common::temp_home();
    let home = home.path();
    common::ok(&common::run(
        home,
        &["config", "set", "security.redact_secrets", "false"],
    ));
    let warnings = || {
        std::fs::read_to_string(home.join("profiles/default/logs/prometheus.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| {
                l.contains(r#""level":"warn""#)
                    && l.contains(prometheus::startup::REDACTION_DISABLED_WARNING)
            })
            .count()
    };
    let before = warnings();
    common::ok(&common::run(home, &["config", "get", "approvals.mode"]));
    common::ok(&common::run(home, &["config", "get", "approvals.mode"]));
    assert_eq!(warnings(), before + 2);
}
