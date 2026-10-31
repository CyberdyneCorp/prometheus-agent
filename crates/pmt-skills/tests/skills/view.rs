//! 17.4: `skills_list` / `skill_view`, load-time templates and inline
//! shell, skill secrets and settings, skill slash commands.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use pmt_core::secret::SecretString;
use pmt_skills::inline::{BoxFuture, InlineShell};
use pmt_skills::lockfile::{Entry, LockFile, Trust};
use pmt_skills::secrets::SecretPrompt;
use pmt_skills::slash::{self, SlashTable};
use serde_json::json;

use crate::support::{Profile, Session, add_skill, skill_md, write};

#[tokio::test]
async fn list_rows_and_category_filter() {
    let profile = Profile::new("");
    add_skill(&profile.skills(), "dev/git", "git", "Use git.");
    add_skill(&profile.skills(), "ops/deploy", "deploy", "Ship it.");
    let session = Session::open(&profile).await;
    let all = session.json("skills_list", json!({})).await;
    assert_eq!(all["count"], 2);
    assert_eq!(all["categories"], json!(["dev", "ops"]));
    assert_eq!(
        all["skills"][0],
        json!({"name": "git", "description": "Use git.", "category": "dev", "source": "profile"})
    );
    let ops = session
        .json("skills_list", json!({"category": "ops"}))
        .await;
    assert_eq!(ops["count"], 1);
    assert_eq!(ops["skills"][0]["name"], "deploy");
}

#[tokio::test]
async fn no_skill_tool_takes_offset_or_limit() {
    let profile = Profile::new("");
    let session = Session::open(&profile).await;
    for tool in ["skills_list", "skill_view"] {
        let result = session
            .call(tool, json!({"name": "x", "offset": 1, "limit": 2}))
            .await;
        assert!(!result.status.succeeded(), "{tool} accepted offset/limit");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn path_traversal_through_a_symlink_is_refused() {
    let profile = Profile::new("");
    add_skill(&profile.skills(), "foo", "foo", "Has a leak.");
    let refs = profile.skills().join("foo/references");
    std::fs::create_dir_all(&refs).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", refs.join("leak")).unwrap();
    let session = Session::open(&profile).await;
    let result = session
        .call(
            "skill_view",
            json!({"name": "foo", "file_path": "references/leak"}),
        )
        .await;
    assert!(!result.status.succeeded());
    let text = result.text_content();
    assert!(
        text.contains("file is outside the skill directory"),
        "{text}"
    );
    assert!(!text.contains("root:"), "content leaked: {text}");
    let dotdot = session
        .text(
            "skill_view",
            json!({"name": "foo", "file_path": "../../config.toml"}),
        )
        .await;
    assert!(
        dotdot.contains("file is outside the skill directory"),
        "{dotdot}"
    );
}

#[tokio::test]
async fn categorized_and_plain_names_resolve() {
    let profile = Profile::new("");
    add_skill(&profile.skills(), "mlops/axolotl", "axolotl", "Fine-tune.");
    let session = Session::open(&profile).await;
    for name in ["axolotl", "mlops/axolotl"] {
        assert_eq!(
            session.json("skill_view", json!({"name": name})).await["name"],
            "axolotl"
        );
    }
}

#[tokio::test]
async fn templates_substitute_known_tokens() {
    let profile = Profile::new("");
    write(
        &profile.skills(),
        "t/SKILL.md",
        "---\nname: t\ndescription: Templates.\n---\nrun ${PROMETHEUS_SKILL_DIR}/scripts/x.sh in ${HERMES_SESSION_ID}; keep ${UNKNOWN}\n",
    );
    let session = Session::open(&profile).await;
    let content = session.json("skill_view", json!({"name": "t"})).await["content"]
        .as_str()
        .unwrap()
        .to_owned();
    let dir = profile.skills().join("t");
    assert!(
        content.contains(&format!("run {}/scripts/x.sh", dir.display())),
        "{content}"
    );
    assert!(
        content.contains(&format!("in {};", session.id)),
        "{content}"
    );
    assert!(content.contains("keep ${UNKNOWN}"));
}

#[tokio::test]
async fn template_vars_can_be_turned_off() {
    let profile = Profile::new("[skills]\ntemplate_vars = false\n");
    write(
        &profile.skills(),
        "t/SKILL.md",
        "---\nname: t\ndescription: T.\n---\n${PROMETHEUS_SKILL_DIR}\n",
    );
    let session = Session::open(&profile).await;
    let view = session.json("skill_view", json!({"name": "t"})).await;
    assert!(
        view["content"]
            .as_str()
            .unwrap()
            .contains("${PROMETHEUS_SKILL_DIR}")
    );
}

/// Records every snippet it is asked to run.
#[derive(Default)]
struct Recording {
    ran: Mutex<Vec<String>>,
}

impl InlineShell for Recording {
    fn run<'a>(
        &'a self,
        command: &'a str,
        _: &'a Path,
        _: Duration,
    ) -> BoxFuture<'a, Result<String, String>> {
        self.ran
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(command.to_owned());
        Box::pin(async move { Ok(format!("ran {command}")) })
    }
}

