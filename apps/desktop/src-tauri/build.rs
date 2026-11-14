//! Generates the Tauri context; `shell` is the app's only command, so the
//! capability file can grant exactly it.

fn main() {
    let manifest = tauri_build::AppManifest::new().commands(&["shell"]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("the Tauri build step failed");
}
