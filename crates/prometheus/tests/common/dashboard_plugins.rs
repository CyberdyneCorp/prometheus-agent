//! Dashboard plugins of a test home (spec: web-dashboard, Dashboard plugin
//! contributions): `a` and `b` both override `/sessions` (frame-only
//! plugins whose page reports what `document.cookie` shows it), `mute`
//! overrides `/logs` and never registers, and `weather` adds a page after
//! Sessions, a `header-right` slot and backend routes (`ui-panel`).

use std::path::Path;

use super::{ok, run};

/// A frame document that registers (unless `register` is false), shows
/// `heading`, what `document.cookie` gives it, its context, and with
/// `forecast` the answer of its backend's `/forecast`.
fn document(heading: &str, register: bool, forecast: bool) -> String {
    let mut script = String::from(
        "let c; try { c = document.cookie; } catch (e) { c = 'blocked'; }\n\
         document.getElementById('cookie').textContent = 'cookie: ' + (c === '' ? '(empty)' : c);\n",
    );
    if register {
        script.push_str(
            "prometheus.register().then(() => prometheus.context()).then((x) => {\n\
             document.getElementById('context').textContent = 'route ' + x.route + ' slot ' + x.slot; });\n",
        );
    }
    if forecast {
        script.push_str(
            "prometheus.api('GET', '/forecast').then((x) => {\n\
             document.getElementById('forecast').textContent = 'forecast ' + x.forecast; },\n\
             (e) => { document.getElementById('forecast').textContent = 'failed ' + e.message; });\n",
        );
    }
    format!(
        "<!doctype html><html><head><title>{heading}</title></head><body>\
         <h1>{heading}</h1><p id=\"cookie\"></p><p id=\"context\"></p><p id=\"forecast\"></p>\
         <script>{script}</script></body></html>"
    )
}

/// Writes plugin `id`'s directory with its `[dashboard]` text and frame
/// document; a `ui-panel` plugin also gets its component.
fn place(home: &Path, id: &str, worlds: &str, dashboard: &str, page: &str) {
    let dir = home.join("profiles/default/plugins").join(id);
    std::fs::create_dir_all(dir.join("dashboard")).unwrap();
    if worlds.contains("ui-panel") {
        pmt_plugins::testkit::install("testing-panel", &dir);
    }
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "manifest_version = 1\nid = \"{id}\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
             description = \"dashboard test plugin\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
             worlds = [{worlds}]\n\n{dashboard}"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("dashboard/index.html"), page).unwrap();
}

/// Adopts and enables plugin `id`.
pub fn enable(home: &Path, id: &str) {
    let dir = home.join("profiles/default/plugins").join(id);
    ok(&run(home, &["plugins", "adopt", dir.to_str().unwrap()]));
    ok(&run(home, &["plugins", "enable", id, "--grant", "all"]));
}

/// Places and enables `a`, `b`, `mute` and `weather`.
pub fn install(home: &Path) {
    let overriding = |label: &str, route: &str| {
        format!(
            "[dashboard]\nlabel = \"{label}\"\n[dashboard.tab]\npath = \"/{}\"\noverride = \"{route}\"\nhidden = true\n",
            label.to_lowercase().replace(' ', "-")
        )
    };
    place(
        home,
        "a",
        "",
        &overriding("Plugin A", "/sessions"),
        &document("Plugin A page", true, false),
    );
    place(
        home,
        "b",
        "",
        &overriding("Plugin B", "/sessions"),
        &document("Plugin B page", true, false),
    );
    place(
        home,
        "mute",
        "",
        &overriding("Mute", "/logs"),
        &document("Mute page", false, false),
    );
    place(
        home,
        "weather",
        "\"ui-panel@1.0\"",
        "[dashboard]\nlabel = \"Weather\"\nslots = [\"header-right\"]\n\
         [dashboard.tab]\npath = \"/weather\"\nposition = \"after:sessions\"\n",
        &document("Weather page", true, true),
    );
    for id in ["a", "b", "mute", "weather"] {
        enable(home, id);
    }
}
