//! `fs.list` and `fs.read` (spec: desktop-app, File browser pane; Chat
//! transcript rendering): a session's workspace through the session's own
//! file tools state, so the entries of a container session come from the
//! container and every read passes the guards `read_file` applies
//! (device paths, protected secret files, the backend's capability
//! checks). The session is loaded first: loading attaches its
//! environment.

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use pmt_core::error::{Error, codes};
use pmt_gateway::Link;
use pmt_rpc::contracts::{FsEntry, FsFile, FsListParams, FsListing, FsReadParams};
use pmt_tools::environments::Environments;
use pmt_tools::files::{FileSession, Kind};

use super::{DaemonHost, Profile, session_id};

/// The most `fs.read` returns at once.
const MAX_READ: u64 = 8 * 1024 * 1024;

/// A file larger than this is never read whole to preview its start.
const MAX_PREVIEW_SOURCE: u64 = 256 * 1024 * 1024;

fn refused(message: String) -> Error {
    Error::new(&codes::CAPABILITY_NOT_GRANTED, message)
}

impl DaemonHost {
    pub(super) async fn session_files(
        &self,
        profile: &Arc<Profile>,
        session: &str,
        link: &Link,
    ) -> Result<Arc<FileSession>, Error> {
        self.load(profile, session_id(session)?, link).await?;
        Environments::process()
            .session(session)
            .and_then(|env| env.files.clone())
            .ok_or_else(|| {
                Error::new(
                    &codes::CAPABILITY_NOT_GRANTED,
                    "this session has no file tools, so its workspace cannot be browsed",
                )
            })
    }

    pub(super) async fn fs_list_of(
        &self,
        profile: &Arc<Profile>,
        params: FsListParams,
        link: &Link,
    ) -> Result<FsListing, Error> {
        let files = self
            .session_files(profile, &params.session_id, link)
            .await?;
        listing(&files, &params)
    }

    pub(super) async fn fs_read_of(
        &self,
        profile: &Arc<Profile>,
        params: FsReadParams,
        link: &Link,
    ) -> Result<FsFile, Error> {
        let files = self
            .session_files(profile, &params.session_id, link)
            .await?;
        read(&files, params)
    }
}

/// One directory of the session's workspace, through its backend.
fn listing(files: &FileSession, params: &FsListParams) -> Result<FsListing, Error> {
    let dir = normalize(&params.path);
    let listed = files
        .list_dir(if dir.is_empty() { "." } else { &dir })
        .map_err(refused)?;
    let rules = gitignore(files, &dir);
    let mut entries: Vec<FsEntry> = listed
        .into_iter()
        .filter(|e| matches!(e.kind, Kind::Dir | Kind::File))
        .map(|e| {
            let path = join(&dir, &e.name);
            let is_dir = e.kind == Kind::Dir;
            let ignored = e.name == ".git"
                || rules
                    .matched_path_or_any_parents(Path::new(&path), is_dir)
                    .is_ignore();
            FsEntry {
                name: e.name,
                path,
                dir: is_dir,
                size: (!is_dir).then_some(e.len),
                ignored,
            }
        })
        .filter(|e| params.show_ignored || !e.ignored)
        .collect();
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.cmp(&b.name)));
    Ok(FsListing {
        path: if dir.is_empty() { ".".to_owned() } else { dir },
        entries,
    })
}

/// The start of one file, through the session's backend.
fn read(files: &FileSession, params: FsReadParams) -> Result<FsFile, Error> {
    let size = files.binary_len(&params.path).map_err(refused)?;
    if size > MAX_PREVIEW_SOURCE {
        return Err(refused(format!(
            "{} is too large to preview ({size} bytes); open it externally",
            params.path
        )));
    }
    let mut bytes = files.read_binary(&params.path).map_err(refused)?;
    let limit = params.max_bytes.unwrap_or(MAX_READ).min(MAX_READ);
    let truncated = u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit;
    bytes.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    let mime = image_mime(&params.path);
    let kind = if mime.is_some() {
        "image"
    } else if looks_textual(&bytes) {
        "text"
    } else {
        "binary"
    };
    let absolute = files
        .on_host()
        .then(|| files.resolve(&params.path).to_string_lossy().into_owned());
    let workspace = absolute
        .is_some()
        .then(|| files.cwd().to_string_lossy().into_owned());
    Ok(FsFile {
        absolute,
        workspace,
        path: params.path,
        size,
        truncated,
        kind: kind.to_owned(),
        mime: mime.map(str::to_owned),
        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
    })
}

