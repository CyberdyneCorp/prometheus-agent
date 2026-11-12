//! `prometheus skills tap|publish|snapshot|sign` (spec: skills, Custom
//! skill taps; Skill publishing and snapshots; Skill signatures and
//! provenance).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use pmt_skillhub::keyless::{self, Services};
use pmt_skillhub::publish::{self, Target};
use pmt_skillhub::sign::{self, SIGNATURE_FILE, SignatureFile};
use pmt_skillhub::{snapshot, taps};
use pmt_skills::lint::LintContext;
use serde_json::json;

use super::Env;
use super::args::{PublishTo, SnapshotCommand, TapCommand};
use super::hub::hub;
use crate::context::Context;
use crate::output::{json, print_raw};

/// `skills tap list|add|remove`.
pub(super) fn tap(env: &Env, command: &TapCommand) -> Result<u8, Error> {
    let dir = env.scope.dir();
    match command {
        TapCommand::List => {
            let list = taps::read(dir)?;
            if env.json {
                return json(&json!({"taps": list}));
            }
            let mut text = String::new();
            for tap in &list {
                let _ = writeln!(text, "{}  (path {})", tap.repo, tap.path);
            }
            if list.is_empty() {
                text.push_str("no taps\n");
            }
            print_raw(&text)
        }
        TapCommand::Add { repo, path } => {
            let new = taps::add(dir, repo, path.as_deref())?;
            let trust = pmt_skillhub::sources::trust_text(hub(env)?.repo_trust(repo));
            if env.json {
                return json(&json!({"repo": repo, "added": new, "trust": trust}));
            }
            eprintln!(
                "tap {repo} {} (trust {trust})",
                if new { "added" } else { "updated" }
            );
            Ok(0)
        }
        TapCommand::Remove { repo } => {
            taps::remove(dir, repo)?;
            if env.json {
                return json(&json!({"repo": repo, "removed": true}));
            }
            eprintln!("tap {repo} removed");
            Ok(0)
        }
    }
}

/// `skills publish <dir> --to github|clawhub [--repo owner/repo]`.
pub(super) fn publish(
    env: &Env,
    dir: &Path,
    to: PublishTo,
    repo: Option<&str>,
) -> Result<u8, Error> {
    let target = match (to, repo) {
        (PublishTo::Github, Some(repo)) => Target::GitHub {
            repo: repo.to_owned(),
        },
        (PublishTo::Github, None) => {
            return Err(Error::new(
                &codes::CLI_USAGE,
                "skills publish --to github needs --repo owner/repo",
            ));
        }
        (PublishTo::Clawhub, _) => Target::ClawHub,
    };
    let dir_name = dir
        .canonicalize()
        .ok()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let catalog = env.profile_catalog();
    let resolves = |name: &str| catalog.lookup(name).is_ok();
    let ctx = LintContext {
        dir_name: &dir_name,
        description_chars: env.scope.config().skills.index_description_chars as usize,
        resolves: &resolves,
    };
    let key = signing_key(env);
    let published = publish::publish(&hub(env)?, dir, &target, key.as_deref(), &ctx)?;
    if env.json {
        return json(
            &json!({"name": published.name, "url": published.url, "signed": published.signed}),
        );
    }
    print_raw(&format!("{}\n", published.url))
}

fn signing_key(env: &Env) -> Option<PathBuf> {
    let key = &env.scope.config().skills.signing_key;
    (!key.trim().is_empty()).then(|| pmt_skills::roots::expand(key, env.user_home.as_deref()))
}

/// `skills snapshot export|import`.
pub(super) fn snapshot(env: &Env, command: &SnapshotCommand) -> Result<u8, Error> {
    match command {
        SnapshotCommand::Export { file } => {
            let snap = snapshot::export(&hub(env)?)?;
            let text = serde_json::to_string_pretty(&snap).unwrap_or_default() + "\n";
            if file == "-" {
                return print_raw(&text);
            }
            std::fs::write(file, text)
                .map_err(|e| Error::io(&codes::SKILL_IO, Path::new(file), &e))?;
            eprintln!(
                "exported {} skill(s) and {} tap(s) to {file}",
                snap.skills.len(),
                snap.taps.len()
            );
            Ok(0)
        }
        SnapshotCommand::Import { file, force } => import(env, file, *force),
    }
}

fn import(env: &Env, file: &Path, force: bool) -> Result<u8, Error> {
    let text = std::fs::read_to_string(file).map_err(|e| Error::io(&codes::SKILL_IO, file, &e))?;
    let snap = snapshot::parse(&text)?;
    let mut hub = hub(env)?;
    let rows = snapshot::import(&mut hub, &snap, force);
    let failed = rows.iter().any(|r| r.action == "failed");
    if env.json {
        json(&json!({"entries": rows}))?;
    } else {
        let mut text = String::new();
        for r in &rows {
            let detail = r
                .detail
                .as_deref()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default();
            let _ = writeln!(text, "{}: {}{detail}", r.name, r.action);
        }
        print_raw(&text)?;
    }
    Ok(if failed { pmt_core::exit::PARTIAL } else { 0 })
}

