//! Browser automation against a real headless Chromium-family browser and
//! a local test page (spec: execution-environments, Browser session
//! resolution; Browser URL safety; Browser supervision, dialogs and
//! cleanup; `browser_exec` tool; distribution-and-update, Lazy runtime
//! installation policy). The browser comes from this host (Chrome,
//! Chromium, Brave, Edge, or Playwright's download).
//!
//! Unix only: on Windows the interpreter runs under the AppContainer
//! sandbox, whose Python setup is not covered here yet.
#![cfg(unix)]

mod cleanup;
mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use pmt_config::schema::DialogPolicy;
use pmt_tools::browser::launch::{InstallFuture, Runtimes};
use pmt_tools::browser::{CloudProvider, NoRuntimes, Origin, RuntimePolicy};
use pmt_tools::result::{Content, Status};
use support::{Fixture, KNOWN_SECRET, records, run, run_in, site};

fn kill(pid: u32) {
    std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
}

#[tokio::test]
async fn a_batched_sub_procedure_keeps_its_tab_for_the_next_call() {
    let fixture = Fixture::new();
    let site = site().await;
    let manager = fixture.manager();
    let exec = fixture.exec(&manager, false);
    let (result, body) = run(
        &exec,
        &format!(
            "new_tab('{}/'); wait_for_load(); print(page_info())\nkept = 1\nopen('notes.txt', 'w').write('saved')",
            site.uri()
        ),
    )
    .await;
    assert_eq!(result.status, Status::Ok, "{body}");
    let output = body["output"].as_str().unwrap();
    assert!(output.contains("Test page"), "{output}");
    assert!(output.contains("Hello from the test page"), "{output}");
    let workspace = body["workspace"].as_str().unwrap().to_owned();

    let (result, body) = run(
        &exec,
        "print(page_info()['title'])\nprint(open('notes.txt').read())\nprint('kept' in dir())",
    )
    .await;
    assert_eq!(result.status, Status::Ok, "{body}");
    assert_eq!(body["output"], "Test page\nsaved\nFalse\n");
    assert_eq!(body["workspace"], workspace.as_str());
    manager.shutdown().await;
}

#[tokio::test]
async fn helpers_fill_click_evaluate_and_screenshot() {
    let fixture = Fixture::new();
    let site = site().await;
    let manager = fixture.manager();
    let exec = fixture.exec(&manager, true);
    let (result, body) = run(
        &exec,
        &format!(
            "new_tab('{}/'); wait_for_load()\nfill_input('#q', 'typed')\nprint(js('document.querySelector(\"#q\").value'))\n\
             click_at_xy(50, 115)\nprint(js('document.title'))\nprint(capture_screenshot('shot.png'))\n\
             print(cdp('Runtime.evaluate', {{'expression': '6*7', 'returnByValue': True}})['result']['value'])",
            site.uri()
        ),
    )
    .await;
    assert_eq!(result.status, Status::Ok, "{body}");
    let lines: Vec<&str> = body["output"].as_str().unwrap().lines().collect();
    assert_eq!(lines[0], "typed");
    assert_eq!(lines[1], "clicked");
    assert!(lines[2].ends_with("shot.png"), "{lines:?}");
    assert_eq!(lines[3], "42");
    // The model accepts images: the screenshot is attached.
    assert!(
        result
            .content
            .iter()
            .any(|c| matches!(c, Content::Media { mime, .. } if mime == "image/png"))
    );
    let shot = std::fs::read(lines[2]).unwrap();
    assert!(shot.starts_with(b"\x89PNG"));
    // Without image input the path is all that comes back.
    let text_only = fixture.exec(&manager, false);
    let (result, _) = run(&text_only, "capture_screenshot()").await;
    assert!(
        result
            .content
            .iter()
            .all(|c| matches!(c, Content::Text { .. }))
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn refused_navigations_and_closing_the_browser() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let exec = fixture.exec(&manager, false);
    let code = format!(
        "for url in ['http://169.254.169.254/latest/meta-data', 'https://example.com/?api_key=abcdef0123456789', 'https://example.com/?q={KNOWN_SECRET}']:\n\
         \x20   try:\n        goto_url(url)\n    except BrowserError as err:\n        print(err)\n\
         try:\n    cdp('Browser.close')\nexcept BrowserError as err:\n    print(err)"
    );
    let (result, body) = run(&exec, &code).await;
    assert_eq!(result.status, Status::Ok, "{body}");
    assert_eq!(
        body["output"],
        "Blocked: URL targets a cloud metadata endpoint\n\
         Blocked: URL contains what appears to be an API key or token.\n\
         Blocked: URL contains what appears to be an API key or token.\n\
         Browser.close is not allowed\n"
    );
    // The browser is still there.
    let (result, _) = run(&exec, "ensure_real_tab()").await;
    assert_eq!(result.status, Status::Ok);
    manager.shutdown().await;
}

#[tokio::test]
async fn a_crashed_browser_is_replaced_and_its_record_removed() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let exec = fixture.exec(&manager, false);
    run(&exec, "ensure_real_tab()").await;
    let first = manager.session("ses_b/default").await.unwrap();
    let pid = first.pid().unwrap();
    let old_records = records(&fixture.browser_cache());
    assert_eq!(old_records.len(), 1);
    drop(first);
    kill(pid);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (result, body) = run(&exec, "print(ensure_real_tab() is not None)").await;
    assert_eq!(result.status, Status::Ok, "{body}");
    let second = manager.session("ses_b/default").await.unwrap();
    assert_ne!(second.pid().unwrap(), pid);
    let now = records(&fixture.browser_cache());
    assert_eq!(now.len(), 1);
    assert!(!now.contains(&old_records[0]), "the dead record is gone");
    drop(second);
    manager.shutdown().await;
}