fn whoami_skill(profile: &Profile, name: &str) {
    write(
        &profile.skills(),
        &format!("{name}/SKILL.md"),
        &format!("---\nname: {name}\ndescription: Who.\n---\nUser: !`whoami`\n"),
    );
}

#[tokio::test]
async fn community_skill_never_runs_inline_shell() {
    let profile = Profile::new("[skills]\ninline_shell = true\n");
    whoami_skill(&profile, "community");
    whoami_skill(&profile, "local");
    whoami_skill(&profile, "garbled");
    let lock = LockFile::in_dir(&profile.skills());
    for (name, trust) in [("community", Some(Trust::Community)), ("garbled", None)] {
        lock.upsert(Entry {
            name: name.into(),
            source: format!("someone/skills/{name}"),
            content_digest: "sha256:0".into(),
            trust,
            installed_at: "2026-01-01T00:00:00Z".into(),
        })
        .unwrap();
    }
    let shell = Arc::new(Recording::default());
    let mut setup = profile.setup(profile.session());
    setup.inline_shell = Some(shell.clone());
    let session = Session::start(&profile, setup).await;
    for name in ["community", "garbled"] {
        let view = session.json("skill_view", json!({"name": name})).await;
        assert!(
            view["content"]
                .as_str()
                .unwrap()
                .contains("User: !`whoami`"),
            "{view}"
        );
    }
    assert!(shell.ran.lock().unwrap().is_empty(), "a snippet ran");
    let local = session.json("skill_view", json!({"name": "local"})).await;
    assert!(
        local["content"]
            .as_str()
            .unwrap()
            .contains("User: ran whoami")
    );
    assert_eq!(*shell.ran.lock().unwrap(), ["whoami"]);
}

#[tokio::test]
async fn inline_shell_is_off_by_default() {
    let profile = Profile::new("");
    whoami_skill(&profile, "local");
    let shell = Arc::new(Recording::default());
    let mut setup = profile.setup(profile.session());
    setup.inline_shell = Some(shell.clone());
    let session = Session::start(&profile, setup).await;
    let view = session.json("skill_view", json!({"name": "local"})).await;
    assert!(view["content"].as_str().unwrap().contains("!`whoami`"));
    assert!(shell.ran.lock().unwrap().is_empty());
}

#[cfg(unix)]
mod sandboxed {
    use std::ffi::OsString;
    use std::sync::Arc;

    use pmt_config::schema::SandboxConfig;
    use pmt_core::capability::{BASELINE, CapabilitySet};
    use pmt_core::env::{EnvPolicy, LaunchEnv};
    use pmt_sandbox::{Options, Paths, Policy, Sandbox};
    use pmt_skills::inline::SandboxShell;
    use pmt_tools::terminal::env::{ChildEnv, EnvRequest};
    use pmt_tools::terminal::shell;
    use serde_json::json;

    use crate::support::{Profile, Session, write};

