//! Embeds the bundled skill tree (`bundled/`) into the binary: one
//! `(relative path, include_bytes!)` entry per file, sorted, so bundled
//! skills are served read-only without ever being copied into a profile.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path
            .file_name()
            .is_some_and(|n| n != ".gitkeep" && n != ".DS_Store")
        {
            out.push(path);
        }
    }
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let root = manifest.join("bundled");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    collect(&root, &mut files);
    files.sort();
    let mut code = String::from("pub(crate) static FILES: &[(&str, &[u8])] = &[\n");
    for path in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        let rel = path
            .strip_prefix(&root)
            .expect("below root")
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let _ = writeln!(
            code,
            "    ({rel:?}, include_bytes!({:?})),",
            path.display().to_string()
        );
    }
    code.push_str("];\n");
    let out = PathBuf::from(env::var("OUT_DIR").expect("out dir")).join("bundled_files.rs");
    fs::write(out, code).expect("write bundled_files.rs");
}