#[tokio::test]
async fn named_sessions_get_their_own_browser_and_idle_ones_are_closed() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let exec = fixture.exec(&manager, false);
    run(&exec, "ensure_real_tab()").await;
    run_in(&exec, Some("other"), "ensure_real_tab()").await;
    let default = manager.session("ses_b/default").await.unwrap().pid();
    let other = manager.session("ses_b/other").await.unwrap().pid();
    assert_ne!(default, other);
    assert_eq!(records(&fixture.browser_cache()).len(), 2);
    // `browser.inactivity_timeout_secs` is read at use time.
    fixture.config.lock().unwrap().inactivity_timeout_secs = 0;
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    // The background reaper may have closed them already.
    manager.reap_idle().await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !records(&fixture.browser_cache()).is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(records(&fixture.browser_cache()).is_empty());
    let fresh = manager.session("ses_b/default").await.unwrap().pid();
    assert_ne!(fresh, default, "a closed session starts a new browser");
    manager.shutdown().await;
}

#[tokio::test]
async fn dialogs_follow_the_dialog_policy() {
    for (policy, expected) in [
        (DialogPolicy::AutoDismiss, "False"),
        (DialogPolicy::AutoAccept, "True"),
        (DialogPolicy::MustRespond, "False"),
    ] {
        let fixture = Fixture::new();
        {
            let mut config = fixture.config.lock().unwrap();
            config.dialog_policy = policy;
            config.dialog_timeout_secs = 1;
        }
        let manager = fixture.manager();
        let exec = fixture.exec(&manager, false);
        let (result, body) = run(
            &exec,
            "ensure_real_tab()\nprint(js('confirm(\"Proceed?\")'))",
        )
        .await;
        assert_eq!(result.status, Status::Ok, "{policy:?}: {body}");
        assert_eq!(body["output"], format!("{expected}\n"), "{policy:?}");
        let handled = body["dialogs"].to_string();
        assert!(handled.contains("Proceed?"), "{policy:?}: {handled}");
        if policy == DialogPolicy::MustRespond {
            assert!(handled.contains("after 1 s"), "{handled}");
        }
        manager.shutdown().await;
    }
}

struct FailingCloud;

impl CloudProvider for FailingCloud {
    fn create(&self) -> pmt_tools::browser::session::CreateFuture<'_> {
        Box::pin(async { Err("quota exhausted".to_owned()) })
    }
}

#[tokio::test]
async fn a_failing_cloud_provider_falls_back_to_a_local_browser() {
    let fixture = Fixture::new();
    fixture.config.lock().unwrap().provider = "browserbase".into();
    let providers: BTreeMap<String, Arc<dyn CloudProvider>> =
        BTreeMap::from([("browserbase".to_owned(), Arc::new(FailingCloud) as _)]);
    let manager = fixture.manager_with(Fixture::chromium(), providers.clone());
    let exec = fixture.exec(&manager, false);
    let (result, body) = run(&exec, "ensure_real_tab()").await;
    assert_eq!(result.status, Status::Ok, "{body}");
    assert_eq!(body["fallback_from_cloud"], true);
    let session = manager.session("ses_b/default").await.unwrap();
    assert_eq!(session.origin(), &Origin::Local);
    drop(session);
    manager.shutdown().await;

    // Both failing names both reasons.
    let nothing = RuntimePolicy {
        runtimes: Arc::new(NoRuntimes),
        allow_lazy_install: false,
        host_candidates: Vec::new(),
    };
    let manager = fixture.manager_with(nothing, providers);
    let err = manager.session("ses_b/default").await.err().unwrap();
    assert_eq!(
        err,
        "Cloud provider browserbase failed (quota exhausted) and local fallback also failed \
         (chromium is not installed and lazy installs are disabled: run 'prometheus runtimes install chromium')"
    );
}

struct FakeStore {
    executable: std::path::PathBuf,
    installs: std::sync::Mutex<Vec<String>>,
}

