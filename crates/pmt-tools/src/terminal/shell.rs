//! The shell commands run in, and the scripts that wrap each command
//! (spec: execution-environments, Shell startup files; Session cwd
//! tracking; Windows local shell).
//!
//! Commands run in a fresh non-interactive, non-login shell per call. The
//! "persistent shell" is a snapshot file: the init step sources `~/.bashrc`
//! (bash, `terminal.auto_source_bashrc`) and `terminal.shell_init_files`
//! once, with stdin closed and under `terminal.shell_init_timeout_secs`,
//! and dumps the resulting exported variables, functions and aliases. Each
//! command sources the snapshot, runs, writes its final directory to a cwd
//! file (`cwd_observed`) and, when `terminal.persistent_shell` is on, dumps
//! the snapshot again so `export`s and functions carry over. Secrets never
//! enter the snapshot. A fresh process per call keeps timeouts, kills and
//! output capture simple and leaves nothing running between calls.
//!
//! PowerShell has no such snapshot: persistence is degraded there.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Which shell language commands are written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// GNU bash (also Git Bash on Windows).
    Bash,
    /// A POSIX `sh`.
    Sh,
    /// PowerShell 7.
    Pwsh,
    /// Windows PowerShell 5.
    PowerShell,
}

impl ShellKind {
    /// Whether commands use POSIX shell syntax (and get the snapshot).
    #[must_use]
    pub fn is_posix(self) -> bool {
        matches!(self, Self::Bash | Self::Sh)
    }

    /// The name used in notices and doctor output.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Sh => "sh",
            Self::Pwsh => "pwsh",
            Self::PowerShell => "powershell",
        }
    }
}

/// A resolved shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    /// Its language.
    pub kind: ShellKind,
    /// Its executable.
    pub path: PathBuf,
    /// A one-time notice to show with the first result (for example that
    /// POSIX syntax is unavailable).
    pub notice: Option<String>,
}

/// The local shell on Unix: `bash` from `PATH`, else `/bin/bash`, else
/// `sh` from `PATH`, else `/bin/sh`.
#[must_use]
pub fn resolve_unix(path_var: Option<&OsStr>) -> Shell {
    let found = |name: &str, fallback: &str| {
        path_var
            .and_then(|p| find_executable(name, p, None))
            .or_else(|| {
                Path::new(fallback)
                    .is_file()
                    .then(|| PathBuf::from(fallback))
            })
    };
    match found("bash", "/bin/bash") {
        Some(path) => Shell {
            kind: ShellKind::Bash,
            path,
            notice: None,
        },
        None => Shell {
            kind: ShellKind::Sh,
            path: found("sh", "/bin/sh").unwrap_or_else(|| PathBuf::from("/bin/sh")),
            notice: None,
        },
    }
}

/// Every match of `name` on `path_var`, in order (with each `PATHEXT`
/// extension on Windows when `name` has none).
#[must_use]
pub fn find_all(name: &str, path_var: &OsStr, pathext: Option<&OsStr>) -> Vec<PathBuf> {
    let extensions: Vec<String> = match pathext {
        Some(ext) if Path::new(name).extension().is_none() => std::iter::once(String::new())
            .chain(
                ext.to_string_lossy()
                    .split(';')
                    .filter(|e| !e.is_empty())
                    .map(str::to_ascii_lowercase),
            )
            .collect(),
        _ => vec![String::new()],
    };
    std::env::split_paths(path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| {
            extensions
                .iter()
                .map(move |ext| dir.join(format!("{name}{ext}")))
        })
        .filter(|candidate| candidate.is_file())
        .collect()
}

/// The first match of [`find_all`].
#[must_use]
pub fn find_executable(name: &str, path_var: &OsStr, pathext: Option<&OsStr>) -> Option<PathBuf> {
    find_all(name, path_var, pathext).into_iter().next()
}

/// Files the session writes for the shell, all inside the scratch
/// directory the sandbox lets the child write.
#[derive(Debug, Clone)]
pub struct StateFiles {
    /// The environment snapshot.
    pub snapshot: PathBuf,
    /// Where each command writes its final directory.
    pub cwd: PathBuf,
}

impl StateFiles {
    /// The files for a session under `scratch`.
    #[must_use]
    pub fn in_dir(scratch: &Path) -> Self {
        Self {
            snapshot: scratch.join("pmt-shell-snapshot.sh"),
            cwd: scratch.join("pmt-shell-cwd"),
        }
    }
}

/// The snapshot dump for `kind`, run in a subshell with `$__pmt_hide` (the
/// secret names) unset.
fn dump(kind: ShellKind) -> &'static str {
    match kind {
        ShellKind::Bash => "{ export -p; declare -f; alias -p; }",
        _ => "export -p",
    }
}

