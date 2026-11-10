//! Embeds the bundled skill tree (`bundled/`) and the official optional
//! catalog (`official/`) into the binary: one `(relative path, bytes,
//! deflated)` entry per file, sorted, so bundled skills are served
//! read-only without ever being copied into a profile. Files are stored
//! deflated (the catalog is mostly Markdown and code) and inflated on
//! first read, except the bundled `SKILL.md` files: every session start
//! parses all of them for the skills index, and inflating them each time
//! cost more start time (~3 ms) than their plain bytes cost binary size
//! (~0.3 MiB).

use std::fmt::Write as _;
use std::io::Write as _;
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

/// `pub(crate) static <name>: &[(&str, &[u8], bool)]` over every file
/// below `root`, sorted; each file `keep_plain` does not name is deflated
/// into `out` and included from there.
fn embed(code: &mut String, name: &str, root: &Path, out: &Path, keep_plain: fn(&Path) -> bool) {
    println!("cargo:rerun-if-changed={}", root.display());
    let mut files = Vec::new();
    collect(root, &mut files);
    files.sort();
    let store = out.join(name);
    let _ = fs::remove_dir_all(&store);
    fs::create_dir_all(&store).expect("create the deflated store");
    let _ = writeln!(
        code,
        "pub(crate) static {name}: &[(&str, &[u8], bool)] = &["
    );
    for (index, path) in files.iter().enumerate() {
        println!("cargo:rerun-if-changed={}", path.display());
        let rel = path
            .strip_prefix(root)
            .expect("below root")
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if keep_plain(path) {
            let _ = writeln!(
                code,
                "    ({rel:?}, include_bytes!({:?}), false),",
                path.display().to_string()
            );
            continue;
        }
        let bytes = fs::read(path).expect("read a bundled file");
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&bytes).expect("deflate");
        let packed = store.join(format!("{index}.deflate"));
        fs::write(&packed, encoder.finish().expect("deflate")).expect("write a deflated file");
        let _ = writeln!(
            code,
            "    ({rel:?}, include_bytes!({:?}), true),",
            packed.display().to_string()
        );
    }
    code.push_str("];\n");
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let out = PathBuf::from(env::var("OUT_DIR").expect("out dir"));
    let mut code = String::new();
    embed(
        &mut code,
        "FILES",
        &manifest.join("bundled"),
        &out,
        |path| path.file_name().is_some_and(|n| n == "SKILL.md"),
    );
    embed(
        &mut code,
        "OFFICIAL_FILES",
        &manifest.join("official"),
        &out,
        |_| false,
    );
    fs::write(out.join("bundled_files.rs"), code).expect("write bundled_files.rs");
}
