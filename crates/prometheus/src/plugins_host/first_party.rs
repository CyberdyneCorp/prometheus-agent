//! The first-party plugins compiled into the binary, each disabled by
//! default (spec: plugin-system, First-party disk-cleanup plugin;
//! First-party `google_meet` plugin). They are python plugins: their files
//! are written under the profile when enabled and hosted by the bridge
//! (`pmt_plugins::embedded`).

use pmt_plugins::discovery::EmbeddedPlugin;

/// `(path, content)` of each listed file of `plugins/first-party/<dir>`.
macro_rules! files {
    ($dir:literal: $($file:literal),+ $(,)?) => {
        &[$(($file, include_str!(concat!("../../../../plugins/first-party/", $dir, "/", $file)))),+]
    };
}

const DISK_CLEANUP: EmbeddedPlugin = EmbeddedPlugin {
    manifest: include_str!("../../../../plugins/first-party/disk-cleanup/plugin.toml"),
    component: b"",
    files: files!("disk-cleanup": "plugin.toml", "__init__.py", "disk_cleanup.py"),
    enabled_by_default: false,
};

const GOOGLE_MEET: EmbeddedPlugin = EmbeddedPlugin {
    manifest: include_str!("../../../../plugins/first-party/google_meet/plugin.toml"),
    component: b"",
    files: files!(
        "google_meet":
        "plugin.toml",
        "__init__.py",
        "audio_bridge.py",
        "cli.py",
        "jsonfile.py",
        "meet_bot.py",
        "meeturl.py",
        "node_client.py",
        "node_protocol.py",
        "node_registry.py",
        "node_server.py",
        "process_manager.py",
        "realtime.py",
        "runtime.py",
        "tools.py",
    ),
    enabled_by_default: false,
};

/// Every first-party plugin the binary carries.
pub const EMBEDDED: &[EmbeddedPlugin] = &[DISK_CLEANUP, GOOGLE_MEET];

#[cfg(test)]
mod tests {
    use super::*;

    /// A file added to a first-party plugin directory must be embedded too,
    /// or the enabled plugin would run without it.
    #[test]
    fn every_file_of_a_first_party_plugin_is_embedded() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/first-party");
        for (dir, plugin) in [("disk-cleanup", DISK_CLEANUP), ("google_meet", GOOGLE_MEET)] {
            let mut on_disk: Vec<String> = std::fs::read_dir(root.join(dir))
                .unwrap()
                .flatten()
                .filter(|e| e.path().is_file())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.'))
                .collect();
            on_disk.sort();
            let mut embedded: Vec<String> =
                plugin.files.iter().map(|(p, _)| (*p).to_owned()).collect();
            embedded.sort();
            assert_eq!(on_disk, embedded, "{dir}");
        }
    }
}