    #[tokio::test]
    async fn snippets_run_in_the_session_sandbox_in_the_skill_dir() {
        let profile =
            Profile::new("[skills]\ninline_shell = true\ninline_shell_timeout_ms = 300\n");
        write(
            &profile.skills(),
            "probe/SKILL.md",
            "---\nname: probe\ndescription: Probe.\n---\ncwd=!`pwd`\nfail=!`exit 3`\nslow=!`sleep 5`\n",
        );
        let id = profile.session();
        let setup = profile.setup(id);
        let skills = pmt_skills::SkillsSession::new(setup.clone());
        let workspace = profile.path("work").canonicalize().unwrap();
        let scratch = profile.path("scratch").canonicalize().unwrap();
        let paths = Paths {
            user_home: profile.path("user-home"),
            prometheus_home: profile.home.root().to_path_buf(),
            workspace,
            scratch: vec![scratch],
        };
        let mut caps = CapabilitySet::parse(BASELINE.iter().copied()).unwrap();
        caps = caps.union(&skills.sandbox_reads());
        let policy = Policy::from_set(&caps, &paths);
        let sandbox = Arc::new(
            Sandbox::with_options(policy, &SandboxConfig::default(), Options::default()).unwrap(),
        );
        let path = std::env::var_os("PATH").unwrap_or_default();
        let launch = LaunchEnv::from_vars(
            [(OsString::from("PATH"), path.clone())],
            &EnvPolicy {
                provider_secret_names: &[],
                replacements: &[],
            },
        );
        let env = ChildEnv::build(&launch, EnvRequest::default());
        let mut setup = setup;
        setup.inline_shell = Some(Arc::new(SandboxShell::new(
            sandbox,
            env,
            shell::resolve_unix(Some(&path)),
            None,
        )));
        let session = Session::start(&profile, setup).await;
        let view = session.json("skill_view", json!({"name": "probe"})).await;
        let content = view["content"].as_str().unwrap();
        let dir = profile.skills().join("probe").canonicalize().unwrap();
        assert!(
            content.contains(&format!("cwd={}", dir.display())),
            "{content}"
        );
        assert!(
            content.contains("fail=[inline-shell error: exit code 3: ]"),
            "{content}"
        );
        assert!(
            content.contains("slow=[inline-shell error: timed out after 300 ms]"),
            "{content}"
        );
    }
}

