//! The managed trigger's binary side (spec: scheduled-jobs, Managed
//! scheduler contract; Pluggable trigger provider): arming each enabled
//! job's next occurrence at `cron.managed.endpoint` ([`sync`]) and
//! verifying a fire's token against the scheduler's JWKS ([`verified`]).
//! The daemon serves the fires (`daemon_cmd/host/cron_managed.rs`); the
//! claim is `pmt_cron::managed::claim_fire`.
//!
//! What was armed is kept in `<profile>/cron/managed-armed.json`, so a
//! sync posts only new occurrences; the scheduler deduplicates on the
//! key, so two processes arming the same occurrence are harmless.

use std::collections::{BTreeSet, HashMap};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use pmt_config::ProfileScope;
use pmt_config::schema::{CronManagedConfig, CronProvider};
use pmt_core::error::{Error, codes};
use pmt_cron::managed::{self, Arm};
use pmt_store::Store;
use serde_json::{Value, json};

/// How long a fetched JWKS is trusted before it is fetched again.
const JWKS_TTL: Duration = Duration::from_mins(5);
/// A token no cached key verifies refetches the set at most this often.
const JWKS_REFETCH: Duration = Duration::from_secs(30);

/// The managed settings of `scope` when the managed trigger is in
/// effect.
#[must_use]
pub fn in_effect(scope: &ProfileScope) -> Option<CronManagedConfig> {
    let cron = &scope.config().cron;
    let (provider, _) = pmt_cron::doctor::effective_provider(cron);
    (provider == CronProvider::Managed).then(|| cron.managed.clone())
}

fn armed_file(scope: &ProfileScope) -> std::path::PathBuf {
    scope.path("cron").join("managed-armed.json")
}

fn unavailable(message: String) -> Error {
    Error::new(&codes::NET_CLIENT_INIT, message)
}

/// Arms every occurrence of `scope`'s jobs not armed yet and forgets the
/// ones no job needs any more; `Ok(())` once the scheduler took all of
/// them. Nothing happens when the managed trigger is not in effect.
///
/// # Errors
///
/// Why the scheduler could not be reached or refused an arm (the caller
/// falls back to the built-in trigger); store errors.
pub async fn sync(scope: &ProfileScope, store: &Store) -> Result<(), Error> {
    let Some(config) = in_effect(scope) else {
        return Ok(());
    };
    let file = armed_file(scope);
    let mut armed: BTreeSet<String> = std::fs::read_to_string(&file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let arms = managed::arms(store)?;
    let client = crate::model_cmd::client(scope.config())?;
    let bearer = match &config.token_secret {
        Some(name) => Some(scope.secrets().resolve(&name.0)?),
        None => None,
    };
    let mut failure = None;
    let new: Vec<&Arm> = arms
        .iter()
        .filter(|a| !armed.contains(&a.dedup_key))
        .collect();
    for arm in new {
        match post(
            &client,
            &config,
            scope.name().as_str(),
            arm,
            bearer.as_ref(),
        )
        .await
        {
            Ok(()) => {
                armed.insert(arm.dedup_key.clone());
            }
            Err(err) => {
                failure = Some(err);
                break;
            }
        }
    }
    let wanted: BTreeSet<&String> = arms.iter().map(|a| &a.dedup_key).collect();
    armed.retain(|key| wanted.contains(key));
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let text = serde_json::to_string(&armed).unwrap_or_default();
    if let Err(e) = pmt_core::private_fs::write_atomic(&file, text.as_bytes()) {
        tracing::warn!(
            "cannot record the armed occurrences in {}: {e}",
            file.display()
        );
    }
    failure.map_or(Ok(()), Err)
}

async fn post(
    client: &reqwest::Client,
    config: &CronManagedConfig,
    profile: &str,
    arm: &Arm,
    bearer: Option<&pmt_core::secret::SecretString>,
) -> Result<(), Error> {
    let endpoint = config.endpoint.as_deref().unwrap_or_default();
    let body = json!({
        "job_id": arm.job_id,
        "profile": profile,
        "fire_at": pmt_cron::schedule::at_ms(arm.fire_at_ms).to_string(),
        "dedup_key": arm.dedup_key,
        "callback_url": config.callback_url,
    });
    let mut request = client.post(endpoint).json(&body);
    if let Some(token) = bearer {
        request = request.bearer_auth(token.expose());
    }
    let response = request.send().await.map_err(|e| {
        unavailable(format!(
            "the managed scheduler at {endpoint} is unreachable: {e}"
        ))
    })?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(unavailable(format!(
            "the managed scheduler at {endpoint} refused an arm: HTTP {}",
            response.status().as_u16()
        )))
    }
}

/// `sync` from a command outside the daemon (a `cron` mutation): a
/// failure is a warning, the daemon re-arms on its next pass.
pub fn sync_now(scope: &ProfileScope, store: &Store) {
    if in_effect(scope).is_none() {
        return;
    }
    match crate::model_cmd::block_on(sync(scope, store)) {
        Ok(Ok(())) => {}
        Ok(Err(err)) | Err(err) => {
            eprintln!(
                "warning: {}; the daemon arms it again on its next pass",
                err.message()
            );
        }
    }
}

/// Fetched key sets by URL, with when they were fetched.
static KEYS: LazyLock<Mutex<HashMap<String, (Value, Instant)>>> = LazyLock::new(Mutex::default);

async fn fetch_jwks(scope: &ProfileScope, url: &str) -> Result<Value, Error> {
    let client = crate::model_cmd::client(scope.config())?;
    let keys: Value = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| unavailable(format!("cannot fetch the JWKS at {url}: {e}")))?
        .json()
        .await
        .map_err(|e| unavailable(format!("the JWKS at {url} is not JSON: {e}")))?;
    KEYS.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(url.to_owned(), (keys.clone(), Instant::now()));
    Ok(keys)
}

/// Whether `token` is a valid fire token of `scope`'s managed scheduler
/// for the fire `dedup_key` at `now`: signed (asymmetrically) by a key of
/// `cron.managed.jwks_url`, with the claims
/// [`pmt_cron::managed::claims_ok`] checks.
pub async fn verified(scope: &ProfileScope, token: &str, dedup_key: &str, now: Timestamp) -> bool {
    let Some(config) = in_effect(scope) else {
        return false;
    };
    let (Some(url), Some(audience)) = (config.jwks_url.as_deref(), config.audience.as_deref())
    else {
        return false;
    };
    let cached = KEYS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(url)
        .cloned();
    let check = |keys: &Value| {
        pmt_net::jwt::verify(token, keys)
            .is_ok_and(|claims| managed::claims_ok(&claims, audience, dedup_key, now))
    };
    match cached {
        Some((keys, at)) if at.elapsed() < JWKS_TTL && check(&keys) => true,
        Some((_, at)) if at.elapsed() < JWKS_REFETCH => false,
        _ => match fetch_jwks(scope, url).await {
            Ok(keys) => check(&keys),
            Err(err) => {
                tracing::warn!("{}", err.message());
                false
            }
        },
    }
}
