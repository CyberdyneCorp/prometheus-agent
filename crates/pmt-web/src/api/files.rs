//! The files page's API (spec: web-dashboard, Files page): a profile's
//! workspace roots (`<profile>/workspace` and, when the profile runs its
//! sessions on the local backend, the working directories of its recent
//! sessions) with list, preview (text up to 2 MiB, images), download,
//! upload (capped by `dashboard.max_upload_bytes`), rename and confirmed
//! delete. Every path is resolved server-side, links included: a path
//! whose real location leaves the chosen root answers 403, and a listing
//! leaves out entries that lead outside it. Inside the root the page
//! applies the file-access rules of a session working there
//! ([`DashboardHost::file_rules`](crate::DashboardHost::file_rules)): a
//! session started in the user's home does not expose `.ssh`, dotenv
//! files or the Prometheus home through this page.

use std::path::{Component, Path, PathBuf};

use hyper::body::Incoming;
use pmt_core::capability::Access;
use pmt_store::ListOptions;
use serde_json::{Value, json};

use super::profile::Profile;
use crate::Shared;
use crate::host::FileRules;
use crate::http::{Reply, Req, bytes, error, json_reply, read_body, read_json};

/// The largest text preview.
const MAX_PREVIEW: u64 = 2 * 1024 * 1024;
/// Sessions whose working directories are offered as roots.
const SESSION_ROOTS: u32 = 20;

/// The roots of `at`: `(id, path)`. A session's working directory is a
/// host path only on the local backend (a container or SSH session's is
/// remote), so session roots are offered only when the profile's
/// sessions run there.
fn roots(at: &Profile<'_>) -> Vec<(String, PathBuf)> {
    let mut roots = vec![("workspace".to_owned(), at.scope.dir().join("workspace"))];
    if at.scope.config().terminal.backend != "local" {
        return roots;
    }
    let options = ListOptions {
        limit: Some(SESSION_ROOTS),
        ..ListOptions::default()
    };
    for session in at.store.list_sessions(&options).unwrap_or_default() {
        let Some(cwd) = session.cwd.map(PathBuf::from) else {
            continue;
        };
        if roots.iter().all(|(_, known)| *known != cwd) {
            roots.push((format!("session:{}", session.id), cwd));
        }
    }
    roots
}

fn forbidden() -> Reply {
    error(403, "outside_root", "the path leaves the workspace root")
}

fn refused(refusal: &str) -> Reply {
    error(403, "capability.not_granted", refusal)
}

/// A resolved path below a root, with the root's access rules.
struct Target {
    root: PathBuf,
    path: PathBuf,
    rules: FileRules,
}

impl Target {
    /// Refuses `path` for `access` under the root's rules.
    fn check(&self, path: &Path, access: Access) -> Result<(), Reply> {
        (self.rules)(path, access).map_err(|refusal| refused(&refusal))
    }
}

/// The real path of `rel` below the root `req` names, checked for
/// `access`, or the refusal.
fn resolve(
    shared: &Shared,
    at: &Profile<'_>,
    req: &Req,
    rel: &str,
    access: Access,
) -> Result<Target, Reply> {
    let id = req.param("root").unwrap_or_else(|| "workspace".to_owned());
    let Some((_, root)) = roots(at).into_iter().find(|(r, _)| *r == id) else {
        return Err(error(404, "not_found", &format!("no workspace root {id}")));
    };
    let root = root
        .canonicalize()
        .map_err(|_| error(404, "not_found", "the workspace root does not exist yet"))?;
    let relative = Path::new(rel.trim_start_matches('/'));
    if relative
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(forbidden());
    }
    let session = id.strip_prefix("session:");
    let rules = shared
        .host
        .file_rules(at.name, session, &root)
        .map_err(|e| crate::http::from_error(&e))?;
    let resolved = root
        .join(relative)
        .canonicalize()
        .map_err(|_| error(404, "not_found", &format!("no such file {rel}")))?;
    if !resolved.starts_with(&root) {
        return Err(forbidden());
    }
    let target = Target {
        root,
        path: resolved,
        rules,
    };
    target.check(&target.path, access)?;
    Ok(target)
}

fn path_param(req: &Req) -> String {
    req.param("path").unwrap_or_default()
}

/// `GET files`: the roots.
pub(crate) fn list_roots(at: &Profile<'_>) -> Reply {
    let roots: Vec<Value> = roots(at)
        .into_iter()
        .map(|(id, path)| json!({"id": id, "path": path, "exists": path.is_dir()}))
        .collect();
    json_reply(200, &json!({"roots": roots}))
}

fn modified(meta: &std::fs::Metadata) -> Option<String> {
    let time = meta.modified().ok()?;
    jiff::Timestamp::try_from(time).ok().map(|t| t.to_string())
}

