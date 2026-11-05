//! Local browser isolation on Windows (spec: execution-environments, Local
//! browser isolation): a real Chromium-family browser of the host (Chrome
//! or Edge on the CI runners) starts inside an `AppContainer`, with the
//! built binary as the sandbox launcher, answers DevTools over the
//! inherited pipe, and reaches a local site only through the session's
//! egress proxy (via the container's relay). What the container itself
//! refuses (writes outside its directories, direct connections) is
//! covered by pmt-sandbox's Windows tests.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pmt_config::schema::SandboxConfig;
use pmt_core::env::{EnvPolicy, LaunchEnv};
use pmt_net::DestinationPolicy;
use pmt_net::egress::{HostPolicy, Mode, Proxy, Rules};
use pmt_tools::browser::launch::{Confinement, launch, system_candidates};
use pmt_tools::terminal::env::{ChildEnv, EnvRequest};
use serde_json::{Value, json};
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Prints `<pid> <1|0>` for every process whose command line names
/// `$profile` (the browser and its helpers): whether its token is an
/// `AppContainer` token (`TokenIsAppContainer`, class 29).
const CONTAINED: &str = r#"
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class Token {
  [DllImport("kernel32.dll")] static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
  [DllImport("advapi32.dll")] static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
  [DllImport("advapi32.dll")] static extern bool GetTokenInformation(IntPtr token, int cls, out int value, int size, out int returned);
  public static int IsAppContainer(int pid) {
    IntPtr process = OpenProcess(0x1000, false, pid);
    if (process == IntPtr.Zero) return -1;
    IntPtr token;
    if (!OpenProcessToken(process, 8, out token)) return -2;
    int value, returned;
    if (!GetTokenInformation(token, 29, out value, 4, out returned)) return -3;
    return value;
  }
}
"@
Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -and $_.CommandLine.Contains($env:PMT_PROFILE_DIR) -and $_.Name -ne 'prometheus.exe' } | ForEach-Object { "$($_.ProcessId) $([Token]::IsAppContainer([int]$_.ProcessId))" }
"#;

fn browser() -> PathBuf {
    system_candidates(std::env::var_os("PATH").as_deref())
        .into_iter()
        .find(|path| path.is_file())
        .expect("no Chromium-family browser on this host (Chrome or Edge)")
}

fn child_env(home: &Path) -> ChildEnv {
    let mut vars: Vec<_> = std::env::vars_os().collect();
    vars.push(("USERPROFILE".into(), home.as_os_str().to_owned()));
    let launch = LaunchEnv::from_vars(
        vars,
        &EnvPolicy {
            provider_secret_names: &[],
            replacements: &[],
        },
    );
    ChildEnv::build(&launch, EnvRequest::default())
}

async fn site() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<!doctype html><title>Contained page</title><p>hello</p>",
            "text/html; charset=utf-8",
        ))
        .mount(&server)
        .await;
    server
}

/// The title of the first page target, once it is `want` (or the last
/// seen titles after 30 s).
async fn wait_for_title(cdp: &pmt_tools::browser::cdp::Cdp, want: &str) -> Result<(), String> {
    let mut seen = Value::Null;
    for _ in 0..300 {
        seen = cdp
            .call("Target.getTargets", json!({}), None)
            .await
            .unwrap_or(Value::Null);
        let found = seen["targetInfos"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|target| target["title"] == want);
        if found {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(seen.to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_browser_runs_in_an_app_container_behind_the_proxy() {
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let base = root.path().to_path_buf();
    let home = base.join("home");
    let profiles = home.join(".prometheus/profiles/default/cache/browser/profiles");
    std::fs::create_dir_all(&profiles).unwrap();
    let rules = Rules::new(
        Arc::new(HostPolicy::new(Mode::Public, Vec::new())),
        DestinationPolicy::new(true, &[]).unwrap(),
        "browser",
    );
    let proxy = Proxy::start(rules).unwrap();
    let confinement = Confinement {
        user_home: home.clone(),
        prometheus_home: home.join(".prometheus"),
        sandbox: SandboxConfig::default(),
        launcher: Some(PathBuf::from(env!("CARGO_BIN_EXE_prometheus"))),
    };
    let launched = launch(
        &browser(),
        &profiles,
        &child_env(&home),
        Duration::from_secs(60),
        &proxy,
        &confinement,
    )
    .await
    .unwrap();

    // Every process of this browser runs in an AppContainer.
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", CONTAINED])
        .env("PMT_PROFILE_DIR", &launched.profile_dir)
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&output.stdout).into_owned();
    let rows: Vec<&str> = listing.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(!rows.is_empty(), "no browser process found: {output:?}");
    assert!(
        rows.iter().all(|row| row.ends_with(" 1")),
        "a browser process outside the AppContainer: {listing}"
    );

    // DevTools answers over the pipe and the page loads through the proxy.
    let site = site().await;
    launched
        .cdp
        .call(
            "Target.createTarget",
            json!({"url": format!("{}/", site.uri())}),
            None,
        )
        .await
        .unwrap();
    wait_for_title(&launched.cdp, "Contained page")
        .await
        .unwrap_or_else(|targets| panic!("the page did not load: {targets}"));
    assert!(
        !site
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "the site was not reached"
    );
    assert!(
        std::fs::read_dir(&launched.profile_dir).unwrap().count() > 0,
        "the browser wrote nothing to its profile"
    );
    let profile_dir = launched.profile_dir.clone();
    launched.terminate().await;
    assert!(!profile_dir.exists(), "the throwaway profile is removed");
}
