//! `prometheus debug share` and `debug delete` (spec: observability,
//! Opt-in debug share upload; No outbound telemetry by default): a report
//! of `debug summary` plus the last log records of the daemon and the
//! active profile, redacted like trajectories, shown in full (or by path
//! when longer than a screen) and uploaded only after an explicit yes.
//! Without a terminal and without `--yes` nothing is uploaded (exit 4); a
//! declined upload makes no connection (exit 1). `--local` writes the same
//! report to a file without any network access. Uploads are recorded in
//! `<home>/logs/shares.json` so `debug delete <url>` can remove them where
//! the target supports deletion.

use std::fmt::Write as _;
use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use crate::context::Context;

/// Lines of report printed in full; a longer report is shown by path.
const SCREEN_LINES: usize = 40;
/// One upload or deletion request's deadline.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Where a report goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// `https://paste.rs` (no expiry; deletable).
    #[value(name = "paste_rs")]
    PasteRs,
    /// `https://dpaste.com` (expires after `--expire-days`; not deletable).
    Dpaste,
    /// `debug.share.endpoint`, a private support endpoint.
    Custom,
}

/// `prometheus debug share` arguments.
#[derive(Debug, Clone, clap::Args)]
#[allow(clippy::struct_excessive_bools)] // independent command-line flags
pub struct ShareArgs {
    /// Log records of the daemon and of the profile to include, each.
    #[arg(long, value_name = "N", default_value_t = 200)]
    pub lines: usize,
    /// Days until the paste expires, where the target supports expiry.
    #[arg(long, value_name = "D", default_value_t = 7)]
    pub expire_days: u32,
    /// Where to upload.
    #[arg(long, value_enum, default_value_t = Target::PasteRs)]
    pub target: Target,
    /// Upload without redaction (asks a second time; never unattended).
    #[arg(long)]
    pub no_redact: bool,
    /// Upload without asking.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Write the report to a file instead (no network access at all).
    #[arg(long)]
    pub local: bool,
}

/// One recorded upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    /// Its public URL.
    pub url: String,
    /// The target.
    pub target: Target,
    /// When (RFC 3339).
    pub created_at: String,
    /// Days until it expires, where the target expires pastes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expire_days: Option<u32>,
    /// The target's deletion token, when it gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_token: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Shares {
    shares: Vec<Share>,
}

/// Runs `prometheus debug share`.
///
/// # Errors
///
/// `debug.io` writing the local report, `debug.share_invalid` without a
/// custom endpoint, `debug.share_failed` when the upload fails.
pub fn run(args: &ShareArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let report = report(ctx, &scope, args.lines, !args.no_redact)?;
    if args.local {
        let path = write_report(&scope, &report)?;
        return crate::output::print(&format!(
            "report written to {} (nothing was uploaded)",
            path.display()
        ));
    }
    let host = target_host(ctx, &scope, args.target)?;
    let interactive = std::io::stdin().is_terminal();
    if !interactive && (!args.yes || args.no_redact) {
        eprintln!(
            "debug share asks before uploading: run it on a terminal or pass --yes; nothing was uploaded"
        );
        return Ok(pmt_core::exit::APPROVAL_REQUIRED);
    }
    show(&scope, &report)?;
    let question =
        format!("Upload this report to {host}? It will be public to anyone with the link. [y/N] ");
    if !(args.yes || confirm(&question)) {
        eprintln!("nothing was uploaded");
        return Ok(pmt_core::exit::FAILURE);
    }
    if args.no_redact
        && !confirm("The report is NOT redacted and may contain secrets. Upload it anyway? [y/N] ")
    {
        eprintln!("nothing was uploaded");
        return Ok(pmt_core::exit::FAILURE);
    }
    let share = crate::model_cmd::block_on(upload(ctx, &scope, args, &report))??;
    record(scope.home(), &share)?;
    if ctx.json {
        return crate::output::document("debug share", &share);
    }
    crate::output::print(&format!(
        "uploaded: {}\ndelete it with: prometheus debug delete {}",
        share.url, share.url
    ))
}

