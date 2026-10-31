//! The file tools as registry entries: descriptors, schemas and handlers
//! that run each call on the calling session's [`FileSession`].

use std::sync::Arc;

use serde_json::{Value, json};

use super::FileSession;
use super::patch::PatchArgs;
use super::read::{DEFAULT_LIMIT, ReadArgs};
use super::search::{MAX_CONTEXT, MAX_LIMIT, SearchArgs};
use crate::descriptor::{
    CallContext, Effect, HandlerFuture, Owner, PathClaim, RegisteredTool, ToolDescriptor,
    ToolFailure, ToolHandler,
};
use crate::environments::{Environments, part};
use crate::result::ToolResult;

/// The toolset of every file tool.
pub const TOOLSET: &str = "file";

type Operation = fn(&FileSession, &Value) -> ToolResult;

/// Runs one file operation off the async runtime (file I/O blocks).
struct FileHandler {
    environments: Arc<Environments>,
    operation: Operation,
}

impl ToolHandler for FileHandler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let files = part(&self.environments, context, "file", |env| env.files.clone())?;
            let operation = self.operation;
            tokio::task::spawn_blocking(move || operation(&files, &arguments))
                .await
                .map_err(|err| ToolFailure::new("panic", err.to_string()))
        })
    }
}

/// `read_file`, `write_file`, `patch` and `search_files`.
#[must_use]
pub fn tools(environments: &Arc<Environments>) -> Vec<RegisteredTool> {
    let entry = |descriptor: ToolDescriptor, operation: Operation| {
        let handler = Arc::new(FileHandler {
            environments: Arc::clone(environments),
            operation,
        });
        RegisteredTool::new(descriptor, Owner::Builtin, handler)
    };
    vec![
        entry(read_file(), |files, args| files.read_file(&read_args(args))),
        entry(write_file(), |files, args| {
            files.write_file(text(args, "path"), text(args, "content"))
        }),
        entry(patch(), |files, args| match PatchArgs::from_value(args) {
            Ok(patch) => files.patch(&patch),
            Err(why) => ToolResult::error(why),
        }),
        entry(search_files(), |files, args| {
            files.search_files(&SearchArgs::from_value(args))
        }),
    ]
}

fn text<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn read_args(args: &Value) -> ReadArgs {
    let number = |key: &str, default: usize| {
        args.get(key)
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(default)
    };
    ReadArgs {
        path: text(args, "path").to_owned(),
        offset: number("offset", 1).max(1),
        limit: number("limit", DEFAULT_LIMIT).max(1),
    }
}

fn descriptor(name: &str, description: &str, parameters: Value, effect: Effect) -> ToolDescriptor {
    let mut descriptor = ToolDescriptor::new(name, TOOLSET, description, parameters);
    descriptor.effect = effect;
    descriptor.idempotent = effect == Effect::None;
    descriptor
}

fn read_file() -> ToolDescriptor {
    let mut d = descriptor(
        "read_file",
        "Read a text file with line numbers (`LINE|text`), a page at a time. \
         Use offset and limit to page through long files; the result gives next_offset \
         when more lines remain. Binary files, device files and secret files such as \
         .env are refused. Re-reading an unchanged region returns a short stub.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to the working directory."},
                "offset": {"type": "integer", "minimum": 1, "description": "First line to read (1-based, default 1)."},
                "limit": {"type": "integer", "minimum": 1, "description": format!("Most lines to return (default {DEFAULT_LIMIT}).")}
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        Effect::None,
    );
    d.claims.reads = vec![PathClaim::Argument("/path".into())];
    d
}

fn write_file() -> ToolDescriptor {
    let mut d = descriptor(
        "write_file",
        "Create a file or replace its whole content (parent directories are created). \
         An existing file must have been read in full in this session and be unchanged \
         since; otherwise the write is refused with stale_write_blocked. The result \
         reports a syntax check of the new content as lint.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, absolute or relative to the working directory."},
                "content": {"type": "string", "description": "The complete new content."}
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
        Effect::Workspace,
    );
    d.claims.writes = vec![PathClaim::Argument("/path".into())];
    d
}

fn patch() -> ToolDescriptor {
    let mut d = descriptor(
        "patch",
        "Edit files. mode=replace (default): replace old_string with new_string in path; \
         old_string must match exactly one location unless replace_all=true (whitespace \
         and quote differences are tolerated). mode=v4a: apply a multi-file patch \
         ('*** Begin Patch' ... '*** End Patch' with Add File, Update File (optionally \
         Move to) and Delete File sections); all files change or none. The result has a \
         unified diff and a syntax check (lint).",
        json!({
            "type": "object",
            "properties": {
                "mode": {"type": "string", "enum": ["replace", "v4a"], "description": "replace (default) or v4a."},
                "path": {"type": "string", "description": "replace: the file to edit."},
                "old_string": {"type": "string", "description": "replace: the text to find."},
                "new_string": {"type": "string", "description": "replace: its replacement."},
                "replace_all": {"type": "boolean", "description": "replace: replace every match (default false)."},
                "patch": {"type": "string", "description": "v4a: the patch text."}
            },
            "additionalProperties": false
        }),
        Effect::Workspace,
    );
    d.claims.writes = vec![
        PathClaim::Argument("/path".into()),
        PathClaim::PatchText("/patch".into()),
    ];
    d
}

fn search_files() -> ToolDescriptor {
    let mut d = descriptor(
        "search_files",
        "Search file contents with a regex (target=content, default) or find files whose \
         names match a glob (target=files). Respects .gitignore and skips binary files. \
         Content matches come back as path:line:text; output_mode=files_only lists \
         matching files and output_mode=count gives per-file counts. Page with offset \
         when truncated is true.",
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex for content, glob for file names."},
                "target": {"type": "string", "enum": ["content", "files"]},
                "path": {"type": "string", "description": "Directory to search (default: the working directory)."},
                "file_glob": {"type": "string", "description": "Only search files matching this glob, e.g. *.rs."},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT},
                "offset": {"type": "integer", "minimum": 0},
                "order": {"type": "string", "enum": ["discovery", "modified"]},
                "output_mode": {"type": "string", "enum": ["content", "files_only", "count"]},
                "context": {"type": "integer", "minimum": 0, "maximum": MAX_CONTEXT}
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
        Effect::None,
    );
    d.claims.reads = vec![PathClaim::Argument("/path".into())];
    d
}
