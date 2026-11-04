//! Shell completions loaded in real shells (spec: cli, Shell completions).

#![cfg(unix)]

mod common;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use self::common::{mock_home, ok, run};

/// `PATH` with the built binary's directory first, so the scripts' calls
/// to `prometheus` reach it.
fn path_with_binary() -> String {
    let dir = Path::new(common::BIN).parent().unwrap();
    format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn shell(program: &str, home: &Path, script: &str) -> std::process::Output {
    Command::new(program)
        .args(["-c", script])
        .env_clear()
        .env("PATH", path_with_binary())
        .env("HOME", common::user_home(home))
        .env("PROMETHEUS_HOME", home)
        .output()
        .unwrap_or_else(|e| panic!("{program} is required for this test: {e}"))
}

#[test]
fn the_bash_script_loads_and_completes_config_keys_and_profiles() {
    let home = mock_home();
    ok(&run(home.path(), &["profile", "create", "work"]));
    let out = shell(
        "bash",
        home.path(),
        r#"eval "$(prometheus completions bash)" || exit 9
complete -p prometheus
COMP_WORDS=(prometheus config set agent.max_it); COMP_CWORD=3; _prometheus
printf 'key:%s\n' "${COMPREPLY[@]}"
COMP_WORDS=(prometheus -p w); COMP_CWORD=2; _prometheus
printf 'profile:%s\n' "${COMPREPLY[@]}"
COMP_WORDS=(prometheus sess); COMP_CWORD=1; _prometheus
printf 'command:%s\n' "${COMPREPLY[@]}""#,
    );
    let text = ok(&out);
    assert!(
        text.contains("complete -o default -F _prometheus prometheus"),
        "{text}"
    );
    assert!(text.contains("key:agent.max_iterations\n"), "{text}");
    assert!(text.contains("profile:work\n"), "{text}");
    assert!(text.contains("command:sessions\n"), "{text}");
}

#[test]
fn the_zsh_script_loads_and_registers_its_function() {
    let home = mock_home();
    let out = shell(
        "zsh",
        home.path(),
        r"autoload -U compinit && compinit -u -D
source <(prometheus completions zsh) || exit 9
print -r -- ${_comps[prometheus]} ${_comps[pmt]}",
    );
    assert_eq!(ok(&out).trim(), "_prometheus _prometheus");
}

/// Scenario: Config key completion.
#[test]
fn zsh_completes_a_config_key_on_tab() {
    use expectrl::Expect as _;
    let home = mock_home();
    let user_home = common::user_home(home.path());
    std::fs::create_dir_all(&user_home).unwrap();
    let mut cmd = Command::new("/usr/bin/env");
    cmd.args(["-i", "TERM=xterm", "PS1=> "])
        .arg(format!("PATH={}", path_with_binary()))
        .arg(format!("HOME={}", user_home.display()))
        .arg(format!("PROMETHEUS_HOME={}", home.path().display()))
        .args(["zsh", "-f", "-i"]);
    let mut zsh = expectrl::session::OsSession::spawn(cmd).unwrap();
    zsh.set_expect_timeout(Some(Duration::from_secs(30)));
    zsh.send_line("autoload -U compinit && compinit -u -D && source <(prometheus completions zsh) && echo load''ed")
        .unwrap();
    zsh.expect("loaded").unwrap();
    zsh.send("prometheus config set agent.max_it\t").unwrap();
    zsh.expect("agent.max_iterations").unwrap();
    zsh.send("\x03").unwrap();
    zsh.send_line("exit").unwrap();
}

/// `__complete` reads only local files and stays fast on a large store.
#[test]
fn session_completion_is_fast_on_ten_thousand_sessions_and_writes_nothing() {
    let home = mock_home();
    ok(&run(home.path(), &["-z", "seed the store"]));
    let db = home.path().join("profiles/default/state.db");
    let mut conn = rusqlite::Connection::open(&db).unwrap();
    let tx = conn.transaction().unwrap();
    tx.execute_batch("CREATE TEMP TABLE one AS SELECT * FROM sessions LIMIT 1")
        .unwrap();
    for i in 1..=10_000 {
        let id = format!("ses_{i:026}");
        tx.execute(
            "UPDATE one SET id = ?1, root_session_id = ?1, title = ?2, title_source = 'user', last_activity_at = ?3",
            rusqlite::params![id, format!("title {i}"), i],
        )
        .unwrap();
        tx.execute("INSERT INTO sessions SELECT * FROM one", [])
            .unwrap();
    }
    tx.commit().unwrap();
    drop(conn);
    let before = common::files_under(home.path());
    let complete = || {
        Command::new(common::BIN)
            .args(["__complete", "zsh", "--", "prometheus", "-r", "title 999"])
            .env_clear()
            .env("PROMETHEUS_HOME", home.path())
            .env("HOME", common::user_home(home.path()))
            .output()
            .unwrap()
    };
    // The bound is per TAB press. The first run of the completion path in
    // a freshly linked binary also pays one-time costs (paging its code
    // in, the macOS executable scan) that no later TAB pays.
    ok(&complete());
    let started = Instant::now();
    let out = complete();
    let elapsed = started.elapsed();
    let text = ok(&out);
    assert!(text.contains("title 9999:ses_"), "{text}");
    assert!(elapsed < Duration::from_millis(200), "{elapsed:?}");
    assert_eq!(common::files_under(home.path()), before, "nothing written");
}
