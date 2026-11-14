//! Embeds the user documentation of this build (`docs/introduction.md`,
//! `docs/guide/` and the generated `docs/reference/`) for the dashboard's
//! offline `/docs` page: one `(relative path, text)` entry per Markdown
//! file, sorted.

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
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let docs = manifest.join("../../docs");
    let mut files = vec![docs.join("introduction.md")];
    for dir in ["guide", "reference"] {
        println!("cargo:rerun-if-changed={}", docs.join(dir).display());
        collect(&docs.join(dir), &mut files);
    }
    println!(
        "cargo:rerun-if-changed={}",
        docs.join("introduction.md").display()
    );
    files.sort();
    let mut code = String::from("pub(crate) static PAGES: &[(&str, &str)] = &[\n");
    for file in files.iter().filter(|f| f.is_file()) {
        let relative = file
            .strip_prefix(&docs)
            .expect("collected below docs/")
            .to_string_lossy()
            .replace('\\', "/");
        let absolute = file.canonicalize().expect("the file exists");
        let _ = writeln!(
            code,
            "    ({relative:?}, include_str!({:?})),",
            absolute.display().to_string()
        );
    }
    code.push_str("];\n");
    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets it")).join("docs.rs");
    fs::write(out, code).expect("OUT_DIR is writable");
}
