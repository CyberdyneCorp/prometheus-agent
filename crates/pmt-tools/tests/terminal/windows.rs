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
use pmt_tools::terminal::shell::{Shell, ShellKind};
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

/// A session in `<base>\work` running `shell`. Confinement itself is
/// covered by the pmt-sandbox Windows tests; these check the shell plumbing
/// (encoding, exit codes, cwd tracking), so the sandbox is off.
fn session(machine: &Machine, launch: &LaunchEnv, shell: Shell) -> LocalTerminal {
    let work = machine.base.join("work");
    let scratch = machine.base.join("scratch");
    std::fs::create_dir_all(work.join("sub")).unwrap();
    std::fs::create_dir_all(&scratch).unwrap();
    let paths = Paths {
        user_home: machine.base.clone(),
        prometheus_home: machine.base.join(".prometheus"),
        workspace: work.clone(),
        scratch: vec![scratch.clone()],
    };
    let policy = Policy::from_capabilities(pmt_sandbox::BASELINE, &paths).unwrap();
    let config = SandboxConfig {
        mode: SandboxMode::Off,
        allow_degraded: vec![DegradedFeature::All],
    };
    let sandbox = Sandbox::with_options(policy, &config, Options::default()).unwrap();
    let mut env = ChildEnv::build(launch, EnvRequest::default());
    env.set_temp_dir(&scratch);
    LocalTerminal::new(LocalSetup {
        terminal: TerminalConfig::default(),
        max_chars: 50_000,
        kill_grace: Duration::from_secs(2),
        sandbox: Arc::new(sandbox),
        env,
        shell,
        scratch,
        spill_dir: spill::session_dir(&machine.base, "ses_1"),
        // Canonical, hence verbatim (`\\?\D:\..`), as a resolved cwd can be.
        start_cwd: work,
        user_home: Some(machine.base.clone()),
        redactor: Redactor::default(),
        hooks: Vec::new(),
        guards: GuardChain::default(),
        notices: Vec::new(),
        notify: None,
        clock: None,
    })
    .unwrap()
}

/// `cd sub` is reported in the plain Windows form and becomes the session
/// cwd (the next command starts there).
async fn assert_cwd_follows(session: &LocalTerminal, machine: &Machine, cd: &str) {
    let sub = machine.base.join(r"work\sub");
    let moved = session.run(TerminalArgs::new(cd)).await;
    assert_eq!(moved.status, Status::Ok, "{moved:?}");
    let reported = PathBuf::from(moved.cwd.clone().expect("cwd reported"));
    assert!(
        !reported.to_string_lossy().starts_with(r"\\?\"),
        "{reported:?}"
    );
    assert_eq!(std::fs::canonicalize(&reported).unwrap(), sub);
    assert_eq!(std::fs::canonicalize(session.cwd()).unwrap(), sub);
}

#[tokio::test]
async fn commands_run_in_the_fallback_shell() {
    let machine = machine_with_only_wsl_bash();
    let shell = resolve(WindowsShell::Auto, &no_git_for_windows(&machine.launch)).unwrap();
    let session = session(&machine, &machine.launch, shell);
    let first = session.run(TerminalArgs::new("Write-Output héllo")).await;
    assert_eq!(first.status, Status::Ok, "{first:?}");
    assert_eq!(first.output.trim(), "héllo");
    assert!(
        first
            .notices
            .iter()
            .any(|n| n.contains("POSIX shell syntax is unavailable"))
    );
    assert_cwd_follows(&session, &machine, "Set-Location sub").await;
    let here = session
        .run(TerminalArgs::new("(Get-Location).ProviderPath"))
        .await;
    assert_eq!(here.output.trim(), session.cwd().to_string_lossy());
    // `PATH` holds only the fake WSL launcher, so name cmd.exe in full.
    let failed = session
        .run(TerminalArgs::new(
            "& ([Environment]::SystemDirectory + '\\cmd.exe') /c exit 3",
        ))
        .await;
    assert_eq!(failed.exit_code, Some(3), "{failed:?}");
    let missing = session
        .run(TerminalArgs::new("Get-Item no-such-item"))
        .await;
    assert_eq!(missing.status, Status::Error, "{missing:?}");
    assert!(
        missing.output.contains("no-such-item") && !missing.output.contains("CLIXML"),
        "errors are plain text: {missing:?}"
    );
}

/// Git Bash (installed on the CI runners) tracks the cwd too: MSYS paths
/// are not what the session spawns in.
#[tokio::test]
async fn git_bash_tracks_the_cwd() {
    let machine = machine_with_only_wsl_bash();
    let launch = LaunchEnv::from_vars(std::env::vars_os(), &EnvPolicy::default());
    let Ok(shell) = resolve(WindowsShell::GitBash, &Probe::from_launch(&launch, None)) else {
        assert!(
            std::env::var_os("CI").is_none(),
            "Git for Windows missing on CI"
        );
        eprintln!("skipped: Git for Windows is not installed");
        return;
    };
    let session = session(&machine, &launch, shell);
    let first = session.run(TerminalArgs::new("echo \"$((6 * 7))\"")).await;
    assert_eq!(first.output.trim(), "42", "{first:?}");
    assert_cwd_follows(&session, &machine, "cd sub").await;
    let here = session.run(TerminalArgs::new("basename \"$(pwd)\"")).await;
    assert_eq!(here.output.trim(), "sub", "{here:?}");
}