impl Runtimes for FakeStore {
    fn installed(&self, _: &str) -> Option<std::path::PathBuf> {
        None
    }

    fn install<'a>(&'a self, name: &'a str) -> InstallFuture<'a> {
        self.installs.lock().unwrap().push(name.to_owned());
        Box::pin(async move { Ok(self.executable.clone()) })
    }
}

#[tokio::test]
async fn a_missing_chromium_is_installed_on_first_use_only_when_allowed() {
    let fixture = Fixture::new();
    let store = Arc::new(FakeStore {
        executable: support::browsers()[0].clone(),
        installs: std::sync::Mutex::default(),
    });
    let refused = fixture.manager_with(
        RuntimePolicy {
            runtimes: store.clone(),
            allow_lazy_install: false,
            host_candidates: Vec::new(),
        },
        BTreeMap::new(),
    );
    let err = refused.session("ses_b/default").await.err().unwrap();
    assert_eq!(
        err,
        "chromium is not installed and lazy installs are disabled: run 'prometheus runtimes install chromium'"
    );
    assert!(
        store.installs.lock().unwrap().is_empty(),
        "nothing downloaded"
    );
    let lazy = fixture.manager_with(
        RuntimePolicy {
            runtimes: store.clone(),
            allow_lazy_install: true,
            host_candidates: Vec::new(),
        },
        BTreeMap::new(),
    );
    assert!(lazy.session("ses_b/default").await.is_ok());
    assert_eq!(*store.installs.lock().unwrap(), ["chromium"]);
    lazy.shutdown().await;
}

#[tokio::test]
async fn an_explicit_endpoint_is_used_and_rebinding_replaces_the_session() {
    let fixture = Fixture::new();
    let env = fixture.env();
    let executable = support::browsers()[0].clone();
    let profiles = fixture.home.join("external");
    let a =
        pmt_tools::browser::launch::launch(&executable, &profiles, &env, Duration::from_secs(30))
            .await
            .unwrap();
    let b =
        pmt_tools::browser::launch::launch(&executable, &profiles, &env, Duration::from_secs(30))
            .await
            .unwrap();
    fixture.config.lock().unwrap().cdp_url = Some(a.endpoint.clone());
    let manager = fixture.manager();
    let first = manager.session("ses_b/default").await.unwrap();
    assert_eq!(first.origin(), &Origin::Connected(a.endpoint.clone()));
    assert_eq!(first.pid(), None, "an explicit endpoint launches nothing");
    manager.set_connect(Some(b.endpoint.clone()));
    let second = manager.session("ses_b/default").await.unwrap();
    assert_eq!(second.origin(), &Origin::Connected(b.endpoint.clone()));
    // The supervisor gives up on a browser that is gone for good.
    b.terminate().await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!second.is_alive());
    manager.shutdown().await;
    a.terminate().await;
}

#[tokio::test]
async fn a_crashed_owners_browser_is_reaped_by_the_next_process() {
    let fixture = Fixture::new();
    let executable = support::browsers()[0].clone();
    let cache = fixture.browser_cache();
    let mut launched = pmt_tools::browser::launch::launch(
        &executable,
        &cache.join("profiles"),
        &fixture.env(),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    // A record written by an owner that has since exited.
    let gone = std::process::Command::new("true").spawn().unwrap();
    let owner = gone.id();
    let _ = gone.wait_with_output();
    let record = pmt_tools::browser::records::Record {
        owner_pid: owner,
        owner_start: 1,
        browser_pid: launched.pid,
        browser_start: pmt_core::runtime::process_start_time(launched.pid).unwrap(),
        profile_dir: launched.profile_dir.clone(),
        temp_dir: Some(launched.temp_dir.clone()),
    };
    std::fs::create_dir_all(cache.join("records")).unwrap();
    std::fs::write(
        cache.join("records/orphan.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    let _manager = fixture.manager();
    let exited = tokio::time::timeout(Duration::from_secs(10), launched.child.wait()).await;
    assert!(exited.is_ok(), "the orphaned browser was killed");
    assert!(records(&cache).is_empty());
    assert!(!launched.profile_dir.exists());
    assert!(!launched.temp_dir.exists());
}

#[tokio::test]
async fn a_failed_start_reports_what_the_browser_said() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new();
    let fake = fixture.home.join("fake-chrome");
    std::fs::write(
        &fake,
        "#!/bin/sh\necho 'FATAL: No usable sandbox!' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = pmt_tools::browser::launch::launch(
        &fake,
        &fixture.home.join("profiles"),
        &fixture.env(),
        Duration::from_secs(10),
    )
    .await
    .err()
    .unwrap();
    assert!(err.starts_with("the browser exited during start"), "{err}");
    assert!(err.contains("No usable sandbox!"), "{err}");
}
