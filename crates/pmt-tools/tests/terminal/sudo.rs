//! Sudo password handling (spec: execution-environments, Sudo password
//! handling) against a stand-in `sudo` on `PATH` that behaves like the real
//! one: `-S` reads the password from stdin, `-v` only checks it, a good
//! password is remembered (sudo's credential cache), and without one it
//! fails as sudo does without a terminal.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pmt_config::schema::TerminalConfig;
use pmt_core::secret::SecretString;
use pmt_tools::terminal::sudo::{SecretLookup, Sudo, SudoPrompt, Surface};
use pmt_tools::terminal::{Status, Terminal};

use crate::common::{Fixture, run};

const PASSWORD: &str = "correct-horse-sudo";

const FAKE_SUDO: &str = r#"#!/bin/sh
typed=""; check=0
while [ $# -gt 0 ]; do
  case "$1" in
    -S) IFS= read -r typed || true; shift ;;
    -p) shift 2 ;;
    -v) check=1; shift ;;
    -*) shift ;;
    *) break ;;
  esac
done
if [ -f "$FAKE_SUDO_DIR/cached" ]; then :
elif [ -n "$typed" ] && [ "$typed" = "correct-horse-sudo" ]; then touch "$FAKE_SUDO_DIR/cached"
elif [ -n "$typed" ]; then echo "Sorry, try again." >&2; echo "sudo: 1 incorrect password attempt" >&2; exit 1
else echo "sudo: a terminal is required to read the password; either use the -S option" >&2; exit 1
fi
[ "$check" = 1 ] && exit 0
exec "$@"
"#;

/// A prompt that answers `answer` and counts the questions.
struct Counting {
    answer: &'static str,
    asked: AtomicUsize,
}

impl SudoPrompt for Counting {
    fn ask(&self, _command: &str) -> Option<SecretString> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Some(SecretString::new(self.answer))
    }
}

struct Host {
    fixture: Fixture,
    cache: PathBuf,
}

impl Host {
    fn new() -> Self {
        let fixture = Fixture::new();
        let bin = fixture.work.join("bin");
        let cache = fixture.work.join("sudo-cache");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        let sudo = bin.join("sudo");
        std::fs::write(&sudo, FAKE_SUDO).unwrap();
        std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { fixture, cache }
    }

    fn session(&self, sudo: Sudo) -> Terminal {
        let fixture = &self.fixture;
        let cache = self.cache.to_string_lossy().into_owned();
        let mut env = fixture.child_env(
            &["FAKE_SUDO_DIR".to_owned()],
            &[("FAKE_SUDO_DIR", cache.as_str())],
        );
        env.prepend_path(&[fixture.work.join("bin")]);
        let setup = fixture.setup(TerminalConfig::default(), env, fixture.sandbox(&[], None));
        let terminal = Terminal::new(setup).unwrap();
        terminal.set_sudo(Arc::new(sudo));
        terminal
    }

    /// Forgets sudo's own credential cache (as its timeout would).
    fn expire(&self) {
        let _ = std::fs::remove_file(self.cache.join("cached"));
    }
}

#[tokio::test]
async fn interactive_sudo() {
    // GIVEN an interactive CLI session WHEN the model runs `sudo apt-get
    // update` twice THEN the user is prompted once and the second command
    // reuses the cached password.
    let host = Host::new();
    let prompt = Arc::new(Counting {
        answer: PASSWORD,
        asked: AtomicUsize::new(0),
    });
    let session = host.session(Sudo::new(Surface::Interactive(prompt.clone()), None));
    let command = "sudo sh -c 'echo updated; echo \"leak=[$PMT_SUDO_PASSWORD]\"; cat'";
    let first = run(&session, command).await;
    assert_eq!(first.status, Status::Ok, "{first:?}");
    assert!(first.output.contains("updated"), "{}", first.output);
    // The command itself sees neither the variable nor the password line.
    assert!(first.output.contains("leak=[]"), "{}", first.output);
    assert!(!first.output.contains(PASSWORD), "{}", first.output);
    host.expire();
    let second = run(&session, command).await;
    assert_eq!(second.status, Status::Ok, "{second:?}");
    assert_eq!(prompt.asked.load(Ordering::SeqCst), 1);
    // Commands that are not literally sudo are left alone.
    let other = run(&session, "bash -c 'echo sudo'").await;
    assert_eq!(other.output.trim(), "sudo");
    assert_eq!(prompt.asked.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn subagent_sudo() {
    // GIVEN a subagent without `SUDO_PASSWORD` WHEN it runs `sudo id` THEN
    // no prompt is shown and the result explains that subagents cannot
    // prompt for sudo.
    let host = Host::new();
    let session = host.session(Sudo::new(Surface::Subagent, None));
    let result = run(&session, "sudo id").await;
    assert_eq!(result.status, Status::Error, "{result:?}");
    let hint = result.hint.unwrap_or_default();
    assert!(hint.contains("subagents cannot prompt for sudo"), "{hint}");
}

#[tokio::test]
async fn the_granted_secret_is_used_without_a_prompt() {
    let host = Host::new();
    let secret: SecretLookup = Arc::new(|| Some(SecretString::new(PASSWORD)));
    let session = host.session(Sudo::new(Surface::Messaging, Some(secret)));
    let result = run(
        &session,
        "env FOO=1 /usr/bin/env true && sudo echo elevated",
    )
    .await;
    // The first simple command is `env ... /usr/bin/env`, not sudo: left
    // alone, and the second sudo has no password from the shell.
    assert_eq!(result.status, Status::Error, "{result:?}");
    let result = run(&session, "env FOO=1 sudo sh -c 'echo $FOO elevated'").await;
    assert_eq!(result.status, Status::Ok, "{result:?}");
    assert_eq!(result.output.trim(), "1 elevated");
}

#[tokio::test]
async fn a_messaging_session_without_the_secret_is_told_how_to_store_it() {
    let host = Host::new();
    let session = host.session(Sudo::new(Surface::Messaging, None));
    let result = run(&session, "sudo id").await;
    assert_eq!(result.status, Status::Error, "{result:?}");
    let hint = result.hint.unwrap_or_default();
    assert!(
        hint.contains("prometheus secrets set SUDO_PASSWORD"),
        "{hint}"
    );
}

#[tokio::test]
async fn a_rejected_password_is_forgotten_and_never_shown() {
    let host = Host::new();
    let prompt = Arc::new(Counting {
        answer: "wrong-password-typed",
        asked: AtomicUsize::new(0),
    });
    let session = host.session(Sudo::new(Surface::Interactive(prompt.clone()), None));
    let first = run(&session, "sudo echo hi").await;
    assert_eq!(first.status, Status::Error, "{first:?}");
    assert!(
        first
            .hint
            .unwrap_or_default()
            .contains("rejected the password"),
        "{:?}",
        first.output
    );
    assert!(!first.output.contains("wrong-password-typed"));
    let _ = run(&session, "sudo echo hi").await;
    assert_eq!(prompt.asked.load(Ordering::SeqCst), 2, "asked again");
}