/// The init step: sources each file (stdin closed), then writes the
/// snapshot. Arguments: snapshot path, hidden names, files.
#[must_use]
pub fn init_argv(
    shell: &Shell,
    files: &StateFiles,
    hidden: &[String],
    init_files: &[PathBuf],
) -> Vec<OsString> {
    let script = format!(
        "__pmt_snap=$1; __pmt_hide=$2; shift 2\n\
         for __pmt_f in \"$@\"; do . \"$__pmt_f\" </dev/null; done\n\
         ( unset $__pmt_hide; {dump} ) > \"$__pmt_snap.tmp\" 2>/dev/null && mv -f \"$__pmt_snap.tmp\" \"$__pmt_snap\"\n",
        dump = dump(shell.kind)
    );
    let mut argv = posix_prefix(shell);
    argv.extend([
        OsString::from(script),
        "pmt-shell-init".into(),
        files.snapshot.clone().into(),
        hidden.join(" ").into(),
    ]);
    argv.extend(init_files.iter().map(|f| f.clone().into_os_string()));
    argv
}

/// How one command runs.
#[derive(Debug, Clone)]
pub struct RunScript<'a> {
    /// The command line.
    pub command: &'a str,
    /// The directory it starts in.
    pub cwd: &'a Path,
    /// Source the snapshot first.
    pub use_snapshot: bool,
    /// Dump the snapshot again afterwards (`terminal.persistent_shell`).
    pub persist: bool,
    /// Report the final directory.
    pub track_cwd: bool,
    /// Secret names kept out of the snapshot.
    pub hidden: &'a [String],
}

/// The argv that runs `run` in `shell`.
#[must_use]
pub fn run_argv(shell: &Shell, files: &StateFiles, run: &RunScript<'_>) -> Vec<OsString> {
    if shell.kind.is_posix() {
        posix_run(shell, files, run)
    } else {
        powershell_run(shell, files, run)
    }
}

fn posix_prefix(shell: &Shell) -> Vec<OsString> {
    let mut argv = vec![shell.path.clone().into_os_string()];
    if shell.kind == ShellKind::Bash {
        argv.extend(["--noprofile".into(), "--norc".into()]);
    }
    argv.push("-c".into());
    argv
}

fn posix_run(shell: &Shell, files: &StateFiles, run: &RunScript<'_>) -> Vec<OsString> {
    let flag = |on: bool| if on { "1" } else { "0" };
    let script = format!(
        "if [ \"$5\" = 1 ] && [ -r \"$1\" ]; then . \"$1\" >/dev/null 2>&1; fi\n\
         shopt -s expand_aliases 2>/dev/null\n\
         __pmt_snap=$1; __pmt_cwdf=$2; __pmt_cmd=$4; __pmt_persist=$6; __pmt_track=$7; __pmt_hide=$8\n\
         cd -- \"$3\" || exit 126\n\
         shift 8\n\
         eval \"$__pmt_cmd\"\n\
         __pmt_rc=$?\n\
         if [ \"$__pmt_track\" = 1 ]; then pwd -P > \"$__pmt_cwdf\" 2>/dev/null; fi\n\
         if [ \"$__pmt_persist\" = 1 ]; then ( unset $__pmt_hide; {dump} ) > \"$__pmt_snap.tmp\" 2>/dev/null && mv -f \"$__pmt_snap.tmp\" \"$__pmt_snap\"; fi\n\
         exit $__pmt_rc\n",
        dump = dump(shell.kind)
    );
    let mut argv = posix_prefix(shell);
    argv.extend([
        OsString::from(script),
        "pmt-shell".into(),
        files.snapshot.clone().into(),
        files.cwd.clone().into(),
        run.cwd.as_os_str().to_owned(),
        run.command.into(),
        flag(run.use_snapshot).into(),
        flag(run.persist && run.use_snapshot).into(),
        flag(run.track_cwd).into(),
        run.hidden.join(" ").into(),
    ]);
    argv
}

fn powershell_run(shell: &Shell, files: &StateFiles, run: &RunScript<'_>) -> Vec<OsString> {
    let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let track = if run.track_cwd {
        format!(
            "(Get-Location).ProviderPath | Out-File -LiteralPath {} -Encoding utf8\n",
            quote(&files.cwd.to_string_lossy())
        )
    } else {
        String::new()
    };
    let script = format!(
        "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)\n\
         Set-Location -LiteralPath {cwd}\n\
         $global:LASTEXITCODE = 0\n\
         {command}\n\
         $__pmt_ok = $?; $__pmt_rc = $global:LASTEXITCODE\n\
         if (-not $__pmt_ok -and $__pmt_rc -eq 0) {{ $__pmt_rc = 1 }}\n\
         {track}exit $__pmt_rc\n",
        cwd = quote(&run.cwd.to_string_lossy()),
        command = run.command,
    );
    vec![
        shell.path.clone().into_os_string(),
        "-NoLogo".into(),
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-EncodedCommand".into(),
        encode_utf16_base64(&script).into(),
    ]
}

/// PowerShell's `-EncodedCommand` form: base64 of UTF-16LE.
fn encode_utf16_base64(script: &str) -> String {
    use base64::Engine as _;
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The hint the first result carries when the init step timed out.
pub const INIT_TIMEOUT_HINT: &str = "shell init timed out; guard interactive-only code in your rc file with: case $- in *i*) ;; *) return;; esac";
