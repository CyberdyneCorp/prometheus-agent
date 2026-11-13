//! The desktop app in the background and at launch (spec: desktop-app),
//! end to end in the desktop browser harness (`common/desktop.rs`): the
//! page in Chromium at the webview origin, its `shell` command served by
//! the real shell core with the real binary as the bundled one.

#![cfg(all(unix, feature = "dashboard"))]

mod common;

use std::fmt::Write as _;
use std::path::Path;

use common::dashboard::playwright;
use common::desktop::Setup;
use common::{append_config, mock_home, ok, run};

/// Stops the home's daemon (the desktop started it) when the test ends.
struct Stop<'a>(&'a Path);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        let _ = run(self.0, &["daemon", "stop"]);
    }
}

/// The mock provider's script: "clean" asks to run `rm -rf build`,
/// "slow" answers after a few seconds, "hello" answers at once.
fn script(home: &Path) {
    let script = home.join("script.toml");
    std::fs::write(
        &script,
        "[[rule]]\ntool_result_regex = \"^BLOCKED: \"\ntext = \"refused\"\nrepeat = true\n\
         [[rule]]\nuser = \"clean\"\nrepeat = true\n[[rule.tool_calls]]\nname = \"terminal\"\n\
         arguments = { command = \"rm -rf build\" }\n\
         [[rule]]\nuser = \"slow\"\ntext = \"finished slowly\"\nlatency_ms = 2500\nrepeat = true\n\
         [[rule]]\nuser = \"hello\"\ntext = \"hi there\"\nrepeat = true\n",
    )
    .unwrap();
    let mut text = String::new();
    let _ = write!(
        text,
        "\n[providers.mock]\nscript = {:?}\n",
        script.to_string_lossy()
    );
    append_config(home, &text);
    std::fs::create_dir_all(home.join("profiles/default/workspace/build")).unwrap();
}

/// A frame document: it writes its own storage key, then shows the keys
/// it sees, what `sdk.sessions.list()` and a native command give it.
fn panel_document(id: &str) -> String {
    format!(
        "<!doctype html><html><head><title>{id}</title></head><body>\
         <p id=\"keys\"></p><p id=\"sessions\"></p><p id=\"native\"></p><script>\
         localStorage.setItem('own-{id}', '1');\
         setTimeout(() => {{ document.getElementById('keys').textContent = Object.keys(localStorage).sort().join(','); }}, 1500);\
         sdk.register();\
         sdk.sessions.list().then((s) => {{ document.getElementById('sessions').textContent = 'sessions ' + s.length; }},\
           (e) => {{ document.getElementById('sessions').textContent = e.message; }});\
         new Promise((ok, no) => {{ const id = 999; addEventListener('message', (e) => {{ if (e.data && e.data.id === id) (e.data.error ? no(new Error(e.data.error)) : ok(e.data.result)); }});\
           parent.postMessage({{ prometheus: 1, id, method: 'read_clipboard' }}, '*'); }})\
           .catch((e) => {{ document.getElementById('native').textContent = e.message; }});\
         </script></body></html>"
    )
}

/// Writes plugin `id` with a `[ui]` panel asking for `sdk`.
fn panel_plugin(root: &Path, id: &str, sdk: &str) -> std::path::PathBuf {
    let dir = root.join(id);
    std::fs::create_dir_all(dir.join("ui")).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "manifest_version = 1\nid = \"{id}\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
             description = \"desktop panel\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
             worlds = []\n\n[capabilities]\nui = true\n\n[ui]\nlabel = \"Panel {id}\"\nsdk = [{sdk}]\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("ui/index.html"), panel_document(id)).unwrap();
    dir
}

/// `a` (no SDK capability) and `b` (`sessions.read`) installed from
/// unsigned packages with `--allow-unsigned`; `c` placed by hand and
/// adopted, so its unverified panel does not load.
fn panels(home: &Path) {
    let src = home.join("src");
    for (id, sdk) in [("a", ""), ("b", "\"sessions.read\"")] {
        let dir = panel_plugin(&src, id, sdk);
        let package = src.join(format!("{id}.pmtpkg"));
        ok(&run(
            home,
            &[
                "plugins",
                "pack",
                dir.to_str().unwrap(),
                "--out",
                package.to_str().unwrap(),
            ],
        ));
        ok(&run(
            home,
            &[
                "plugins",
                "install",
                package.to_str().unwrap(),
                "--allow-unsigned",
                "--enable",
                "--grant",
                "all",
            ],
        ));
    }
    let c = panel_plugin(&home.join("profiles/default/plugins"), "c", "");
    ok(&run(
        home,
        &["plugins", "adopt", c.to_str().unwrap(), "--grant", "all"],
    ));
}

/// Scenario: Background approval (a notification click opens the card
/// without approving). Scenario: Quit during a turn. Scenario: Plugin
/// reads another plugin's storage. Scenario: Ungranted capability.
/// Scenario: Quick follow-up.
#[test]
fn background_approvals_quitting_plugin_panels_and_quick_ask() {
    let home = mock_home();
    eprintln!("DEBUGHOME {}", home.path().display());
    let home = Box::leak(Box::new(home));
    script(home.path());
    panels(home.path());
    let _stop = Stop(home.path());
    let harness = common::desktop::start(home.path());
    playwright(
        home.path(),
        "desktop-background.spec.ts",
        &[("PMT_DESKTOP_HARNESS", harness.url.as_str())],
    );
}

/// Scenario: Launch in a project directory (the window side). Scenario:
/// Typo in a launch option. Scenario: Website install button. Scenario:
/// Link pointing at an arbitrary URL. Scenario: Theme changed in the
/// webapp. Scenario: Wayland without the portal. Scenario: Apply on a
/// commit build (the window side).
#[test]
fn launch_links_install_dialog_shared_preferences_and_platform_support() {
    let home = mock_home();
    append_config(home.path(), "\n[desktop]\ndisable_gpu = \"yes\"\n");
    let catalog = common::plugin_catalog::CatalogServer::start(home.path());
    catalog.publish_weather(home.path());
    let cwd = home.path().join("src/api");
    std::fs::create_dir_all(&cwd).unwrap();
    let link = format!(
        "prometheus://new?profile=default&cwd={}",
        url_encode(&cwd.to_string_lossy())
    );
    let _stop = Stop(home.path());
    let harness = common::desktop::start_with(
        home.path(),
        Setup {
            facts: pmt_desktop::platform::Facts {
                os: "linux".into(),
                wayland: true,
                sni_host: Some(true),
            },
            links: vec![link],
            ..Setup::default()
        },
    );
    let cwd_text = cwd.to_string_lossy().into_owned();
    playwright(
        home.path(),
        "desktop-launch.spec.ts",
        &[
            ("PMT_DESKTOP_HARNESS", harness.url.as_str()),
            ("PMT_LAUNCH_CWD", cwd_text.as_str()),
        ],
    );
    // Cancelled first, installed on Install: the package was fetched once.
    let packages = catalog
        .asked()
        .into_iter()
        .filter(|p| p == "/weather.pmtpkg")
        .count();
    assert_eq!(packages, 1, "{:?}", catalog.asked());
    let listed = ok(&run(home.path(), &["plugins", "list"]));
    assert!(listed.contains("weather"), "{listed}");
}

fn url_encode(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}
