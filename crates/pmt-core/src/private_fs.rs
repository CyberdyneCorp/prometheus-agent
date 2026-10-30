//! Owner-only files and directories under the Prometheus home: the home
//! can hold prompts, paths and pids, so everything is created 0700/0600 on
//! POSIX (spec: configuration-and-profiles, home layout).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::Path;

/// Creates `dir` and its missing parents with mode 0700.
///
/// # Errors
///
/// Any I/O error from creating the directory.
pub fn create_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// Open options for a new 0600 file; `create_new` refuses to reuse a path.
#[must_use]
pub fn new_file_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

/// Open options that append to `path`, creating it 0600 when missing (an
/// existing file keeps its mode; callers tighten it when they care).
#[must_use]
pub fn append_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

/// Replaces `path` with `bytes` atomically: readers see the old or the new
/// contents, never a torn file. The data goes to a 0600 sibling temp file
/// that is synced and then renamed over `path`.
///
/// # Errors
///
/// Any I/O error; the temp file is removed on failure.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let mut temp_name = name.to_os_string();
    temp_name.push(format!(".tmp.{}", std::process::id()));
    let temp = path.with_file_name(temp_name);
    let _ = fs::remove_file(&temp);
    let written = new_file_options()
        .open(&temp)
        .and_then(|mut file: File| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&temp, path));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

/// Copies the regular files and directories under `src` into `dst`
/// (created 0700, files 0600), replacing existing files. Symbolic links
/// and special files are skipped, never followed.
///
/// # Errors
///
/// Any I/O error.
pub fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    create_dir(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            copy_file(&entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Copies one file to `dst` with mode 0600, replacing `dst` atomically.
///
/// # Errors
///
/// Any I/O error.
pub fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    write_atomic(dst, &fs::read(src)?)
}

#[cfg(test)]
mod tests {
    use super::{copy_tree, create_dir, write_atomic};

    #[test]
    fn copy_tree_copies_files_and_skips_links() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        create_dir(&src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), "a").unwrap();
        std::fs::write(src.join("sub/b.txt"), "b").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hosts", src.join("link")).unwrap();
        let dst = dir.path().join("dst");
        copy_tree(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("sub/b.txt")).unwrap(), "b");
        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "a");
        assert!(!dst.join("link").exists());
    }

    #[test]
    fn atomic_write_replaces_contents_privately() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("a/b");
        create_dir(&sub).unwrap();
        let path = sub.join("record.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(
            std::fs::read_dir(&sub).unwrap().count(),
            1,
            "no temp file left"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode();
            assert_eq!(mode(&sub) & 0o777, 0o700);
            assert_eq!(mode(&path) & 0o777, 0o600);
        }
    }
}
