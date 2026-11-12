//! `prometheus console` (spec: cli, Safe command console): only the
//! allowlisted commands run, parsed without a shell, on the console's
//! profile.

mod common;

use common::{command, mock_home, ok, run, run_cmd_with_stdin, stderr, stdout};

/// Scenario: Shell metacharacters refused. `logs; rm -rf /` is rejected
/// and nothing runs (a would-be victim file survives every shell form);
/// other commands are not available; allowlisted ones run on the `-p`
/// profile.
#[test]
fn the_console_runs_allowlisted_commands_and_refuses_shell_syntax() {
    let home = mock_home();
    ok(&run(home.path(), &["profile", "create", "work"]));
    ok(&run(
        home.path(),
        &["-p", "work", "config", "set", "model.default", "work-model"],
    ));
    let victim = home.path().join("victim.txt");
    std::fs::write(&victim, "keep me").unwrap();
    let victim = victim.display().to_string();
    let lines = [
        format!("logs; rm -rf {victim}"),
        format!("version | rm {victim}"),
        format!("status && rm {victim}"),
        format!("!rm {victim}"),
        format!("version > {victim}"),
        "config set model.default other".to_owned(),
        "secrets list".to_owned(),
        "config get model.default".to_owned(),
        "version".to_owned(),
        "exit".to_owned(),
    ];
    let out = run_cmd_with_stdin(
        command(home.path()).args(["-p", "work", "console"]),
        &format!("{}\n", lines.join("\n")),
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let err = stderr(&out);
    assert_eq!(
        err.matches("not available in the console").count(),
        7,
        "every refused line says so:\n{err}"
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("victim.txt")).unwrap(),
        "keep me"
    );
    let printed = stdout(&out);
    assert!(
        printed.contains("work-model"),
        "config get ran on -p work: {printed}"
    );
    assert!(printed.contains("prometheus "), "version ran: {printed}");
    let model = ok(&run(
        home.path(),
        &["-p", "work", "config", "get", "model.default"],
    ));
    assert_eq!(
        model.trim(),
        "work-model",
        "the refused config set changed nothing"
    );
}
