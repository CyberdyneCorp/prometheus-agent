//! A job the model sets up through the `cronjob` tool cannot reach past
//! the calling session (spec: scheduled-jobs, Model tool surface; Script
//! wake gate and script credentials; Per-job working directory).

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{mock_home, ok, run, stderr};
use pmt_core::capability::{Capability, CapabilitySet, Places};
use pmt_core::id::SessionId;
use prometheus::cron_host::caller::Caller;
use serde_json::{Value, json};

fn cron_home() -> tempfile::TempDir {
    let home = mock_home();
    std::fs::write(
        home.path().join("profiles/default/mock.toml"),
        "[[rule]]\nrepeat = true\ntext = \"ok\"\n",
    )
    .unwrap();
    common::append_config(
        home.path(),
        "\n[providers.mock]\nscript = \"mock.toml\"\n\n[cron]\ntimezone = \"UTC\"\n",
    );
    let scripts = home.path().join("profiles/default/scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::write(scripts.join("poll.py"), "print('hi')\n").unwrap();
    home
}

fn scope(home: &Path) -> pmt_config::ProfileScope {
    use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
    let home = Home::open(home).unwrap();
    let profile = home
        .existing_profile(&ProfileName::parse("default").unwrap())
        .unwrap();
    ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap()
}

/// A tool port of a CLI session working in `workspace` with `caller`'s
/// reach.
struct Session {
    port: Arc<dyn pmt_agent::tools::CronPort>,
    id: SessionId,
    store: Arc<pmt_store::Store>,
}

impl Session {
    fn open(home: &Path, workspace: &Path, caller: Caller) -> Self {
        let scope = scope(home);
        let store = Arc::new(pmt_store::Store::open(&scope).unwrap());
        let mut new = pmt_store::NewSession::new(pmt_store::Source::Cli);
        new.cwd = Some(workspace.to_string_lossy().into_owned());
        let id = store.create_session(&new).unwrap().id;
        let launch = pmt_core::env::LaunchEnv::default();
        let port = prometheus::cron_host::tool::port(&scope, &launch, &store, true, caller);
        Self { port, id, store }
    }

    fn call(&self, args: Value) -> Value {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(self.port.call(self.id, args));
        serde_json::from_str(&result.text_content())
            .unwrap_or_else(|_| json!({"error": result.text_content()}))
    }

    /// `create` with `fields`, confirmed with the token it hands back.
    fn create_confirmed(&self, fields: &Value) -> Value {
        let mut args = json!({"action": "create", "schedule": "every 1d", "prompt": "x"});
        args.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let first = self.call(args.clone());
        let Some(token) = first["confirm_token"].as_str() else {
            return first;
        };
        args["confirm_token"] = json!(token);
        self.call(args)
    }
}

/// The reach of a session working in `workspace` that may write there,
/// holds `grants` and selects `tools`.
fn caller(workspace: &Path, grants: &[Capability], tools: &[&str]) -> Caller {
    let workspace = pmt_core::capability::canonical(workspace);
    let capabilities: CapabilitySet = std::iter::once(Capability::write_under(&workspace))
        .chain(grants.iter().cloned())
        .collect();
    Caller {
        tools: tools.iter().map(|t| (*t).to_owned()).collect(),
        capabilities,
        places: Places {
            workspace,
            ..Places::default()
        },
    }
}

fn error(reply: &Value) -> &str {
    reply["error"].as_str().unwrap_or_default()
}

/// Scenario: Tool-created job cannot take secrets or an interpreter the
/// session does not hold.
#[test]
fn a_model_job_cannot_take_secrets_or_a_writable_interpreter() {
    let home = cron_home();
    let workspace = home.path().join("acme");
    std::fs::create_dir_all(&workspace).unwrap();
    let interpreter = workspace.join("x.sh");
    std::fs::write(&interpreter, "#!/bin/sh\nenv\n").unwrap();
    let interpreter = interpreter.to_string_lossy().into_owned();
    let outside = std::env::current_exe().unwrap();
    let outside = outside.to_string_lossy();
    let plain = Session::open(home.path(), &workspace, caller(&workspace, &[], &[]));
    let refused = plain.create_confirmed(&json!({"script": "poll.py",
        "interpreter": interpreter, "script_secrets": ["SUDO_PASSWORD"]}));
    assert!(
        error(&refused).contains("this session's secrets.read grant"),
        "{refused}"
    );
    let refused = plain.create_confirmed(&json!({"script": "poll.py",
        "interpreter": interpreter}));
    assert!(
        error(&refused).contains(&format!(
            "interpreter {interpreter} is a file this session can write"
        )),
        "{refused}"
    );
    assert!(
        error(&refused).contains("prometheus cron add|edit"),
        "{refused}"
    );
    assert!(plain.store.cron_jobs().unwrap().is_empty(), "nothing saved");
    let sudo = Capability::SecretsRead("SUDO_PASSWORD".to_owned());
    let granted = Session::open(home.path(), &workspace, caller(&workspace, &[sudo], &[]));
    let created = granted.create_confirmed(&json!({"script": "poll.py",
        "interpreter": outside, "script_secrets": ["SUDO_PASSWORD"]}));
    assert_eq!(created["status"], "created", "{created}");
}

/// Scenario: Tool-created job cannot widen the session's toolsets.
#[test]
fn a_model_job_cannot_widen_the_sessions_toolsets() {
    let home = cron_home();
    let workspace = home.path().join("acme");
    std::fs::create_dir_all(&workspace).unwrap();
    let file_tools = prometheus::tools_cmd::expand_toolsets(&["file".to_owned()]).unwrap();
    let tools: Vec<&str> = file_tools.iter().map(String::as_str).collect();
    let session = Session::open(home.path(), &workspace, caller(&workspace, &[], &tools));
    let refused = session.create_confirmed(&json!({"toolsets": ["terminal", "file"]}));
    assert!(
        error(&refused).contains("give tools this session lacks"),
        "{refused}"
    );
    assert!(
        session.store.cron_jobs().unwrap().is_empty(),
        "nothing saved"
    );
    let created = session.create_confirmed(&json!({"toolsets": ["file"]}));
    assert_eq!(created["status"], "created", "{created}");
    // A person may still give a job wider toolsets.
    ok(&run(
        home.path(),
        &[
            "cron",
            "add",
            "every 1d",
            "--prompt",
            "y",
            "--toolsets",
            "terminal",
            "--yes",
        ],
    ));
}

/// Scenario: Repointed workdir link does not move a job.
#[cfg(unix)]
#[test]
fn a_repointed_workdir_link_does_not_move_the_job() {
    let home = cron_home();
    let workspace = home.path().join("acme");
    let sub = workspace.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let link = workspace.join("link");
    std::os::unix::fs::symlink(&sub, &link).unwrap();
    let session = Session::open(home.path(), &workspace, caller(&workspace, &[], &[]));
    let created = session.create_confirmed(&json!({"workdir": link.to_string_lossy()}));
    assert_eq!(created["status"], "created", "{created}");
    let real = pmt_core::capability::canonical(&sub);
    assert_eq!(
        created["job"]["workdir"],
        json!(real.to_string_lossy()),
        "{created}"
    );
    // The link repointed: the saved path is unaffected.
    let elsewhere = home.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
    // A directory along the saved path replaced by a link: runs refuse it.
    std::fs::remove_dir(&sub).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &sub).unwrap();
    let id = created["job"]["id"].as_str().unwrap();
    let out = run(home.path(), &["cron", "run", id]);
    let runs: Value =
        serde_json::from_str(&ok(&run(home.path(), &["--json", "cron", "runs", id]))).unwrap();
    let last = &runs["runs"][0];
    assert_eq!(last["status"], "blocked_config", "{runs} {}", stderr(&out));
    assert!(
        last["error"]
            .as_str()
            .is_some_and(|e| e.contains("now resolves to")),
        "{runs}"
    );
}
