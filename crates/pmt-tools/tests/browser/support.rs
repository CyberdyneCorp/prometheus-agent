//! A temporary profile, a local test site and browser managers over a
//! real headless Chromium-family browser of this host.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pmt_config::schema::{BrowserConfig, SandboxConfig};
use pmt_core::env::{EnvPolicy, LaunchEnv};
use pmt_core::secret::{Redactor, SecretString};
use pmt_net::DestinationPolicy;
use pmt_net::resolve::SystemLookup;
use pmt_sandbox::{Options, Paths, Policy, Sandbox};
use pmt_tools::browser::{
    BrowserExec, BrowserManager, BrowserSetup, CloudProvider, ExecArgs, ExecSetup, NoRuntimes,
    RuntimePolicy, UrlSafety,
};
use pmt_tools::result::ToolResult;
use pmt_tools::terminal::env::{ChildEnv, EnvRequest};
use serde_json::Value;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A secret value the session knows.
pub(crate) const KNOWN_SECRET: &str = "hunter2-browser-secret";

pub(crate) struct Fixture {
    _root: tempfile::TempDir,
    pub(crate) home: PathBuf,
    pub(crate) profile: PathBuf,
    pub(crate) scratch: PathBuf,
    pub(crate) config: Arc<Mutex<BrowserConfig>>,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let base = std::fs::canonicalize(root.path()).unwrap();
        let home = base.join("home");
        let profile = home.join(".prometheus/profiles/default");
        let scratch = profile.join("cache/scratch/ses_b");
        for dir in [&home, &profile, &scratch] {
            std::fs::create_dir_all(dir).unwrap();
        }
        Self {
            _root: root,
            home,
            profile,
            scratch,
            config: Arc::new(Mutex::new(BrowserConfig::default())),
        }
    }

    pub(crate) fn browser_cache(&self) -> PathBuf {
        self.profile.join("cache/browser")
    }

    pub(crate) fn env(&self) -> ChildEnv {
        let launch = LaunchEnv::from_vars(
            [
                (
                    OsString::from("PATH"),
                    std::env::var_os("PATH").unwrap_or_default(),
                ),
                ("HOME".into(), self.home.clone().into()),
                ("LANG".into(), "C".into()),
            ],
            &EnvPolicy {
                provider_secret_names: &[],
                replacements: &[],
            },
        );
        let mut env = ChildEnv::build(&launch, EnvRequest::default());
        env.set_temp_dir(&self.scratch);
        env
    }

    pub(crate) fn chromium() -> RuntimePolicy {
        RuntimePolicy {
            runtimes: Arc::new(NoRuntimes),
            allow_lazy_install: false,
            host_candidates: browsers(),
        }
    }

    pub(crate) fn manager(&self) -> Arc<BrowserManager> {
        self.manager_with(Self::chromium(), BTreeMap::new())
    }

    pub(crate) fn manager_with(
        &self,
        runtime: RuntimePolicy,
        providers: BTreeMap<String, Arc<dyn CloudProvider>>,
    ) -> Arc<BrowserManager> {
        let config = Arc::clone(&self.config);
        BrowserManager::new(BrowserSetup {
            config: Arc::new(move || config.lock().unwrap().clone()),
            runtime,
            env: self.env(),
            cache_dir: self.browser_cache(),
            safety: UrlSafety {
                destinations: DestinationPolicy::new(true, &[]).unwrap(),
                lookup: Arc::new(SystemLookup),
                secrets: Redactor::new([&SecretString::new(KNOWN_SECRET)]),
            },
            providers,
        })
    }

    pub(crate) fn exec(&self, manager: &Arc<BrowserManager>, image_input: bool) -> BrowserExec {
        let paths = Paths {
            user_home: self.home.clone(),
            prometheus_home: self.home.join(".prometheus"),
            workspace: self.home.clone(),
            scratch: vec![self.scratch.clone()],
        };
        let policy = Policy::from_capabilities(pmt_core::capability::BASELINE, &paths).unwrap();
        let sandbox =
            Sandbox::with_options(policy, &SandboxConfig::default(), Options::default()).unwrap();
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut pythons = Vec::new();
        for name in ["python3", "python"] {
            pythons.extend(pmt_tools::terminal::shell::find_all(name, &path, None));
        }
        BrowserExec::new(ExecSetup {
            manager: Arc::clone(manager),
            python: RuntimePolicy {
                runtimes: Arc::new(NoRuntimes),
                allow_lazy_install: false,
                host_candidates: pythons,
            },
            sandbox: Arc::new(sandbox),
            env: self.env(),
            workspace_root: self.scratch.join("browser"),
            image_input,
        })
    }
}

/// Chromium-family browsers of this host, then Playwright's downloads.
pub(crate) fn browsers() -> Vec<PathBuf> {
    let mut found =
        pmt_tools::browser::launch::system_candidates(std::env::var_os("PATH").as_deref());
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    for cache in [
        home.join(".cache/ms-playwright"),
        home.join("Library/Caches/ms-playwright"),
    ] {
        let Ok(entries) = std::fs::read_dir(&cache) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("chromium-"))
            })
            .collect();
        dirs.sort();
        for dir in dirs.iter().rev() {
            for exe in [
                "chrome-linux/chrome",
                "chrome-linux64/chrome",
                "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
                "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
                "chrome-mac/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            ] {
                found.push(dir.join(exe));
            }
        }
    }
    let found: Vec<PathBuf> = found.into_iter().filter(|p| p.is_file()).collect();
    assert!(
        !found.is_empty(),
        "no Chromium-family browser on this host: install Chrome/Chromium or run `npx playwright install chromium`"
    );
    found
}

/// A local site: `/` (title "Test page"), `/form` and `/dialog`.
pub(crate) async fn site() -> MockServer {
    let server = MockServer::start().await;
    let page = |body: &str| {
        ResponseTemplate::new(200).set_body_raw(body.to_owned(), "text/html; charset=utf-8")
    };
    Mock::given(path("/"))
        .respond_with(page(
            "<!doctype html><title>Test page</title><h1>Hello from the test page</h1>\
             <input id=q><button id=b style='position:absolute;left:10px;top:100px;width:80px;height:30px' \
             onclick=\"document.title='clicked'\">Go</button>",
        ))
        .mount(&server)
        .await;
    server
}

pub(crate) async fn run(exec: &BrowserExec, code: &str) -> (ToolResult, Value) {
    run_in(exec, None, code).await
}

pub(crate) async fn run_in(
    exec: &BrowserExec,
    session: Option<&str>,
    code: &str,
) -> (ToolResult, Value) {
    let result = exec
        .exec(
            "ses_b",
            &ExecArgs {
                code: code.to_owned(),
                session: session.map(str::to_owned),
                timeout_secs: Some(60),
            },
        )
        .await;
    let body: Value = serde_json::from_str(&result.text_content())
        .unwrap_or_else(|_| Value::String(result.text_content()));
    (result, body)
}

/// Record files under `dir`.
pub(crate) fn records(cache: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(cache.join("records"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}