/// Answers every secret prompt with a fixed value.
struct Answer(&'static str, Mutex<Vec<String>>);

impl SecretPrompt for Answer {
    fn ask(&self, _: &str, name: &str, _: Option<&str>) -> Option<SecretString> {
        self.1.lock().unwrap().push(name.to_owned());
        Some(SecretString::new(self.0))
    }
}

const NOTION: &str = "---\nname: notion\ndescription: Notion pages.\nmetadata:\n  prometheus:\n    secrets:\n      - name: NOTION_API_KEY\n        prompt: Notion integration token\n---\nUse the API.\n";

#[tokio::test]
async fn secret_value_never_reaches_the_transcript() {
    let profile = Profile::new("");
    write(&profile.skills(), "notion/SKILL.md", NOTION);
    let answer = Arc::new(Answer("ntn_secret_value_123456", Mutex::new(Vec::new())));
    let mut setup = profile.setup(profile.session());
    setup.secret_prompt = Some(answer.clone());
    let session = Session::start(&profile, setup).await;
    // Unset secrets do not hide the skill.
    assert!(session.skills.index_text().unwrap().contains("notion"));
    let source = session.skills.secret_source();
    assert!(source().is_empty(), "nothing loaded yet");
    let result = session.call("skill_view", json!({"name": "notion"})).await;
    let text = result.text_content();
    assert!(text.contains("NOTION_API_KEY"), "{text}");
    assert!(!text.contains("ntn_secret_value_123456"), "{text}");
    assert_eq!(*answer.1.lock().unwrap(), ["NOTION_API_KEY"]);
    let stored = profile.scope.secrets().resolve("NOTION_API_KEY").unwrap();
    assert_eq!(stored.expose(), "ntn_secret_value_123456");
    // The loaded skill's secret now reaches the terminal, and only it.
    let granted: Vec<String> = source().into_iter().map(|(n, _)| n).collect();
    assert_eq!(granted, ["NOTION_API_KEY"]);
    // A second load finds it set and does not ask again.
    session.call("skill_view", json!({"name": "notion"})).await;
    assert_eq!(answer.1.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn missing_secret_without_a_prompt_names_the_command() {
    let profile = Profile::new("");
    write(&profile.skills(), "notion/SKILL.md", NOTION);
    let session = Session::open(&profile).await;
    let view = session.json("skill_view", json!({"name": "notion"})).await;
    let content = view["content"].as_str().unwrap();
    assert!(
        content.contains(
            "NOTION_API_KEY: not set; ask the user to run `prometheus secrets set NOTION_API_KEY`"
        ),
        "{content}"
    );
}

#[tokio::test]
async fn settings_are_injected_and_never_stored_as_secrets() {
    let profile = Profile::new("[skills.config.weather]\nunits = \"metric\"\n");
    write(
        &profile.skills(),
        "weather/SKILL.md",
        &skill_md(
            "weather",
            "Forecasts.",
            "config:\n  - key: units\n  - key: city\n    default: Lisbon\n  - key: api_base\n",
        ),
    );
    let session = Session::open(&profile).await;
    let content = session.json("skill_view", json!({"name": "weather"})).await["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        content.contains("[Skill config]\nunits = metric\ncity = Lisbon\napi_base = (not set)"),
        "{content}"
    );
    assert!(profile.scope.secrets().resolve("units").is_err());
    assert!(
        !profile.scope.dir().join("secrets.toml").exists()
            || !std::fs::read_to_string(profile.scope.dir().join("secrets.toml"))
                .unwrap()
                .contains("metric")
    );
}

#[tokio::test]
async fn view_bumps_the_counter() {
    let profile = Profile::new("");
    add_skill(&profile.skills(), "foo", "foo", "Foo.");
    let session = Session::open(&profile).await;
    for _ in 0..2 {
        session.json("skill_view", json!({"name": "foo"})).await;
    }
    let before = profile.store.skill_usage("foo").unwrap().unwrap();
    assert_eq!(before.view_count, 2);
    tokio::time::sleep(Duration::from_millis(5)).await;
    session.json("skill_view", json!({"name": "foo"})).await;
    let after = profile.store.skill_usage("foo").unwrap().unwrap();
    assert_eq!(after.view_count, 3);
    assert!(after.last_activity_at > before.last_activity_at);
}

#[tokio::test]
async fn built_in_name_wins() {
    let profile = Profile::new("");
    add_skill(&profile.skills(), "help", "help", "A skill named help.");
    add_skill(&profile.skills(), "dev/git_flow", "git_flow", "Branching.");
    let session = Session::open(&profile).await;
    let core = ["help", "/new", "q"];
    let table = slash::table(&session.skills, &core);
    assert!(!table.commands.contains_key("help"));
    assert_eq!(table.commands["git-flow"], "git_flow");
    assert_eq!(
        table.note("help").unwrap(),
        "slash command /help unavailable: name taken by a built-in command; use /skill help"
    );
    let message = slash::invoke(&session.skills, "help", "do it")
        .await
        .unwrap();
    assert!(
        message.contains("A skill named help") || message.contains("# help"),
        "{message}"
    );
    assert!(message.ends_with("User instruction: do it"));
    assert_eq!(
        profile
            .store
            .skill_usage("help")
            .unwrap()
            .unwrap()
            .use_count,
        1
    );
    let listing = pmt_skills::activation::run(&session.skills, "", &core)
        .unwrap()
        .text;
    assert!(
        listing.contains("slash command /help unavailable"),
        "{listing}"
    );
}

#[test]
fn slug_collisions_follow_precedence() {
    let profile = Profile::new("");
    add_skill(&profile.skills(), "a/deploy_app", "deploy_app", "First.");
    add_skill(&profile.skills(), "b/deploy-app", "deploy-app", "Second.");
    let id = profile.session();
    let session = pmt_skills::SkillsSession::new(profile.setup(id));
    let (catalog, config) = session.catalog();
    let table = SlashTable::new(session.visible(&catalog, &config), &[]);
    assert_eq!(table.commands.len(), 1);
    assert!(table.commands.contains_key("deploy-app"));
}