/// `GET files/list?root=&path=`.
pub(crate) fn list(shared: &Shared, at: &Profile<'_>, req: &Req) -> Reply {
    let found = match resolve(shared, at, req, &path_param(req), Access::Read) {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    let (root, dir) = (&found.root, &found.path);
    let Ok(read) = std::fs::read_dir(dir) else {
        return error(400, "not_a_directory", "the path is not a directory");
    };
    let mut entries: Vec<Value> = read
        .flatten()
        .filter_map(|entry| {
            let target = entry.path().canonicalize().ok()?;
            // A link leading out of the root, or an entry the root's
            // rules refuse (a credential store), is not listed at all.
            target.starts_with(root).then_some(())?;
            found.check(&target, Access::Read).ok()?;
            let meta = std::fs::metadata(&target).ok()?;
            Some(json!({
                "name": entry.file_name().to_string_lossy(),
                "kind": if meta.is_dir() { "dir" } else { "file" },
                "size": meta.len(),
                "modified": modified(&meta),
            }))
        })
        .collect();
    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let shown = dir.strip_prefix(root).unwrap_or(Path::new(""));
    json_reply(200, &json!({"path": shown, "entries": entries}))
}

fn image_type(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// `GET files/read?root=&path=[&download=1]`: a text or image preview,
/// or the file as an attachment.
pub(crate) fn read(shared: &Shared, at: &Profile<'_>, req: &Req) -> Reply {
    let file = match resolve(shared, at, req, &path_param(req), Access::Read) {
        Ok(found) => found.path,
        Err(reply) => return reply,
    };
    let Ok(meta) = std::fs::metadata(&file) else {
        return error(404, "not_found", "no such file");
    };
    if !meta.is_file() {
        return error(400, "not_a_file", "the path is not a file");
    }
    let download = req.param("download").is_some();
    if !download && image_type(&file).is_none() && meta.len() > MAX_PREVIEW {
        return error(
            413,
            "too_large",
            "previews are limited to 2 MiB; download it instead",
        );
    }
    let Ok(data) = std::fs::read(&file) else {
        return error(500, "io", "the file cannot be read");
    };
    if download {
        let mut reply = bytes(200, "application/octet-stream", data);
        reply.headers_mut().insert(
            hyper::header::CONTENT_DISPOSITION,
            hyper::header::HeaderValue::from_static("attachment"),
        );
        return reply;
    }
    if let Some(kind) = image_type(&file) {
        return bytes(200, kind, data);
    }
    match String::from_utf8(data) {
        Ok(text) => bytes(200, "text/plain; charset=utf-8", text),
        Err(_) => error(415, "binary", "not a text file; download it instead"),
    }
}

/// A new name inside a directory: one plain path component.
fn plain_name(name: &str) -> Option<&str> {
    let mut parts = Path::new(name).components();
    match (parts.next(), parts.next()) {
        (Some(Component::Normal(_)), None) => Some(name),
        _ => None,
    }
}

/// `POST files/upload?root=&path=<dir>&name=<file>`, the file as the body.
pub(crate) async fn upload(shared: &Shared, at: &Profile<'_>, req: &Req, body: Incoming) -> Reply {
    let dir = match resolve(shared, at, req, &path_param(req), Access::Write) {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    let Some(name) = req.param("name").filter(|n| plain_name(n).is_some()) else {
        return error(400, "invalid_name", "expected name=<file name>");
    };
    let max = shared.config.max_upload_bytes;
    let too_large = || {
        error(
            413,
            "too_large",
            &format!("uploads are limited to {max} bytes"),
        )
    };
    let declared = req
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|len| len > max) {
        return too_large();
    }
    let Some(data) = read_body(body, usize::try_from(max).unwrap_or(usize::MAX)).await else {
        return too_large();
    };
    let target = dir.path.join(&name);
    if std::fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
        return forbidden();
    }
    if let Err(reply) = dir.check(&target, Access::Write) {
        return reply;
    }
    match std::fs::write(&target, &data) {
        Ok(()) => json_reply(201, &json!({"name": name, "bytes": data.len()})),
        Err(e) => error(500, "io", &e.to_string()),
    }
}

/// `POST files/rename?root= {"path": "<from>", "to": "<new name>"}`.
pub(crate) async fn rename(shared: &Shared, at: &Profile<'_>, req: &Req, body: Incoming) -> Reply {
    let body = match read_json(body).await {
        Ok(body) => body,
        Err(reply) => return reply,
    };
    let (Some(from), Some(to)) = (
        body["path"].as_str(),
        body["to"].as_str().and_then(plain_name),
    ) else {
        return error(
            400,
            "invalid_body",
            "expected {\"path\": \"<file>\", \"to\": \"<new name>\"}",
        );
    };
    let found = match resolve(shared, at, req, from, Access::Write) {
        Ok(found) => found,
        Err(reply) => return reply,
    };
    let source = &found.path;
    if *source == found.root {
        return forbidden();
    }
    let target = source.with_file_name(to);
    if let Err(reply) = found.check(&target, Access::Write) {
        return reply;
    }
    if target.exists() {
        return error(409, "exists", &format!("{to} already exists"));
    }
    match std::fs::rename(source, &target) {
        Ok(()) => json_reply(200, &json!({"renamed": from, "to": to})),
        Err(e) => error(500, "io", &e.to_string()),
    }
}

/// `DELETE files?root=&path=&confirm=true`.
pub(crate) fn delete(shared: &Shared, at: &Profile<'_>, req: &Req) -> Reply {
    if req.param("confirm").as_deref() != Some("true") {
        return error(409, "confirm", "deleting needs confirm=true");
    }
    let (root, target) = match resolve(shared, at, req, &path_param(req), Access::Write) {
        Ok(found) => (found.root, found.path),
        Err(reply) => return reply,
    };
    if target == root {
        return forbidden();
    }
    let removed = if target.is_dir() {
        std::fs::remove_dir_all(&target)
    } else {
        std::fs::remove_file(&target)
    };
    match removed {
        Ok(()) => json_reply(200, &json!({"deleted": path_param(req)})),
        Err(e) => error(500, "io", &e.to_string()),
    }
}
