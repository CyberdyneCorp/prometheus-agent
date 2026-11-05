//! What an approval's `explain` shows (spec: tool-system, Approval explain
//! and dry-run): the parsed command (programs, arguments, redirections,
//! pipelines), the rule that asked, the absolute paths and expanded globs
//! it would touch and, for commands with a preview form, that preview's
//! output run under a read-only sandbox with no network. Nothing of the
//! original command runs; the gate asks again afterwards.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use pmt_sandbox::Sandbox;

use super::Finding;
use super::shell::{self, Segment};
use crate::terminal::env::ChildEnv;

/// How long one preview may run.
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(15);

/// Most characters of preview output shown.
const PREVIEW_CHARS: usize = 4000;

/// Most paths listed per glob.
const LISTED: usize = 200;

/// Runs previews under a read-only, offline sandbox in the session's
/// working directory.
#[derive(Clone)]
pub struct Previewer {
    sandbox: Arc<Sandbox>,
    env: ChildEnv,
    shell: PathBuf,
}

impl std::fmt::Debug for Previewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Previewer")
            .field("shell", &self.shell)
            .finish_non_exhaustive()
    }
}

impl Previewer {
    /// Previews run by `shell -c` under `sandbox` (which must allow no
    /// write and no network) with `env`.
    #[must_use]
    pub fn new(sandbox: Arc<Sandbox>, env: ChildEnv, shell: PathBuf) -> Self {
        Self {
            sandbox,
            env,
            shell,
        }
    }

    /// Runs `line` in `cwd`; its output and exit status as text.
    pub async fn run(&self, line: &str, cwd: &Path) -> String {
        let mut command = match self.sandbox.command(&self.shell) {
            Ok(command) => command,
            Err(err) => return format!("(preview not run: {})", err.message()),
        };
        command
            .arg("-c")
            .arg(line)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        self.env.apply(&mut command);
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        match tokio::time::timeout(PREVIEW_TIMEOUT, command.output()).await {
            Ok(Ok(output)) => {
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                let text = clip(text.trim_end());
                format!(
                    "{text}\n(exit status {})",
                    output.status.code().unwrap_or(-1)
                )
            }
            Ok(Err(err)) => format!("(preview not run: {err})"),
            Err(_) => format!("(preview stopped after {} s)", PREVIEW_TIMEOUT.as_secs()),
        }
    }
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(PREVIEW_CHARS) {
        Some((at, _)) => format!("{}\n… (cut at {PREVIEW_CHARS} characters)", &text[..at]),
        None => text.to_owned(),
    }
}

/// The explanation of command `line` that `findings` flagged, run in
/// `cwd`; previews run only with a `previewer`.
pub async fn explain(
    line: &str,
    findings: &[Finding],
    cwd: &Path,
    previewer: Option<&Previewer>,
) -> String {
    let mut text = String::from("Explanation (nothing was run):\n");
    let script = match shell::parse(line) {
        Ok(script) => script,
        Err(err) => {
            let _ = writeln!(text, "the command could not be read: {err}");
            return text;
        }
    };
    describe(&mut text, &script.segments);
    let _ = writeln!(text, "why it asks:");
    for finding in findings {
        let _ = writeln!(text, "  {finding}");
    }
    paths(&mut text, &script.segments, cwd);
    for segment in &script.segments {
        if let Some(preview) = preview_of(segment) {
            let _ = writeln!(text, "preview `{preview}` (read-only, no network):");
            let output = match previewer {
                Some(previewer) => previewer.run(&preview, cwd).await,
                None => "(no preview sandbox in this session)".to_owned(),
            };
            for out in output.lines() {
                let _ = writeln!(text, "  {out}");
            }
        }
    }
    text
}

/// One line per simple command: program, arguments, redirections and
/// whether it reads a pipe.
fn describe(text: &mut String, segments: &[Segment]) {
    let _ = writeln!(text, "parsed command:");
    for segment in segments {
        let (program, args) = segment.program().unwrap_or(("(none)", &[]));
        let _ = write!(text, "  program `{program}`");
        if !args.is_empty() {
            let _ = write!(text, ", arguments {args:?}");
        }
        for redirect in &segment.redirects {
            let _ = write!(text, ", redirection `{} {}`", redirect.op, redirect.target);
        }
        if segment.piped {
            let _ = write!(text, ", reading the previous command's output (pipeline)");
        }
        text.push('\n');
    }
}

/// The absolute paths the operands and redirections name, globs expanded.
fn paths(text: &mut String, segments: &[Segment], cwd: &Path) {
    let mut lines = Vec::new();
    for segment in segments {
        let operands = segment
            .program()
            .map(|(_, args)| args.iter().filter(|a| !a.starts_with('-')).cloned())
            .into_iter()
            .flatten();
        let targets = segment.redirects.iter().map(|r| r.target.clone());
        for word in operands.chain(targets) {
            lines.extend(resolve(&word, cwd));
        }
    }
    if lines.is_empty() {
        return;
    }
    let _ = writeln!(text, "paths it would touch:");
    for line in lines {
        let _ = writeln!(text, "  {line}");
    }
}