/// The report: the setup summary and the last `lines` records of the
/// daemon's and the profile's logs, redacted unless `redact` is false.
fn report(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    lines: usize,
    redact: bool,
) -> Result<String, Error> {
    let mut text = super::summary(ctx, scope, false);
    let logs = [
        ("daemon", scope.home().join("logs").join("gateway.log")),
        ("process", scope.home().join("logs").join("prometheus.log")),
        ("profile", scope.path("logs").join("prometheus.log")),
    ];
    for (label, path) in logs {
        let records = tail(&path, lines);
        if records.is_empty() {
            continue;
        }
        let _ = writeln!(
            text,
            "\n--- last {} {label} log records ({}) ---",
            records.len(),
            path.display()
        );
        for record in records {
            text.push_str(&record);
            text.push('\n');
        }
    }
    if !redact {
        return Ok(text);
    }
    let secrets = scope.secrets().redactor()?;
    let redactor = match pmt_obs::redaction::profile_redactor(scope)? {
        Some(profile) => secrets.merged(&profile),
        None => secrets,
    };
    Ok(super::redact_url_credentials(&redactor.redact_all(&text)))
}

/// The last `n` lines of `path`.
fn tail(path: &Path, n: usize) -> Vec<String> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let lines: Vec<String> = std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

fn write_report(scope: &ProfileScope, report: &str) -> Result<PathBuf, Error> {
    let dir = scope.home().join("logs");
    std::fs::create_dir_all(&dir).map_err(|e| Error::io(&codes::DEBUG_IO, &dir, &e))?;
    let stamp = jiff::Timestamp::now()
        .strftime("%Y%m%dT%H%M%SZ")
        .to_string();
    let path = dir.join(format!("debug-report-{stamp}.txt"));
    pmt_core::private_fs::write_atomic(&path, report.as_bytes())
        .map_err(|e| Error::io(&codes::DEBUG_IO, &path, &e))?;
    Ok(path)
}

/// Prints the report, or its path when it is longer than a screen.
fn show(scope: &ProfileScope, report: &str) -> Result<(), Error> {
    if report.lines().count() <= SCREEN_LINES {
        println!("{report}");
    } else {
        let path = write_report(scope, report)?;
        println!(
            "the report is {} lines long; read it at {}",
            report.lines().count(),
            path.display()
        );
    }
    Ok(())
}

