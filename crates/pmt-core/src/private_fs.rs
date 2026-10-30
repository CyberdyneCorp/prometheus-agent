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

#[cfg(test)]
mod tests {
    use super::{create_dir, write_atomic};

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
