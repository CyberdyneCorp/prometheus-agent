//! Host files of a session for the webapp (spec: web-dashboard, Webapp
//! file staging and terminals): a file downloads only through the
//! session's file-access rules, the ones its file tools apply on the
//! host: the device-namespace and protected-secret guards (dotenv files,
//! process memory), then the session's capability set (the profile's
//! baseline and grants plus the session's own tool output) with the
//! credential stores its sandbox denies. Relative paths resolve against
//! the session's working directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_config::grants::{GrantsFile, Subject, baseline};
use pmt_core::capability::{Access, Places};
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_store::Store;
use pmt_tools::files::{ControlPaths, FileBackend as _, Kind, LocalBackend, guards};

/// The largest file served (the whole file is read into memory).
pub(super) const MAX_DOWNLOAD: u64 = 100 * 1024 * 1024;

/// A downloadable file: its name and bytes.
pub(super) struct HostFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

fn refused(message: impl Into<String>) -> Error {
    Error::new(&codes::CAPABILITY_NOT_GRANTED, message)
}

/// The backend the session's file tools use on the host, for `cwd`
/// (`session`: also the session's own tool output).
fn backend(
    scope: &ProfileScope,
    session: Option<&str>,
    cwd: &Path,
    user_home: Option<&Path>,
) -> Result<LocalBackend, Error> {
    let granted = GrantsFile::of(scope.profile())
        .load()?
        .capabilities(&Subject::Profile);
    let mut capabilities = baseline(&scope.config().security).union(&granted);
    if let Some(session) = session {
        capabilities = capabilities.union(&pmt_tools::environments::tool_output_reads(
            scope.dir(),
            session,
        ));
    }
    let home = user_home.map(Path::to_path_buf).unwrap_or_default();
    let places = Places {
        user_home: home.clone(),
        workspace: cwd.to_path_buf(),
        scratch: Vec::new(),
        toolchain: Vec::new(),
    };
    let paths = pmt_sandbox::Paths {
        user_home: home,
        prometheus_home: scope.home().to_path_buf(),
        workspace: cwd.to_path_buf(),
        scratch: Vec::new(),
    };
    let policy = pmt_sandbox::Policy::from_set(&capabilities, &paths);
    Ok(LocalBackend::new(capabilities, places).denying_stores(Arc::new(policy)))
}

/// Reads `raw` for session `session` of the profile.
///
/// # Errors
///
/// `session.not_found`; `capability.not_granted` (403) for a path the
/// session may not read, a directory or a file over [`MAX_DOWNLOAD`];
/// `store.blob_missing` (404) for a missing file.
pub(super) fn read(
    scope: &ProfileScope,
    store: &Store,
    session: &str,
    raw: &str,
    user_home: Option<&Path>,
) -> Result<HostFile, Error> {
    let id: SessionId = session.parse()?;
    let record = store.session(id)?;
    if let Some(refusal) = guards::device_namespace(raw) {
        return Err(refused(refusal));
    }
    let cwd = record
        .cwd
        .map_or_else(|| scope.dir().join("workspace"), PathBuf::from);
    let path = guards::absolute(raw, &cwd, user_home);
    if let Some(refusal) = guards::read_refusal(&path, raw) {
        return Err(refused(refusal));
    }
    let backend = backend(scope, Some(session), &cwd, user_home)?;
    let meta = backend.metadata(&path).map_err(|e| io_error(&path, &e))?;
    if meta.kind != Kind::File {
        return Err(refused(format!("{raw} is not a file")));
    }
    if meta.len > MAX_DOWNLOAD {
        return Err(refused(format!(
            "{raw} is larger than {} MiB",
            MAX_DOWNLOAD / 1024 / 1024
        )));
    }
    let bytes = backend.read(&path).map_err(|e| io_error(&path, &e))?;
    let name = path.file_name().map_or_else(
        || "download".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    Ok(HostFile { name, bytes })
}

fn io_error(path: &Path, err: &std::io::Error) -> Error {
    match err.kind() {
        std::io::ErrorKind::PermissionDenied => refused(err.to_string()),
        std::io::ErrorKind::NotFound => Error::new(
            &codes::STORE_BLOB_MISSING,
            format!("{} does not exist", path.display()),
        ),
        _ => Error::io(&codes::HOME_IO, path, err),
    }
}

/// The Files page's rules below workspace root `root` (spec: web-dashboard,
/// Files page): the ones a session working in `root` applies on the host,
/// so the page never reaches what the session's file tools may not. Reads
/// pass the device-namespace and protected-secret guards; writes the
/// device-namespace and control-file guards; both the capability set
/// (baseline, grants, the session's tool output) with the credential
/// stores its sandbox denies.
///
/// # Errors
///
/// When the profile's grants cannot be read.
pub(super) fn rules(
    scope: &ProfileScope,
    session: Option<&str>,
    root: &Path,
    user_home: Option<&Path>,
) -> Result<pmt_web::FileRules, Error> {
    let backend = backend(scope, session, root, user_home)?;
    let control = ControlPaths {
        prometheus_home: scope.home().to_path_buf(),
    };
    Ok(Box::new(move |path: &Path, access: Access| {
        let shown = path.to_string_lossy();
        if let Some(refusal) = guards::device_namespace(&shown) {
            return Err(refusal);
        }
        let guarded = match access {
            Access::Read => guards::read_refusal(path, &shown),
            Access::Write => control.write_refusal(&guards::resolve_existing(path)),
        };
        if let Some(refusal) = guarded {
            return Err(refusal);
        }
        backend.check(access, path).map_err(|e| e.to_string())
    }))
}
