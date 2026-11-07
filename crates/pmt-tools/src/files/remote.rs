//! The file side of the container, SSH and plugin backends (spec:
//! execution-environments, File tools route through the backend; Backend
//! isolation descriptor).
//!
//! Paths are the backend's own: reading `/etc/hostname` on a container
//! backend reads the container's file. Every operation is one short POSIX
//! shell script run through the backend ([`RemoteExec`]); exit codes 2 and
//! 3 mean "not found" and "permission denied". Host capability checks do
//! not apply (the descriptor says the backend does not use the host
//! filesystem).

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use pmt_core::capability::Access;

use super::backend::{Entry, FileBackend, Kind, Meta};
use crate::terminal::remote::{ExecResult, RemoteExec};

/// `kind|size|mtime` of `$1`, following links; GNU/busybox `stat`, else
/// BSD `stat`.
const STAT: &str = "[ -e \"$1\" ] || exit 2\n\
    stat -L -c '%F|%s|%Y' -- \"$1\" 2>/dev/null || stat -L -f '%HT|%z|%m' -- \"$1\"";

/// `cat` with not-found and permission codes.
const READ: &str = "[ -e \"$1\" ] || exit 2\n[ -r \"$1\" ] || exit 3\nexec cat -- \"$1\"";

/// Atomic replace from stdin, creating parents.
const WRITE: &str = "d=$(dirname -- \"$1\") && mkdir -p -- \"$d\" || exit 3\n\
    t=\"$1.pmt-tmp.$$\"\n\
    cat > \"$t\" || { rm -f -- \"$t\"; exit 3; }\n\
    mv -f -- \"$t\" \"$1\"";

/// Removes a file.
const REMOVE: &str = "[ -e \"$1\" ] || [ -L \"$1\" ] || exit 2\nrm -f -- \"$1\" || exit 3";

/// One `kind|size|name` line per entry of `$1`.
const LIST: &str = "[ -d \"$1\" ] || exit 2\ncd -- \"$1\" || exit 3\n\
    for f in * .[!.]* ..?*; do\n\
      [ -e \"$f\" ] || [ -L \"$f\" ] || continue\n\
      if [ -d \"$f\" ]; then printf 'd|0|%s\\n' \"$f\"\n\
      elif [ -f \"$f\" ]; then printf 'f|%s|%s\\n' \"$(wc -c < \"$f\" | tr -d ' ')\" \"$f\"\n\
      else printf 'o|0|%s\\n' \"$f\"; fi\n\
    done";

/// The files of a remote backend.
#[derive(Debug, Clone)]
pub struct RemoteFiles {
    name: String,
    exec: Arc<dyn RemoteExec>,
}

impl RemoteFiles {
    /// Files of backend `name` through `exec`.
    #[must_use]
    pub fn new(name: &str, exec: Arc<dyn RemoteExec>) -> Self {
        Self {
            name: name.to_owned(),
            exec,
        }
    }

    fn call(&self, script: &str, path: &Path, stdin: Option<&[u8]>) -> io::Result<ExecResult> {
        let path = path.to_string_lossy();
        let result = self
            .exec
            .exec(script, &[&path], stdin)
            .map_err(|why| io::Error::other(format!("{} backend: {why}", self.name)))?;
        match result.code {
            0 => Ok(result),
            2 => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{path}: not found"),
            )),
            3 => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{path}: permission denied"),
            )),
            code => Err(io::Error::other(format!(
                "{path}: {} (exit {code})",
                String::from_utf8_lossy(&result.stderr).trim()
            ))),
        }
    }
}

fn kind_of(text: &str) -> Kind {
    let lower = text.to_ascii_lowercase();
    if lower.contains("directory") {
        Kind::Dir
    } else if lower.contains("regular") {
        Kind::File
    } else if lower.contains("character") {
        Kind::CharDevice
    } else if lower.contains("block") {
        Kind::BlockDevice
    } else if lower.contains("fifo") {
        Kind::Fifo
    } else if lower.contains("socket") {
        Kind::Socket
    } else {
        Kind::Other
    }
}

impl FileBackend for RemoteFiles {
    fn name(&self) -> &str {
        &self.name
    }

    fn uses_host_filesystem(&self) -> bool {
        false
    }

    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        let result = self.call(STAT, path, None)?;
        let text = String::from_utf8_lossy(&result.stdout);
        let mut fields = text.trim().split('|');
        let (Some(kind), Some(len), Some(mtime)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(io::Error::other(format!("unexpected stat reply: {text}")));
        };
        Ok(Meta {
            kind: kind_of(kind),
            len: len.trim().parse().unwrap_or(0),
            modified: mtime
                .trim()
                .parse::<u64>()
                .ok()
                .map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
        })
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        if let Some(read) = self.exec.download(&path.to_string_lossy()) {
            return read.map_err(|why| io::Error::other(format!("{} backend: {why}", self.name)));
        }
        Ok(self.call(READ, path, None)?.stdout)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(written) = self.exec.upload(&path.to_string_lossy(), bytes) {
            return written
                .map_err(|why| io::Error::other(format!("{} backend: {why}", self.name)));
        }
        self.call(WRITE, path, Some(bytes)).map(drop)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        self.call(REMOVE, path, None).map(drop)
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<Entry>> {
        let result = self.call(LIST, path, None)?;
        let mut entries: Vec<Entry> = String::from_utf8_lossy(&result.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.splitn(3, '|');
                let (kind, len, name) = (fields.next()?, fields.next()?, fields.next()?);
                Some(Entry {
                    name: name.to_owned(),
                    kind: match kind {
                        "d" => Kind::Dir,
                        "f" => Kind::File,
                        _ => Kind::Other,
                    },
                    len: len.parse().unwrap_or(0),
                })
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn check(&self, _access: Access, _path: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::kind_of;
    use crate::files::backend::Kind;

    #[test]
    fn gnu_and_bsd_stat_kinds_map_alike() {
        assert_eq!(kind_of("regular file"), Kind::File);
        assert_eq!(kind_of("Regular File"), Kind::File);
        assert_eq!(kind_of("regular empty file"), Kind::File);
        assert_eq!(kind_of("directory"), Kind::Dir);
        assert_eq!(kind_of("Character Device"), Kind::CharDevice);
        assert_eq!(kind_of("fifo"), Kind::Fifo);
    }
}
