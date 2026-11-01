//! The AppContainer + Job Object backend on a real Windows host
//! (spec: security-boundaries, OS sandbox per platform).

use std::path::Path;
use std::process::Output;

use pmt_sandbox::{Feature, Level, Sandbox};

use crate::common::{Fixture, SECRET, confine};

/// Runs `cmd /C <line>` confined, from `cwd` (a directory the container may
/// use, as the terminal backend always sets).
fn cmd(sandbox: &Sandbox, cwd: &Path, line: &[&str]) -> Output {
    let mut command = sandbox.command("cmd.exe").unwrap();
    command.current_dir(cwd).arg("/C").args(line);
    command.output().unwrap()
}

fn write(sandbox: &Sandbox, cwd: &Path, path: &Path) -> Output {
    cmd(sandbox, cwd, &["echo", "x>", path.to_str().unwrap()])
}

#[test]
fn writes_outside_the_workspace_fail() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&[]));
    let inside = fixture.work.join("made-here.txt");
    let output = write(&sandbox, &fixture.work, &inside);
    assert!(output.status.success(), "{output:?}");
    assert!(inside.exists());

    let outside = fixture.outside.join("escaped.txt");
    let output = write(&sandbox, &fixture.work, &outside);
    assert!(!output.status.success(), "{output:?}");
    assert!(!outside.exists());
}

#[test]
fn credential_stores_stay_denied_under_a_broad_read_grant() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&["fs.read:~/**"]));
    let notes = fixture.home.join("notes.txt");
    let output = cmd(&sandbox, &fixture.work, &["type", notes.to_str().unwrap()]);
    assert!(output.status.success(), "{output:?}");
    for store in [".ssh\\id_rsa", ".aws\\credentials"] {
        let path = fixture.home.join(store);
        let output = cmd(&sandbox, &fixture.work, &["type", path.to_str().unwrap()]);
        assert!(!output.status.success(), "{output:?}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains(SECRET));
    }
}

#[test]
fn report_strength_per_feature() {
    let fixture = Fixture::new();
    let sandbox = confine(fixture.policy(&[]));
    let report = sandbox.report();
    assert_eq!(report.platform, "appcontainer");
    let levels: Vec<(Feature, Level)> = report
        .features
        .iter()
        .map(|f| (f.feature, f.level))
        .collect();
    assert_eq!(
        levels,
        [
            (Feature::Fs, Level::Enforced),
            (Feature::Net, Level::Enforced),
            (Feature::Process, Level::Enforced),
            (Feature::Syscalls, Level::Partial),
        ]
    );
    assert_eq!(report.enforcement().to_string(), "full");
}

fn acl(path: &Path) -> String {
    let output = std::process::Command::new("icacls")
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn acl_entries_are_revoked_when_the_sandbox_ends() {
    let fixture = Fixture::new();
    let sub = fixture.work.join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("file.txt"), "x").unwrap();
    let objects = [fixture.work.clone(), sub.clone(), sub.join("file.txt")];
    let before: Vec<String> = objects.iter().map(|p| entries(&acl(p))).collect();
    let sandbox = confine(fixture.policy(&[]));
    let during: Vec<String> = objects.iter().map(|p| acl(p)).collect();
    // AppContainer SIDs start with S-1-15-2-; the grant reaches children.
    assert!(
        during.iter().all(|a| a.contains("S-1-15-2-")),
        "{during:#?}"
    );
    drop(sandbox);
    let after: Vec<String> = objects.iter().map(|p| entries(&acl(p))).collect();
    assert!(after.iter().all(|a| !a.contains("S-1-15-2-")), "{after:#?}");
    assert_eq!(after, before, "the same principals keep the same rights");
}

/// The (principal, rights) entries of an `icacls` listing, without the
/// `(I)` inherited marker: when the parent directory's DACL is not marked
/// auto-inherited (seen on the Windows ARM runners), Windows rewrites
/// inherited entries as explicit copies whenever the DACL is set.
fn entries(listing: &str) -> String {
    listing.replace("(I)", "")
}

/// TEMPORARY diagnostics: how PowerShell behaves under the sandbox. Run
/// with --nocapture --ignored.
#[test]
#[ignore = "diagnostics"]
fn diag_shells() {
    let fixture = Fixture::new();
    std::fs::write(fixture.work.join("data.txt"), "line1\nline2\n").unwrap();
    let sandbox = confine(fixture.policy(&[]));
    let work = fixture.work.to_str().unwrap().to_owned();
    let line = |label: &str, body: &str| {
        format!(
            "$__t = [Diagnostics.Stopwatch]::StartNew(); try {{ {body}; [Console]::WriteLine('{label} ok ' + $__t.ElapsedMilliseconds) }} catch {{ [Console]::WriteLine('{label} FAIL ' + $__t.ElapsedMilliseconds + ' ' + $_.Exception.GetType().Name + ': ' + $_.Exception.Message) }}; "
        )
    };
    let mut probes = String::from("[Console]::WriteLine('start ' + [Environment]::CurrentDirectory + ' loc=' + $ExecutionContext.SessionState.Path.CurrentLocation); ");
    probes += &line("import-mgmt", "Import-Module Microsoft.PowerShell.Management -ErrorAction Stop");
    probes += &line("get-content-rel", "$null = Get-Content data.txt -ErrorAction Stop");
    probes += &line("get-content-abs", &format!("$null = Get-Content -LiteralPath '{work}\\data.txt' -ErrorAction Stop"));
    probes += &line("set-location", &format!("Set-Location -LiteralPath '{work}' -ErrorAction Stop"));
    probes += &line("psdrive", &format!("$null = New-PSDrive -Name PMTW -PSProvider FileSystem -Root '{work}' -ErrorAction Stop; Set-Location PMTW:\\ -ErrorAction Stop"));
    probes += &line("after-psdrive-rel", "$null = Get-Content data.txt -ErrorAction Stop");
    probes += &line("after-psdrive-gci", "$null = Get-ChildItem -ErrorAction Stop");
    probes += &line("native-cwd", "[Console]::WriteLine((cmd.exe /c cd))");
    probes += "[Console]::WriteLine('end loc=' + $ExecutionContext.SessionState.Path.CurrentLocation.ProviderPath)";
    let system_modules = r"C:\Windows\system32\WindowsPowerShell\v1.0\Modules";
    let envs: [(&str, Option<&str>); 3] = [
        ("inherited", None),
        ("unset", Some("")),
        ("system-only", Some(system_modules)),
    ];
    for exe in ["powershell.exe", "pwsh.exe"] {
        for (label, module_path) in envs {
            let mut command = sandbox.command(exe).unwrap();
            command
                .current_dir(&fixture.work)
                .args(["-NoProfile", "-NonInteractive", "-Command", &probes])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            match module_path {
                None => {}
                Some("") => {
                    command.env_remove("PSModulePath");
                }
                Some(value) => {
                    command.env("PSModulePath", value);
                }
            }
            let started = std::time::Instant::now();
            let mut child = command.spawn().unwrap();
            let deadline = started + std::time::Duration::from_secs(90);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break Some(status);
                }
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            };
            let output = child.wait_with_output().unwrap();
            println!(
                "=== {exe} PSModulePath={label} ancestors={:?} -> {status:?} in {:?}\nstdout: {}\nstderr: {}",
                std::env::var_os("PMT_DIAG_ANCESTORS"),
                started.elapsed(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
