//! Windows local shell resolution against the real machine (spec:
//! execution-environments, Windows local shell): with only the WSL launcher
//! on `PATH` and no Git for Windows, the backend falls back to PowerShell
//! and never runs `System32\bash.exe`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::schema::{
    DegradedFeature, SandboxConfig, SandboxMode, TerminalConfig, WindowsShell,
};
use pmt_core::env::{EnvPolicy, LaunchEnv};
use pmt_core::secret::Redactor;
use pmt_sandbox::{Options, Paths, Policy, Sandbox};
use pmt_tools::terminal::env::{ChildEnv, EnvRequest};
use pmt_tools::terminal::guard::GuardChain;
use pmt_tools::terminal::shell::ShellKind;
use pmt_tools::terminal::windows_shell::{Probe, resolve};
use pmt_tools::terminal::{LocalSetup, LocalTerminal, Status, TerminalArgs, spill};

struct Machine {
    _root: tempfile::TempDir,
    base: PathBuf,
    launch: LaunchEnv,
}

/// The real environment with `PATH` holding only a fake WSL launcher
/// (`<tmp>\Windows\System32\bash.exe`).
fn machine_with_only_wsl_bash() -> Machine {
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let base = std::fs::canonicalize(root.path()).unwrap();
    let system32 = base.join(r"Windows\System32");
    std::fs::create_dir_all(&system32).unwrap();
    std::fs::write(system32.join("bash.exe"), "not a real program").unwrap();
    let vars = std::env::vars_os()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("PATH"))
        .chain([(OsString::from("PATH"), system32.into_os_string())]);
    let launch = LaunchEnv::from_vars(vars, &EnvPolicy::default());
    Machine {
        _root: root,
        base,
        launch,
    }
}

fn no_git_for_windows(launch: &LaunchEnv) -> Probe {
    Probe {
        git_bash: Vec::new(),
        ..Probe::from_launch(launch, None)
    }
}

#[test]
fn only_wsl_bash_on_path_uses_powershell() {
    let machine = machine_with_only_wsl_bash();
    let shell = resolve(WindowsShell::Auto, &no_git_for_windows(&machine.launch)).unwrap();
    assert!(
        matches!(shell.kind, ShellKind::Pwsh | ShellKind::PowerShell),
        "{shell:?}"
    );
    assert!(
        !shell.path.starts_with(&machine.base),
        "picked the WSL launcher: {shell:?}"
    );
    assert!(
        shell
            .notice
            .unwrap()
            .contains("POSIX shell syntax is unavailable")
    );
}

#[test]
fn explicit_git_bash_without_git_is_not_found() {
    let machine = machine_with_only_wsl_bash();
    let err = resolve(WindowsShell::GitBash, &no_git_for_windows(&machine.launch)).unwrap_err();
    assert_eq!(err.message(), "git-bash not found");
    assert!(err.hint().unwrap().contains("Git for Windows"));
}

#[tokio::test]
async fn commands_run_in_the_fallback_shell() {
    let machine = machine_with_only_wsl_bash();
    let shell = resolve(WindowsShell::Auto, &no_git_for_windows(&machine.launch)).unwrap();
    let work = machine.base.join("work");
    let scratch = machine.base.join("scratch");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&scratch).unwrap();
    let paths = Paths {
        user_home: machine.base.clone(),
        prometheus_home: machine.base.join(".prometheus"),
        workspace: work.clone(),
        scratch: vec![scratch.clone()],
    };
    let policy = Policy::from_capabilities(pmt_sandbox::BASELINE, &paths).unwrap();
    // Confinement itself is covered by the pmt-sandbox Windows tests; this
    // checks the shell plumbing (encoded command, exit code, cwd tracking).
    let config = SandboxConfig {
        mode: SandboxMode::Off,
        allow_degraded: vec![DegradedFeature::All],
    };
    let sandbox = Sandbox::with_options(policy, &config, Options::default()).unwrap();
    let mut env = ChildEnv::build(&machine.launch, EnvRequest::default());
    env.set_temp_dir(&scratch);
    let setup = LocalSetup {
        terminal: TerminalConfig::default(),
        max_chars: 50_000,
        kill_grace: Duration::from_secs(2),
        sandbox: Arc::new(sandbox),
        env,
        shell,
        scratch,
        spill_dir: spill::session_dir(&machine.base, "ses_1"),
        start_cwd: work.clone(),
        user_home: Some(machine.base.clone()),
        redactor: Redactor::default(),
        hooks: Vec::new(),
        guards: GuardChain::default(),
        notices: Vec::new(),
        notify: None,
        clock: None,
    };
    let session = LocalTerminal::new(setup).unwrap();
    let first = session.run(TerminalArgs::new("Write-Output héllo")).await;
    assert_eq!(first.status, Status::Ok, "{first:?}");
    assert_eq!(first.output.trim(), "héllo");
    assert!(
        first
            .notices
            .iter()
            .any(|n| n.contains("POSIX shell syntax is unavailable"))
    );
    std::fs::create_dir(work.join("sub")).unwrap();
    let moved = session.run(TerminalArgs::new("Set-Location sub")).await;
    assert_eq!(
        moved.cwd.as_deref().map(PathBuf::from),
        Some(work.join("sub"))
    );
    let failed = session.run(TerminalArgs::new("cmd /c exit 3")).await;
    assert_eq!(failed.exit_code, Some(3));
}