/// `src/./a/` → `src/a`; `.` → empty (the workspace root).
fn normalize(path: &str) -> String {
    path.split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/")
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// The `.gitignore` rules that apply in `dir`: the files of the workspace
/// root and of every directory down to `dir`, read through the session's
/// own backend (a container's rules come from the container).
fn gitignore(files: &FileSession, dir: &str) -> Gitignore {
    let mut builder = GitignoreBuilder::new("");
    let mut prefix = String::new();
    let mut parts = vec![""];
    parts.extend(dir.split('/').filter(|p| !p.is_empty()));
    for part in parts {
        if !part.is_empty() {
            prefix = join(&prefix, part);
        }
        let file = join(&prefix, ".gitignore");
        let Ok(bytes) = files.read_binary(&file) else {
            continue;
        };
        for line in String::from_utf8_lossy(&bytes).lines() {
            // A rule of a nested file applies under its directory.
            let rule = scoped(&prefix, line);
            if let Some(rule) = rule {
                let _ = builder.add_line(None, &rule);
            }
        }
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

/// A `.gitignore` line of directory `prefix` as a root-relative rule.
fn scoped(prefix: &str, line: &str) -> Option<String> {
    let line = line.trim_end();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    if prefix.is_empty() {
        return Some(line.to_owned());
    }
    let (negated, body) = line
        .strip_prefix('!')
        .map_or((false, line), |rest| (true, rest));
    let anchored = body.trim_end_matches('/').contains('/');
    let body = body.trim_start_matches('/');
    let rule = if anchored {
        format!("/{prefix}/{body}")
    } else {
        format!("/{prefix}/**/{body}")
    };
    Some(if negated { format!("!{rule}") } else { rule })
}

fn image_mime(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        _ => return None,
    })
}

/// Text when the start is valid UTF-8 (a cut multi-byte character at the
/// end is fine) without NUL bytes.
fn looks_textual(bytes: &[u8]) -> bool {
    if bytes.contains(&0) {
        return false;
    }
    match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(err) => err.error_len().is_none(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use pmt_core::capability::Access;
    use pmt_rpc::contracts::{FsListParams, FsReadParams};
    use pmt_tools::files::{ControlPaths, FileSession, FileSetup, Kind, Linter};
    use pmt_tools::files::{Entry, FileBackend, Meta};

    use super::{listing, looks_textual, normalize, read, scoped};

    /// A container's filesystem: only what is in the map exists, and its
    /// paths are not the host's.
    struct ContainerFs(BTreeMap<PathBuf, Vec<u8>>);

    impl ContainerFs {
        fn missing() -> io::Error {
            io::Error::from(io::ErrorKind::NotFound)
        }
    }

    impl FileBackend for ContainerFs {
        fn name(&self) -> &'static str {
            "container"
        }
        fn uses_host_filesystem(&self) -> bool {
            false
        }
        fn metadata(&self, path: &Path) -> io::Result<Meta> {
            if let Some(bytes) = self.0.get(path) {
                return Ok(Meta {
                    kind: Kind::File,
                    len: bytes.len() as u64,
                    modified: None,
                });
            }
            if self.0.keys().any(|p| p.starts_with(path)) {
                return Ok(Meta {
                    kind: Kind::Dir,
                    len: 0,
                    modified: None,
                });
            }
            Err(Self::missing())
        }
        fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
            self.0.get(path).cloned().ok_or_else(Self::missing)
        }
        fn write(&self, _: &Path, _: &[u8]) -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        }
        fn remove(&self, _: &Path) -> io::Result<()> {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        }
        fn list_dir(&self, path: &Path) -> io::Result<Vec<Entry>> {
            let mut names: BTreeMap<String, Entry> = BTreeMap::new();
            for (file, bytes) in &self.0 {
                let Ok(rest) = file.strip_prefix(path) else {
                    continue;
                };
                let mut parts = rest.components();
                let Some(first) = parts.next() else { continue };
                let name = first.as_os_str().to_string_lossy().into_owned();
                let dir = parts.next().is_some();
                let entry = Entry {
                    name: name.clone(),
                    kind: if dir { Kind::Dir } else { Kind::File },
                    len: if dir { 0 } else { bytes.len() as u64 },
                };
                names.insert(name, entry);
            }
            if names.is_empty() {
                return Err(Self::missing());
            }
            Ok(names.into_values().collect())
        }
        fn check(&self, _: Access, _: &Path) -> io::Result<()> {
            Ok(())
        }
    }

    fn container_session() -> FileSession {
        let files: BTreeMap<PathBuf, Vec<u8>> = [
            ("/work/src/main.rs", "fn main() {}"),
            ("/work/src/lib.rs", "pub fn f() {}"),
            ("/work/target/out.bin", "x"),
            ("/work/.gitignore", "target/\n"),
        ]
        .into_iter()
        .map(|(p, c)| (PathBuf::from(p), c.as_bytes().to_vec()))
        .collect();
        FileSession::new(FileSetup {
            backend: Arc::new(ContainerFs(files)),
            cwd: Arc::new(|| PathBuf::from("/work")),
            user_home: None,
            control: ControlPaths {
                prometheus_home: PathBuf::from("/nonexistent-home"),
            },
            read_max_chars: pmt_tools::setting::fixed(100_000),
            linter: Linter::builtin(),
            documents: None,
            edits: None,
        })
    }

    /// Scenario: Container session.
    #[test]
    fn a_container_sessions_entries_come_from_the_container() {
        let files = container_session();
        let list = |path: &str, show_ignored: bool| {
            let params = FsListParams {
                session_id: "ses".into(),
                path: path.into(),
                show_ignored,
            };
            listing(&files, &params).unwrap()
        };
        let src = list("src", false);
        let names: Vec<&str> = src.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["lib.rs", "main.rs"]);
        assert_eq!(src.entries[1].path, "src/main.rs");
        let root: Vec<String> = list(".", false)
            .entries
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(root, ["src", ".gitignore"], "target/ is ignored");
        assert!(
            list(".", true)
                .entries
                .iter()
                .any(|e| e.name == "target" && e.ignored)
        );
        let file = read(
            &files,
            FsReadParams {
                session_id: "ses".into(),
                path: "src/main.rs".into(),
                max_bytes: None,
            },
        )
        .unwrap();
        assert_eq!(file.kind, "text");
        assert_eq!(
            file.absolute, None,
            "a container path is not opened on the host"
        );
    }

    #[test]
    fn nested_gitignore_rules_apply_under_their_directory() {
        assert_eq!(scoped("", "target/").as_deref(), Some("target/"));
        assert_eq!(scoped("web", "dist").as_deref(), Some("/web/**/dist"));
        assert_eq!(scoped("web", "/out/x").as_deref(), Some("/web/out/x"));
        assert_eq!(scoped("web", "!keep").as_deref(), Some("!/web/**/keep"));
        assert_eq!(scoped("web", "# note"), None);
    }

    #[test]
    fn paths_are_workspace_relative() {
        assert_eq!(normalize("./src//a/"), "src/a");
        assert_eq!(normalize("."), "");
    }

    #[test]
    fn a_text_preview_cut_inside_a_character_is_still_text() {
        let text = "héllo".as_bytes();
        assert!(looks_textual(&text[..2]));
        assert!(!looks_textual(b"\x00\x01"));
    }
}
