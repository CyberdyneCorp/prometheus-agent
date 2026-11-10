//! `prometheus debug bundle [--session ID] [--since 24h] [-o PATH]` (spec:
//! observability, Crash and debug bundle): a local `tar.zst` for a bug
//! report with the window's log records, the configuration, `doctor
//! --json`, `version --json`, crash reports and (with `--session`) that
//! session's trajectory. Every member is redacted with the profile's known
//! secrets and credential patterns, and URL credentials become `***`,
//! whatever `security.redact_secrets` says; nothing is uploaded (that is
//! `prometheus debug share`, which asks first).

use std::io::Read as _;
use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::secret::Redactor;
use serde::Serialize;

use crate::context::Context;

/// `prometheus debug bundle` arguments.
#[derive(Debug, Clone, clap::Args)]
pub struct BundleArgs {
    /// Include this session's trajectory (id, id prefix or title).
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,
    /// Log records since `<N>[smhd]` ago or an RFC 3339 time.
    #[arg(long, value_name = "S", default_value = "24h")]
    pub since: String,
    /// Where to write the bundle (default:
    /// `./prometheus-debug-<UTC>.tar.zst`).
    #[arg(short = 'o', long = "output", value_name = "PATH")]
    pub output: Option<PathBuf>,
}

/// What `debug bundle --json` prints.
#[derive(Debug, Serialize)]
struct Written {
    path: PathBuf,
    members: Vec<String>,
}

/// One bundle member.
struct Member {
    name: String,
    text: String,
}

/// Writes the bundle and prints its path.
///
/// # Errors
///
/// `logs.invalid_since` (2), session resolution errors, `debug.io` when
/// the bundle cannot be written.
pub fn run(args: &BundleArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let since = pmt_obs::logs::view::parse_since(&args.since, jiff::Timestamp::now())?;
    let redactor = redactor(&scope)?;
    let mut members = Vec::new();
    members.extend(logs(&scope, since));
    members.extend(configs(&scope));
    members.push(Member {
        name: "doctor.json".to_owned(),
        text: own_output(ctx, &["--json", "doctor"]),
    });
    members.push(Member {
        name: "version.json".to_owned(),
        text: own_output(ctx, &["--json", "version"]),
    });
    members.extend(crash_reports(scope.home()));
    if let Some(session) = &args.session {
        members.push(trajectory(&scope, session)?);
    }
    let path = args.output.clone().unwrap_or_else(default_path);
    let names = write(&path, members, &redactor)?;
    if ctx.json {
        return crate::output::document(
            "debug bundle",
            &Written {
                path,
                members: names,
            },
        );
    }
    crate::output::print(&format!(
        "debug bundle written to {}\nnothing was uploaded; share it yourself or with 'prometheus debug share'",
        path.display()
    ))
}

/// The bundle's redactor: the profile's known secrets (always, whatever
/// `security.redact_secrets` says) and its PII rules.
fn redactor(scope: &ProfileScope) -> Result<Redactor, Error> {
    let secrets = scope.secrets().redactor()?;
    Ok(match pmt_obs::redaction::profile_redactor(scope)? {
        Some(profile) => secrets.merged(&profile),
        None => secrets,
    })
}

fn default_path() -> PathBuf {
    let stamp = jiff::Timestamp::now()
        .strftime("%Y%m%dT%H%M%SZ")
        .to_string();
    PathBuf::from(format!("prometheus-debug-{stamp}.tar.zst"))
}

/// Records of every log file of the home and the profile at or after
/// `since` (lines that are not records are kept: they cannot be dated).
fn logs(scope: &ProfileScope, since: jiff::Timestamp) -> Vec<Member> {
    let dirs = [
        ("logs/home", scope.home().join("logs")),
        ("logs/profile", scope.path("logs")),
    ];
    let mut members = Vec::new();
    for (prefix, dir) in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.file_name()
                        .is_some_and(|n| n.to_string_lossy().contains(".log"))
            })
            .collect();
        files.sort();
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let kept: String = text
                .lines()
                .filter(|line| recent(line, since))
                .flat_map(|line| [line, "\n"])
                .collect();
            let name = file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            members.push(Member {
                name: format!("{prefix}/{name}"),
                text: kept,
            });
        }
    }
    members
}

