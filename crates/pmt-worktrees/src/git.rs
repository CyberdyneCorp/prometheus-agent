//! Hardened git invocations (spec: git-worktrees, Worktree creation;
//! Archive format).
//!
//! Every command Prometheus runs on a repository it did not author is
//! pinned so the repository cannot run code through it: hooks point at an
//! empty directory, `core.fsmonitor` is off, every clean/smudge/process
//! filter the configuration names is blanked (and LFS smudging skipped),
//! prompts are off and optional index locks are not taken, so read-only
//! commands never rewrite the index under a person working in the tree.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pmt_core::error::{Error, codes};

/// How long one git command may run before it is killed.
const DEFAULT_TIMEOUT: Duration = Duration::from_mins(5);

/// A git runner bound to one working directory.
#[derive(Debug, Clone)]
pub struct Git {
    dir: PathBuf,
    config: Vec<(String, String)>,
    env: Vec<(String, OsString)>,
    timeout: Duration,
}

/// What a finished git command printed.
#[derive(Debug, Clone)]
pub struct Output {
    /// Whether it exited 0.
    pub success: bool,
    /// Its standard output.
    pub stdout: Vec<u8>,
    /// Its standard error, trimmed.
    pub stderr: String,
}

impl Output {
    /// Standard output as text, trailing newline removed.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout)
            .trim_end_matches(['\n', '\r'])
            .to_owned()
    }
}

impl Git {
    /// A runner in `dir` with the repository's hooks, fsmonitor and filters
    /// disabled. `hooks` is the empty directory `core.hooksPath` points at.
    #[must_use]
    pub fn at(dir: &Path, hooks: &Path) -> Self {
        let mut git = Self {
            dir: dir.to_path_buf(),
            config: Vec::new(),
            env: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
        };
        for (key, value) in [
            ("core.hooksPath", hooks.to_string_lossy().as_ref()),
            ("core.fsmonitor", "false"),
            ("core.untrackedCache", "false"),
            ("commit.gpgSign", "false"),
            ("tag.gpgSign", "false"),
            ("advice.detachedHead", "false"),
            ("protocol.file.allow", "user"),
        ] {
            git.config.push((key.to_owned(), value.to_owned()));
        }
        for (key, value) in [
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_LFS_SKIP_SMUDGE", "1"),
            ("GCM_INTERACTIVE", "never"),
            ("LC_ALL", "C"),
        ] {
            git.env.push((key.to_owned(), value.into()));
        }
        git.neutralize_filters();
        git
    }

    /// The same runner in another directory.
    #[must_use]
    pub fn in_dir(&self, dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            ..self.clone()
        }
    }

    /// The working directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// This runner with `GIT_INDEX_FILE` set (a temporary index).
    #[must_use]
    pub fn with_index(&self, index: &Path) -> Self {
        self.with_env("GIT_INDEX_FILE", index.as_os_str().to_owned())
    }

    /// This runner with an extra environment variable.
    #[must_use]
    pub fn with_env(&self, key: &str, value: OsString) -> Self {
        let mut git = self.clone();
        git.env.retain(|(k, _)| k != key);
        git.env.push((key.to_owned(), value));
        git
    }

    /// This runner with a shorter or longer time limit.
    #[must_use]
    pub fn with_timeout(&self, timeout: Duration) -> Self {
        Self {
            timeout,
            ..self.clone()
        }
    }

    /// Blanks every filter driver the configuration names: a checkout or
    /// a status of a hostile repository then runs none of them. Filters
    /// named only by `.gitattributes` without a configured command never
    /// run anyway.
    fn neutralize_filters(&mut self) {
        let Ok(found) = self.output(&[
            "config",
            "--includes",
            "--name-only",
            "-z",
            "--get-regexp",
            r"^filter\..*\.(clean|smudge|process|required)$",
        ]) else {
            return;
        };
        let text = String::from_utf8_lossy(&found.stdout).into_owned();
        let mut drivers: Vec<&str> = text
            .split('\0')
            .filter_map(|key| key.strip_prefix("filter."))
            .filter_map(|rest| rest.rsplit_once('.').map(|(name, _)| name))
            .collect();
        drivers.sort_unstable();
        drivers.dedup();
        for name in drivers {
            for part in ["clean", "smudge", "process"] {
                self.config
                    .push((format!("filter.{name}.{part}"), String::new()));
            }
            self.config
                .push((format!("filter.{name}.required"), "false".to_owned()));
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command.current_dir(&self.dir);
        for (key, value) in &self.config {
            command.arg("-c").arg(format!("{key}={value}"));
        }
        command.args(args);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command
    }

    /// Runs git with `args`, whatever its exit status.
    ///
    /// # Errors
    ///
    /// `worktree.git` when git cannot be started or exceeds its time limit.
    pub fn output(&self, args: &[&str]) -> Result<Output, Error> {
        self.output_with_input(args, None)
    }

    /// Runs git with `args`, feeding `input` on its standard input.
    ///
    /// # Errors
    ///
    /// As [`Git::output`].
    pub fn output_with_input(&self, args: &[&str], input: Option<&[u8]>) -> Result<Output, Error> {
        let mut command = self.command(args);
        command
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| git_error(args, &e.to_string()))?;
        let writer = input.map(|bytes| {
            let mut stdin = child.stdin.take();
            let bytes = bytes.to_vec();
            std::thread::spawn(move || {
                if let Some(stdin) = stdin.as_mut() {
                    let _ = stdin.write_all(&bytes);
                }
            })
        });
        let readers = (
            pipe_reader(child.stdout.take()),
            pipe_reader(child.stderr.take()),
        );
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|e| git_error(args, &e.to_string()))?
            {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(git_error(args, "timed out"));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        if let Some(writer) = writer {
            let _ = writer.join();
        }
        let stdout = readers.0.join().unwrap_or_default();
        let stderr = readers.1.join().unwrap_or_default();
        Ok(Output {
            success: status.success(),
            stdout,
            stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
        })
    }

    /// Runs git with `args` and returns its output text.
    ///
    /// # Errors
    ///
    /// `worktree.git` with git's message when it fails.
    pub fn run(&self, args: &[&str]) -> Result<String, Error> {
        self.checked(args, None).map(|out| out.text())
    }

    /// Runs git with `args` and `input`, returning its raw output.
    ///
    /// # Errors
    ///
    /// As [`Git::run`].
    pub fn run_bytes(&self, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, Error> {
        self.checked(args, input).map(|out| out.stdout)
    }

    /// Whether git with `args` exits 0 (a question, not an action).
    #[must_use]
    pub fn test(&self, args: &[&str]) -> bool {
        self.output(args).is_ok_and(|out| out.success)
    }

    fn checked(&self, args: &[&str], input: Option<&[u8]>) -> Result<Output, Error> {
        let out = self.output_with_input(args, input)?;
        if out.success {
            Ok(out)
        } else {
            Err(git_error(args, &out.stderr))
        }
    }
}

fn pipe_reader<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

/// A `worktree.git` error naming the git subcommand.
#[must_use]
pub fn git_error(args: &[&str], why: &str) -> Error {
    let sub = args.first().copied().unwrap_or("git");
    Error::new(&codes::WORKTREE_GIT, format!("git {sub} failed: {why}"))
}

/// Splits NUL-separated git output into its entries.
#[must_use]
pub fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
        .collect()
}
