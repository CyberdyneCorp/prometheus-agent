//! `terminal-backend` plugins as terminal backends (spec:
//! execution-environments, Remote sandboxes are plugins; Built-in backend
//! names are reserved; Backend isolation descriptor).

mod common;

use common::{Fake, Profile};
use pmt_plugins::host::{CallInfo, Runtime};
use pmt_plugins::terminal::registry;
use pmt_sandbox::backend::{ExecRequest, Isolation};

const WORLD: &str = "\"terminal-backend@1.1\"";

fn request(line: &str) -> ExecRequest {
    ExecRequest {
        id: "c1".to_owned(),
        command: line.to_owned(),
        cwd: "/sandbox".to_owned(),
        env: vec![("LANG".to_owned(), "C.UTF-8".to_owned())],
        timeout_ms: 5_000,
    }
}

#[test]
fn the_sample_backend_registers_and_answers_every_call() {
    let mut profile = Profile::new("p");
    profile.install("sample-backend", "sample-terminal-backend", WORLD, "");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &Fake::new().ports());
    let (registry, rejected) = registry(&loaded, &CallInfo::default());
    assert!(rejected.is_empty(), "{rejected:?}");
    let backend = registry.select("sample").unwrap();
    let plugin = backend.plugin.clone().unwrap();
    assert_eq!(plugin.descriptor().isolation, Isolation::Remote);
    assert_eq!(plugin.descriptor().default_cwd, "/sandbox");
    assert_eq!(plugin.check().unwrap(), "in-memory sample");
    let output = plugin.execute(&request("cd /sandbox/src")).unwrap();
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout, "cd /sandbox/src\n");
    assert_eq!(output.cwd_observed.as_deref(), Some("/sandbox/src"));
    plugin.upload("/sandbox/a.txt", b"hello").unwrap();
    assert_eq!(plugin.download("/sandbox/a.txt").unwrap(), b"hello");
    plugin.cancel("c1");
    plugin.teardown();
}

#[test]
fn plugin_omits_exit_code_reaches_the_host_as_missing() {
    let mut profile = Profile::new("p");
    profile.install("testing-backend", "testing-backend", WORLD, "");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &Fake::new().ports());
    let (registry, _) = registry(&loaded, &CallInfo::default());
    let plugin = registry.select("testbox").unwrap().plugin.clone().unwrap();
    let output = plugin.execute(&request("echo omit-exit-code")).unwrap();
    assert_eq!(output.exit_code, None);
    assert_eq!(plugin.execute(&request("true")).unwrap().exit_code, Some(3));
}

#[test]
fn relative_default_cwd() {
    let mut profile = Profile::new("p");
    profile.install("testing-backend", "testing-backend", WORLD, "");
    let fake = Fake::new();
    fake.set("testing-backend", "default_cwd", "\"work\"");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &fake.ports());
    let (registry, rejected) = registry(&loaded, &CallInfo::default());
    assert_eq!(
        rejected[0].1.message(),
        "terminal backend 'testbox' rejected: default_cwd must be absolute or '~'"
    );
    assert!(registry.select("testbox").is_err());
}

#[test]
fn a_plugin_backend_named_like_a_builtin_is_rejected() {
    let mut profile = Profile::new("p");
    profile.install("testing-backend", "testing-backend", WORLD, "");
    let fake = Fake::new();
    fake.set("testing-backend", "name", "\"Docker\"");
    let runtime = Runtime::new().unwrap();
    let loaded = profile.load(&runtime, &fake.ports());
    let (registry, rejected) = registry(&loaded, &CallInfo::default());
    assert_eq!(
        rejected[0].1.message(),
        "terminal backend name 'Docker' is reserved"
    );
    assert_eq!(
        registry.select("container").unwrap().kind,
        pmt_sandbox::backend::Kind::Container
    );
}