/// `word` as absolute paths: a glob's matches, or the path itself when it
/// looks like one or exists.
fn resolve(word: &str, cwd: &Path) -> Vec<String> {
    let absolute = if Path::new(word).is_absolute() {
        PathBuf::from(word)
    } else {
        cwd.join(word)
    };
    if word.contains(['*', '?', '[']) {
        let pattern = absolute.to_string_lossy().into_owned();
        let matched: Vec<String> = glob::glob(&pattern)
            .into_iter()
            .flatten()
            .flatten()
            .take(LISTED)
            .map(|p| p.display().to_string())
            .collect();
        if matched.is_empty() {
            return vec![format!("{} (matches nothing)", absolute.display())];
        }
        return matched;
    }
    let looks_like_path = word.contains(['/', '\\']) || word.starts_with('.');
    if looks_like_path || absolute.exists() {
        vec![absolute.display().to_string()]
    } else {
        Vec::new()
    }
}

/// The preview of a command with a known dry-run form.
fn preview_of(segment: &Segment) -> Option<String> {
    let (program, args) = segment.program()?;
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let quoted = |words: &[String]| words.iter().map(|w| quote(w)).collect::<Vec<_>>().join(" ");
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    match (program, args.first().map(String::as_str)) {
        ("rm", _) => {
            let operands: Vec<String> = args
                .iter()
                .filter(|a| !a.starts_with('-'))
                .cloned()
                .collect();
            (!operands.is_empty()).then(|| format!("ls -1d -- {}", globbed(&operands)))
        }
        ("git", Some("clean")) => {
            let kept: Vec<String> = rest.iter().filter_map(|a| without_force(a)).collect();
            Some(
                format!("git clean -n {}", quoted(&kept))
                    .trim_end()
                    .to_owned(),
            )
        }
        ("git", Some("reset")) if has("--hard") => {
            Some("git status --short; git diff --stat".to_owned())
        }
        ("rsync", _) => Some(format!("rsync --dry-run {}", quoted(args))),
        ("terraform", Some("apply")) => Some(
            format!("terraform plan {}", quoted(&rest))
                .trim_end()
                .to_owned(),
        ),
        ("kubectl", Some("apply" | "delete")) => {
            Some(format!("kubectl {} --dry-run=server", quoted(args)))
        }
        _ => None,
    }
}

/// A `git clean` argument without its force flag (`-fdx` -> `-dx`).
fn without_force(arg: &str) -> Option<String> {
    if arg == "--force" {
        return None;
    }
    if arg.starts_with('-') && !arg.starts_with("--") {
        let flags: String = arg.chars().filter(|c| *c != 'f').collect();
        return (flags != "-").then_some(flags);
    }
    Some(arg.to_owned())
}

/// Operands quoted except their glob characters, so the preview's shell
/// expands the same globs the command would (`build/*` -> `'build/'*`).
fn globbed(words: &[String]) -> String {
    words
        .iter()
        .map(|word| {
            let mut out = String::new();
            let mut run = String::new();
            for c in word.chars() {
                if matches!(c, '*' | '?' | '[' | ']') {
                    if !run.is_empty() {
                        out.push_str(&quote(&run));
                        run.clear();
                    }
                    out.push(c);
                } else {
                    run.push(c);
                }
            }
            if !run.is_empty() || out.is_empty() {
                out.push_str(&quote(&run));
            }
            out
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `word` single-quoted for a POSIX shell.
fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding() -> Finding {
        Finding {
            detector: "dangerous-pattern".into(),
            reason: "recursive delete".into(),
            fragment: "rm -rf".into(),
        }
    }

    #[tokio::test]
    async fn a_recursive_delete_lists_every_matched_path_and_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let build = dir.path().join("build");
        std::fs::create_dir_all(&build).unwrap();
        for name in ["a.o", "b.o", "c.bin"] {
            std::fs::write(build.join(name), "x").unwrap();
        }
        let text = explain("rm -rf build/*", &[finding()], dir.path(), None).await;
        assert!(text.contains("program `rm`"), "{text}");
        assert!(text.contains("dangerous-pattern"), "{text}");
        for name in ["a.o", "b.o", "c.bin"] {
            assert!(
                text.contains(&build.join(name).display().to_string()),
                "{text}"
            );
            assert!(build.join(name).exists());
        }
        assert!(text.contains("preview `ls -1d -- 'build/'*`"), "{text}");
    }

    #[test]
    fn known_commands_have_their_dry_run_form() {
        let preview = |line: &str| {
            let script = shell::parse(line).unwrap();
            preview_of(&script.segments[0])
        };
        assert_eq!(
            preview("git clean -fdx").as_deref(),
            Some("git clean -n '-dx'")
        );
        assert_eq!(
            preview("git reset --hard HEAD~1").as_deref(),
            Some("git status --short; git diff --stat")
        );
        assert_eq!(
            preview("rsync -a src/ dst/").as_deref(),
            Some("rsync --dry-run '-a' 'src/' 'dst/'")
        );
        assert_eq!(
            preview("terraform apply").as_deref(),
            Some("terraform plan")
        );
        assert_eq!(
            preview("kubectl delete pod x").as_deref(),
            Some("kubectl 'delete' 'pod' 'x' --dry-run=server")
        );
        assert_eq!(preview("ls -la"), None);
    }

    #[test]
    fn pipelines_and_redirections_are_described() {
        let script = shell::parse("cat a.txt | grep x > out.txt").unwrap();
        let mut text = String::new();
        describe(&mut text, &script.segments);
        assert!(text.contains("program `grep`"), "{text}");
        assert!(text.contains("redirection `> out.txt`"), "{text}");
        assert!(text.contains("pipeline"), "{text}");
    }
}