/// `skills sign <dir> [--key <path>|--keyless]`.
pub(super) fn sign(
    env: &Env,
    ctx: &Context<'_>,
    dir: &Path,
    key: Option<&Path>,
    keyless: bool,
) -> Result<u8, Error> {
    let files: Vec<(String, Vec<u8>)> = pmt_skills::layout::walk(dir)
        .into_iter()
        .filter(|rel| rel != SIGNATURE_FILE)
        .map(|rel| {
            let path = dir.join(&rel);
            std::fs::read(&path)
                .map(|b| (rel, b))
                .map_err(|e| Error::io(&codes::SKILL_IO, &path, &e))
        })
        .collect::<Result<_, _>>()?;
    if !files.iter().any(|(p, _)| p == "SKILL.md") {
        return Err(Error::new(
            &codes::SKILL_INVALID,
            format!("{}: no SKILL.md", dir.display()),
        ));
    }
    let digest = pmt_skills::digest::tree(files.iter().map(|(p, b)| (p.clone(), b.as_slice())));
    let file = if keyless {
        keyless_signature(env, ctx, &digest)?
    } else {
        let key = key
            .map(Path::to_path_buf)
            .or_else(|| signing_key(env))
            .ok_or_else(|| {
                Error::new(
                    &codes::CLI_USAGE,
                    "skills sign needs --key <path>, --keyless or skills.signing_key",
                )
            })?;
        sign::sign_minisign(&digest, &key)?
    };
    let identity = match sign::verify(
        Some(sign::render(&file).as_bytes()),
        &digest,
        &pmt_skillhub::keyless::TrustRoots::public(),
    ) {
        sign::Signature::Verified(identity) => identity,
        other => other.record(),
    };
    let path = dir.join(SIGNATURE_FILE);
    pmt_core::private_fs::write_atomic(&path, sign::render(&file).as_bytes())
        .map_err(|e| Error::io(&codes::SKILL_IO, &path, &e))?;
    if env.json {
        return json(&json!({"path": path, "digest": digest, "signer": identity}));
    }
    eprintln!(
        "signed {} as {identity} ({})",
        dir.display(),
        path.display()
    );
    Ok(0)
}

/// A keyless signature: the OpenID token from GitHub Actions' ambient
/// credentials, else a browser sign-in to the Sigstore issuer.
fn keyless_signature(env: &Env, ctx: &Context<'_>, digest: &str) -> Result<SignatureFile, Error> {
    let token = match ambient_token(env)? {
        Some(token) => token,
        None => browser_token(ctx)?,
    };
    let hub = hub(env)?;
    let bundle = keyless::sign(
        digest.as_bytes(),
        &token,
        hub.http.as_ref(),
        &Services::default(),
    )?;
    Ok(SignatureFile {
        version: 1,
        digest: digest.to_owned(),
        minisign: None,
        sigstore: Some(bundle),
    })
}

/// GitHub Actions' OpenID token for audience `sigstore`, when running
/// there.
fn ambient_token(env: &Env) -> Result<Option<String>, Error> {
    let (Ok(url), Ok(bearer)) = (
        std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL"),
        std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN"),
    ) else {
        return Ok(None);
    };
    let hub = hub(env)?;
    let request = pmt_skillhub::http::Request::get(format!("{url}&audience=sigstore"))
        .header("authorization", format!("Bearer {bearer}"));
    let value = pmt_skillhub::http::get_json(hub.http.as_ref(), request)?.unwrap_or_default();
    Ok(value["value"].as_str().map(str::to_owned))
}

/// The browser sign-in to `oauth2.sigstore.dev`; its id token.
fn browser_token(ctx: &Context<'_>) -> Result<String, Error> {
    use pmt_providers::auth::browser;
    use pmt_providers::auth::spec::{DeviceProtocol, OAuthSpec, Redirect, TokenEncoding};
    let spec = OAuthSpec {
        provider: "sigstore".to_owned(),
        client_id: "sigstore".to_owned(),
        authorize_url: Some("https://oauth2.sigstore.dev/auth/auth".to_owned()),
        token_url: "https://oauth2.sigstore.dev/auth/token".to_owned(),
        device_authorization_url: None,
        device_poll_url: None,
        verification_url: None,
        device_redirect_uri: None,
        device_protocol: DeviceProtocol::Rfc8628,
        revoke_url: None,
        scopes: vec!["openid".to_owned(), "email".to_owned()],
        redirect: Redirect {
            uri_host: "localhost".to_owned(),
            port: 0,
            path: "/auth/callback".to_owned(),
        },
        authorize_params: Vec::new(),
        token_encoding: TokenEncoding::Form,
        allowed_hosts: vec!["oauth2.sigstore.dev".to_owned()],
        exchange_url: None,
    };
    let ui = crate::auth_cmd::TerminalUi { launch: ctx.launch };
    let place = crate::auth_cmd::place(ctx);
    let client = pmt_net::http::client(&pmt_net::http::OutboundPolicy::default())?;
    let tokens = crate::model_cmd::block_on(async move {
        let listener = browser::bind(&spec).await?;
        browser::run(&client, &spec, &listener, &ui, place).await
    })??;
    tokens
        .id_token
        .map(|t| t.expose().to_owned())
        .ok_or_else(|| {
            Error::new(
                &codes::SKILL_SIGNATURE,
                "the Sigstore sign-in returned no id token",
            )
        })
}
