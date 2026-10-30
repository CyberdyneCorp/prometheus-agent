//! Owner-only files and directories under the Prometheus home: the home
//! can hold prompts, paths and pids, so everything is created 0700/0600 on
//! POSIX and, on Windows, under a home whose DACL grants only the owner
//! ([`restrict_to_owner`]); files are opened without following symbolic
//! links, junctions or other reparse points (specs: configuration-and-profiles,
//! home layout; security-boundaries, Private file modes).

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
/// existing file keeps its mode; callers tighten it when they care). A
/// symbolic link (or Windows reparse point) at `path` is not followed: the
/// open fails instead of writing through it.
#[must_use]
pub fn append_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(0o600)
            .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits());
    }
    #[cfg(windows)]
    {
        /// `FILE_FLAG_OPEN_REPARSE_POINT`: open the link itself, which
        /// cannot be appended to, instead of its target.
        const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        std::os::windows::fs::OpenOptionsExt::custom_flags(&mut options, OPEN_REPARSE_POINT);
    }
    options
}

/// Restricts `dir` to its owner. On Windows the DACL becomes protected
/// (inherited entries removed) and grants full control only to the current
/// user and SYSTEM, inherited by everything later created inside, the
/// equivalent of mode 0700. POSIX modes are set at creation, so this is a
/// no-op there.
///
/// # Errors
///
/// When `icacls`/`whoami` cannot run or report failure.
pub fn restrict_to_owner(dir: &Path) -> io::Result<()> {
    #[cfg(windows)]
    return owner_acl::restrict(dir);
    #[cfg(not(windows))]
    {
        let _ = dir;
        Ok(())
    }
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

#[cfg(windows)]
mod owner_acl {
    //! Owner-only DACLs through the system `icacls` tool (no unsafe FFI).

    use std::io;
    use std::path::Path;
    use std::process::Command;
    use std::sync::OnceLock;

    /// The well-known SID of the local SYSTEM account.
    const SYSTEM_SID: &str = "S-1-5-18";

    pub(super) fn restrict(dir: &Path) -> io::Result<()> {
        let user = current_user_sid()?;
        let grant = |sid: &str| format!("*{sid}:(OI)(CI)F");
        run(Command::new("icacls")
            .arg(dir)
            .args(["/inheritance:r", "/grant:r"])
            .arg(grant(&user))
            .arg(grant(SYSTEM_SID))
            .arg("/q"))
        .map(drop)
    }

    fn current_user_sid() -> io::Result<String> {
        static SID: OnceLock<String> = OnceLock::new();
        if let Some(sid) = SID.get() {
            return Ok(sid.clone());
        }
        // `"DOMAIN\\user","S-1-5-21-..."`
        let out = run(Command::new("whoami").args(["/user", "/fo", "csv", "/nh"]))?;
        let sid = out
            .trim()
            .rsplit(',')
            .next()
            .map(|field| field.trim_matches('"').to_owned())
            .filter(|sid| sid.starts_with("S-1-"))
            .ok_or_else(|| io::Error::other(format!("unexpected whoami output: {out}")))?;
        Ok(SID.get_or_init(|| sid).clone())
    }

    fn run(command: &mut Command) -> io::Result<String> {
        let output = command.output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "{command:?} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
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

    #[cfg(unix)]
    #[test]
    fn append_does_not_follow_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.log");
        std::fs::write(&target, "").unwrap();
        let link = dir.path().join("app.log");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(super::append_options().open(&link).is_err());
        assert!(
            super::append_options()
                .open(dir.path().join("new.log"))
                .is_ok()
        );
    }

    /// A home under a directory readable by Everyone ends up owner-only:
    /// the inherited Everyone entry is gone from it and from new files.
    #[cfg(windows)]
    #[test]
    fn restrict_to_owner_drops_inherited_access() {
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        create_dir(&shared).unwrap();
        let everyone = Command::new("icacls")
            .arg(&shared)
            .args(["/grant", "*S-1-1-0:(OI)(CI)R"])
            .status()
            .unwrap();
        assert!(everyone.success());
        let home = shared.join("home");
        create_dir(&home).unwrap();
        let acl = |p: &std::path::Path| {
            let out = Command::new("icacls").arg(p).output().unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        assert!(
            acl(&home).contains("Everyone"),
            "precondition: {}",
            acl(&home)
        );

        super::restrict_to_owner(&home).unwrap();
        let file = home.join("secrets.toml");
        write_atomic(&file, b"x").unwrap();
        for path in [&home, &file] {
            let acl = acl(path);
            assert!(!acl.contains("Everyone"), "{acl}");
            assert!(!acl.contains("BUILTIN\\Users"), "{acl}");
        }
    }
}