/// Asks `question` on stderr; only `y`/`yes` agrees.
fn confirm(question: &str) -> bool {
    eprint!("{question}");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// The host a report goes to.
fn target_host(ctx: &Context<'_>, scope: &ProfileScope, target: Target) -> Result<String, Error> {
    let url = target_url(ctx, scope, target)?;
    Ok(reqwest::Url::parse(&url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or(url))
}

fn target_url(ctx: &Context<'_>, scope: &ProfileScope, target: Target) -> Result<String, Error> {
    match target {
        Target::PasteRs => Ok(ctx
            .launch
            .test_paste_url()
            .unwrap_or("https://paste.rs/")
            .to_owned()),
        Target::Dpaste => Ok("https://dpaste.com/api/v2/".to_owned()),
        Target::Custom => custom_endpoint(scope),
    }
}

/// `debug.share.endpoint`: `https://`, or `http://` to a loopback host.
fn custom_endpoint(scope: &ProfileScope) -> Result<String, Error> {
    let invalid = |why: &str| Error::new(&codes::DEBUG_SHARE_INVALID, why.to_owned());
    let endpoint = scope
        .config()
        .debug
        .share
        .endpoint
        .clone()
        .ok_or_else(|| invalid("--target custom needs debug.share.endpoint"))?;
    let url = reqwest::Url::parse(&endpoint)
        .map_err(|e| invalid(&format!("debug.share.endpoint: {e}")))?;
    let loopback = url.host_str().is_some_and(|h| {
        h == "localhost"
            || h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    match url.scheme() {
        "https" => Ok(endpoint),
        "http" if loopback => Ok(endpoint),
        _ => Err(invalid(
            "debug.share.endpoint must be https:// (or http:// to a loopback host)",
        )),
    }
}

fn client(scope: &ProfileScope) -> Result<reqwest::Client, Error> {
    let network = &scope.config().network;
    let policy = pmt_net::OutboundPolicy {
        force_ipv4: network.force_ipv4,
        extra_ca_file: network.extra_ca_file.as_ref().map(Into::into),
        proxy: network.proxy.clone(),
    };
    pmt_net::client_builder(&policy)?
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| Error::new(&codes::NET_CLIENT_INIT, e.to_string()))
}

/// The custom endpoint's headers, from `debug.share.header_secrets`.
fn custom_headers(scope: &ProfileScope) -> Result<Vec<(String, String)>, Error> {
    scope
        .config()
        .debug
        .share
        .header_secrets
        .iter()
        .map(|name| {
            Ok((
                pmt_obs::otel::header_name(name),
                scope.secrets().resolve(name)?.expose().to_owned(),
            ))
        })
        .collect()
}

/// Uploads `report`; async because `/debug share yes` runs inside the
/// session's runtime, where blocking on a second runtime panics.
async fn upload(
    ctx: &Context<'_>,
    scope: &ProfileScope,
    args: &ShareArgs,
    report: &str,
) -> Result<Share, Error> {
    let url = target_url(ctx, scope, args.target)?;
    let headers = if args.target == Target::Custom {
        custom_headers(scope)?
    } else {
        Vec::new()
    };
    let client = client(scope)?;
    let request = match args.target {
        Target::PasteRs => client.post(&url).body(report.to_owned()),
        Target::Dpaste => client
            .post(&url)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(
                url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("content", report)
                    .append_pair("expiry_days", &args.expire_days.to_string())
                    .finish(),
            ),
        Target::Custom => client
            .post(&url)
            .header("content-type", "application/json")
            .body(
                serde_json::json!({"content": report, "expire_days": args.expire_days}).to_string(),
            ),
    };
    let request = headers
        .iter()
        .fold(request, |r, (k, v)| r.header(k.as_str(), v.as_str()));
    let answer = send(request).await.map_err(|why| {
        Error::new(
            &codes::DEBUG_SHARE_FAILED,
            format!("upload to {url} failed: {why}"),
        )
    })?;
    let (url, delete_token) = parse_answer(&answer.0, answer.1.as_deref()).ok_or_else(|| {
        Error::new(
            &codes::DEBUG_SHARE_FAILED,
            "the target answered without a URL",
        )
    })?;
    Ok(Share {
        url,
        target: args.target,
        created_at: pmt_obs::trajectory::timestamp(jiff::Timestamp::now()),
        expire_days: (args.target != Target::PasteRs).then_some(args.expire_days),
        delete_token,
    })
}

/// Sends an upload: the answer's body and `Location` header on success.
async fn send(request: reqwest::RequestBuilder) -> Result<(String, Option<String>), String> {
    let response = request
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let status = response.status();
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = response.text().await.map_err(|e| e.to_string())?;
    if status.is_success() {
        Ok((body, location))
    } else {
        Err(format!("HTTP {}", status.as_u16()))
    }
}

/// The paste URL (and deletion token) from a target's answer: JSON
/// `{"url", "delete_token"}`, a bare URL body, or a `Location` header.
fn parse_answer(body: &str, location: Option<&str>) -> Option<(String, Option<String>)> {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(url) = json["url"].as_str()
    {
        return Some((
            url.to_owned(),
            json["delete_token"].as_str().map(str::to_owned),
        ));
    }
    let text = body.trim().trim_matches('"');
    if text.starts_with("http://") || text.starts_with("https://") {
        return Some((text.to_owned(), None));
    }
    location.map(|l| (l.to_owned(), None))
}

/// Reports `/debug share` showed per session, waiting for `/debug share
/// yes`.
fn pending() -> &'static std::sync::Mutex<std::collections::HashMap<String, PathBuf>> {
    static PENDING: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, PathBuf>>,
    > = std::sync::OnceLock::new();
    PENDING.get_or_init(std::sync::Mutex::default)
}

/// `/debug [share [yes]]` in a session: `/debug` writes the redacted
/// report locally; `/debug share` writes it, shows it and asks; only
/// `/debug share yes` then uploads that same report to paste.rs.
///
/// # Errors
///
/// As `debug share`.
pub(crate) async fn slash(ctx: &Context<'_>, session: &str, args: &str) -> Result<String, Error> {
    let scope = ctx.scope()?;
    let words: Vec<&str> = args.split_whitespace().collect();
    let answer = |path: &Path, report: &str| {
        if report.lines().count() <= SCREEN_LINES {
            format!("{report}\n(written to {})", path.display())
        } else {
            format!(
                "the report is {} lines long; read it at {}",
                report.lines().count(),
                path.display()
            )
        }
    };
    match words.as_slice() {
        [] => {
            let report = report(ctx, &scope, 200, true)?;
            let path = write_report(&scope, &report)?;
            Ok(format!("{}\nnothing was uploaded", answer(&path, &report)))
        }
        ["share"] => {
            let report = report(ctx, &scope, 200, true)?;
            let path = write_report(&scope, &report)?;
            pending()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(session.to_owned(), path.clone());
            let host = target_host(ctx, &scope, Target::PasteRs)?;
            Ok(format!(
                "{}\nUpload this report to {host}? It will be public to anyone with the link.\n\
                 Send /debug share yes to upload; nothing is uploaded otherwise.",
                answer(&path, &report)
            ))
        }
        ["share", "yes" | "y"] => {
            let path = pending()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(session)
                .ok_or_else(|| {
                    Error::new(
                        &codes::DEBUG_SHARE_INVALID,
                        "no report waits for an answer: send /debug share first",
                    )
                })?;
            let report = std::fs::read_to_string(&path)
                .map_err(|e| Error::io(&codes::DEBUG_IO, &path, &e))?;
            let args = ShareArgs {
                lines: 200,
                expire_days: 7,
                target: Target::PasteRs,
                no_redact: false,
                yes: true,
                local: false,
            };
            let share = upload(ctx, &scope, &args, &report).await?;
            record(scope.home(), &share)?;
            Ok(format!(
                "uploaded: {} (delete it with: prometheus debug delete {})",
                share.url, share.url
            ))
        }
        _ => Err(Error::new(
            &codes::CLI_USAGE,
            "usage: /debug, /debug share, /debug share yes",
        )),
    }
}

fn shares_path(home: &Path) -> PathBuf {
    home.join("logs").join("shares.json")
}

fn load(home: &Path) -> Shares {
    std::fs::read(shares_path(home))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(home: &Path, shares: &Shares) -> Result<(), Error> {
    let path = shares_path(home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(&codes::DEBUG_IO, dir, &e))?;
    }
    let json = serde_json::to_vec_pretty(shares).unwrap_or_default();
    pmt_core::private_fs::write_atomic(&path, &json)
        .map_err(|e| Error::io(&codes::DEBUG_IO, &path, &e))
}

fn record(home: &Path, share: &Share) -> Result<(), Error> {
    let mut shares = load(home);
    shares.shares.push(share.clone());
    save(home, &shares)
}

/// Runs `prometheus debug delete <url>`.
///
/// # Errors
///
/// `debug.share_invalid` for an unrecorded URL or a target without
/// deletion, `debug.share_failed` when the target refuses.
pub fn delete(url: &str, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let mut shares = load(scope.home());
    let Some(index) = shares.shares.iter().position(|s| s.url == url) else {
        return Err(Error::new(
            &codes::DEBUG_SHARE_INVALID,
            format!("{url} is not a recorded share (logs/shares.json)"),
        ));
    };
    let share = shares.shares[index].clone();
    let mut request = match share.target {
        Target::Dpaste => {
            return Err(Error::new(
                &codes::DEBUG_SHARE_INVALID,
                "dpaste.com cannot delete a paste; it expires on its own",
            ));
        }
        Target::PasteRs => client(&scope)?.delete(&share.url),
        Target::Custom => {
            let headers = custom_headers(&scope)?;
            let request = client(&scope)?.delete(&share.url);
            headers
                .iter()
                .fold(request, |r, (k, v)| r.header(k.as_str(), v.as_str()))
        }
    };
    if let Some(token) = &share.delete_token {
        request = request.header("x-delete-token", token.as_str());
    }
    crate::model_cmd::block_on(async move {
        let response = request
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(format!("HTTP {}", response.status().as_u16()))
        }
    })?
    .map_err(|why| {
        Error::new(
            &codes::DEBUG_SHARE_FAILED,
            format!("deleting {url} failed: {why}"),
        )
    })?;
    shares.shares.remove(index);
    save(scope.home(), &shares)?;
    crate::output::print(&format!("deleted {url}"))
}

#[cfg(test)]
mod tests {
    use super::parse_answer;

    #[test]
    fn targets_answer_with_a_url_in_json_text_or_location() {
        assert_eq!(
            parse_answer(
                r#"{"url":"https://s.example/p/1","delete_token":"t"}"#,
                None
            ),
            Some(("https://s.example/p/1".to_owned(), Some("t".to_owned())))
        );
        assert_eq!(
            parse_answer("https://paste.rs/abc\n", None),
            Some(("https://paste.rs/abc".to_owned(), None))
        );
        assert_eq!(
            parse_answer("", Some("https://dpaste.com/XYZ")),
            Some(("https://dpaste.com/XYZ".to_owned(), None))
        );
        assert_eq!(parse_answer("ok", None), None);
    }
}