fn recent(line: &str, since: jiff::Timestamp) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|record| record["ts"].as_str()?.parse::<jiff::Timestamp>().ok())
        .is_none_or(|ts| ts >= since)
}

/// The profile's `config.toml` and the home's `daemon.toml` (secret
/// references are names; values never live there, and anything else
/// secret-shaped is redacted with the rest).
fn configs(scope: &ProfileScope) -> Vec<Member> {
    [
        ("config/config.toml", scope.path("config.toml")),
        ("config/daemon.toml", scope.home().join("daemon.toml")),
    ]
    .into_iter()
    .filter_map(|(name, path)| {
        Some(Member {
            name: name.to_owned(),
            text: std::fs::read_to_string(path).ok()?,
        })
    })
    .collect()
}

/// What this binary prints for `args` on the same home and profile.
fn own_output(ctx: &Context<'_>, args: &[&str]) -> String {
    let Ok(binary) = std::env::current_exe() else {
        return "{\"error\": \"this binary's path is unknown\"}\n".to_owned();
    };
    let mut command = std::process::Command::new(binary);
    if let Ok(home) = ctx.home() {
        command.arg("--home").arg(home.root());
    }
    if let Some(profile) = ctx.profile {
        command.args(["--profile", profile]);
    }
    match command
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
    {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            text
        }
        Err(err) => format!("{{\"error\": \"{err}\"}}\n"),
    }
}

fn crash_reports(home: &Path) -> Vec<Member> {
    let Ok(entries) = std::fs::read_dir(home.join("logs").join("crash")) else {
        return Vec::new();
    };
    let mut members: Vec<Member> = entries
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path()).ok()?;
            Some(Member {
                name: format!("crash/{}", e.file_name().to_string_lossy()),
                text,
            })
        })
        .collect();
    members.sort_by(|a, b| a.name.cmp(&b.name));
    members
}

fn trajectory(scope: &ProfileScope, session: &str) -> Result<Member, Error> {
    let id = match pmt_core::id::SessionId::parse(session) {
        Ok(id) => id,
        Err(_) => pmt_store::Store::open(scope)?.resolve_session(session)?,
    };
    let dir = scope.path(pmt_obs::trajectory::files::DIR);
    let path = pmt_obs::trajectory::files::find(&dir, &id.to_string()).ok_or_else(|| {
        Error::new(
            &codes::TRAJECTORY_NOT_FOUND,
            format!("session {id} has no trajectory in {}", dir.display()),
        )
    })?;
    let mut text = String::new();
    pmt_obs::trajectory::files::open(&path)
        .and_then(|mut reader| reader.read_to_string(&mut text))
        .map_err(|e| Error::io(&codes::TRAJECTORY_IO, &path, &e))?;
    Ok(Member {
        name: format!("trajectory/{id}.jsonl"),
        text,
    })
}

/// Writes `members`, each redacted, plus `manifest.json`; their names.
fn write(path: &Path, members: Vec<Member>, redactor: &Redactor) -> Result<Vec<String>, Error> {
    let failed = |e: std::io::Error| Error::io(&codes::DEBUG_IO, path, &e);
    let file = pmt_core::private_fs::new_file_options()
        .open(path)
        .map_err(failed)?;
    let encoder = zstd::Encoder::new(file, 9).map_err(failed)?;
    let mut tar = tar::Builder::new(encoder);
    let mut names = Vec::new();
    for member in members {
        let text = super::redact_url_credentials(&redactor.redact_all(&member.text));
        append(&mut tar, &member.name, text.as_bytes()).map_err(failed)?;
        names.push(member.name);
    }
    let manifest = serde_json::json!({
        "created_at": pmt_obs::trajectory::timestamp(jiff::Timestamp::now()),
        "version": crate::build_info::VERSION,
        "members": names,
    });
    append(&mut tar, "manifest.json", manifest.to_string().as_bytes()).map_err(failed)?;
    tar.into_inner()
        .and_then(zstd::Encoder::finish)
        .map_err(failed)?;
    Ok(names)
}

fn append<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    name: &str,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o600);
    header.set_mtime(u64::try_from(jiff::Timestamp::now().as_second()).unwrap_or(0));
    header.set_cksum();
    tar.append_data(&mut header, name, bytes)
}
