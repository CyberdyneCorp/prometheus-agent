//! External secret sources (spec: security-boundaries, External secret
//! sources are opt-in; configuration-and-profiles, External secret source
//! commands): Bitwarden Secrets Manager (`bws`, every secret of a project
//! by key: a bulk source) and 1Password (`op`, names mapped to references:
//! a mapped source).
//!
//! A disabled source is never built, so its CLI is never run. An enabled
//! one reads its bootstrap token only from the stored secret its
//! `access_token_secret` names, hands it to the CLI through the CLI's own
//! environment variable (never an argument), keeps what it resolved in
//! memory for at most `cache_ttl_secs`, and fails loudly: a refused token
//! or any other failure is an error, never an older cached value.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;

use crate::schema::{BitwardenSourceConfig, Config, OnePasswordSourceConfig, is_secret_name};

/// The most a source CLI may print.
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

/// How long a source CLI may run.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The bootstrap-token variables of the CLIs: set only to the configured
/// token, never inherited.
const TOKEN_VARS: [&str; 2] = ["BWS_ACCESS_TOKEN", "OP_SERVICE_ACCOUNT_TOKEN"];

/// How a source reads its bootstrap token (the profile's stored secret).
pub type TokenReader<'a> = &'a dyn Fn(&str) -> Result<Option<SecretString>, Error>;

/// The profile's enabled external sources.
#[derive(Debug)]
pub struct ExternalSources {
    onepassword: Option<Source<OnePasswordSourceConfig>>,
    bitwarden: Option<Source<BitwardenSourceConfig>>,
}

/// One enabled source: its settings, its CLI and what it resolved.
#[derive(Debug)]
struct Source<C> {
    config: C,
    program: PathBuf,
    cache: Mutex<BTreeMap<String, (Instant, Option<SecretString>)>>,
}

impl<C> Source<C> {
    fn new(config: C, program: PathBuf) -> Self {
        Self {
            config,
            program,
            cache: Mutex::default(),
        }
    }

    /// The value cached under `key` while it is younger than `ttl`.
    fn fresh(&self, key: &str, ttl: Duration) -> Option<SecretString> {
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache
            .get(key)
            .filter(|(at, _)| at.elapsed() < ttl)
            .and_then(|(_, value)| value.clone())
    }

    /// Whether `key` was cached less than `ttl` ago.
    fn is_fresh(&self, key: &str, ttl: Duration) -> bool {
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        cache.get(key).is_some_and(|(at, _)| at.elapsed() < ttl)
    }

    fn remember(&self, key: &str, value: Option<SecretString>) {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_owned(), (Instant::now(), value));
    }

    fn forget(&self) {
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

/// The key under which a bulk listing is cached.
const LISTING: &str = "";

impl ExternalSources {
    /// The enabled sources of `config` (`None` when both are off), their
    /// CLIs from the managed runtime store under `home`, else `PATH`.
    #[must_use]
    pub fn for_profile(config: &Config, home: &Path) -> Option<Self> {
        let secrets = &config.secrets;
        Self::with_programs(
            secrets
                .onepassword
                .enabled
                .then(|| (secrets.onepassword.clone(), program(home, "op"))),
            secrets
                .bitwarden
                .enabled
                .then(|| (secrets.bitwarden.clone(), program(home, "bws"))),
        )
    }

    /// The sources given, each with its CLI (`None` when neither is).
    #[must_use]
    pub fn with_programs(
        onepassword: Option<(OnePasswordSourceConfig, PathBuf)>,
        bitwarden: Option<(BitwardenSourceConfig, PathBuf)>,
    ) -> Option<Self> {
        if onepassword.is_none() && bitwarden.is_none() {
            return None;
        }
        Some(Self {
            onepassword: onepassword.map(|(c, p)| Source::new(c, p)),
            bitwarden: bitwarden.map(|(c, p)| Source::new(c, p)),
        })
    }

    /// The value of `name`: a mapped 1Password entry first, then the
    /// Bitwarden project's (the first claim of a key wins); `None` when
    /// neither has it.
    ///
    /// # Errors
    ///
    /// `secret.source_failed` when a source that would answer fails.
    pub fn get(&self, name: &str, token: TokenReader<'_>) -> Result<Option<SecretString>, Error> {
        if let Some(op) = &self.onepassword
            && let Some(reference) = op.config.map.get(name)
        {
            return onepassword(op, name, reference, token).map(Some);
        }
        match &self.bitwarden {
            Some(bws) if accepts(&bws.config, name) => Ok(bitwarden(bws, token)?.remove(name)),
            _ => Ok(None),
        }
    }

    /// Every value resolved and still cached, by name (for redaction; no
    /// source is contacted).
    #[must_use]
    pub fn cached(&self) -> Vec<(String, SecretString)> {
        let mut values = Vec::new();
        if let Some(op) = &self.onepassword {
            let cache = op.cache.lock().unwrap_or_else(PoisonError::into_inner);
            values.extend(
                cache
                    .iter()
                    .filter_map(|(name, (_, v))| v.clone().map(|v| (name.clone(), v))),
            );
        }
        if let Some(bws) = &self.bitwarden {
            let cache = bws.cache.lock().unwrap_or_else(PoisonError::into_inner);
            values.extend(cache.iter().filter_map(|(name, (_, v))| {
                (name != LISTING).then(|| v.clone().map(|v| (name.clone(), v)))?
            }));
        }
        values
    }
}

/// Whether the Bitwarden source supplies `name`: every key until `sync
/// --apply` recorded an accepted list.
fn accepts(config: &BitwardenSourceConfig, name: &str) -> bool {
    config.names.is_empty() || config.names.iter().any(|n| n == name)
}

/// `op read <reference>` for mapped `name`, cached.
fn onepassword(
    op: &Source<OnePasswordSourceConfig>,
    name: &str,
    reference: &str,
    token: TokenReader<'_>,
) -> Result<SecretString, Error> {
    let ttl = Duration::from_secs(op.config.cache_ttl_secs);
    if let Some(value) = op.fresh(name, ttl) {
        return Ok(value);
    }
    let failed = |why: String| {
        op.forget();
        failure("onepassword", &why)
    };
    let token = bootstrap(&op.config.access_token_secret.0, token).map_err(failed)?;
    let out = run(
        &op.program,
        &["read", "--no-newline", reference],
        "OP_SERVICE_ACCOUNT_TOKEN",
        &token,
    )
    .map_err(failed)?;
    let text =
        String::from_utf8(out).map_err(|_| failed("op read printed non-UTF-8".to_owned()))?;
    let value = SecretString::new(text.trim_end_matches(['\r', '\n']));
    op.remember(name, Some(value.clone()));
    Ok(value)
}

/// The Bitwarden project's secrets by key (`bws secret list`), cached.
fn bitwarden(
    bws: &Source<BitwardenSourceConfig>,
    token: TokenReader<'_>,
) -> Result<BTreeMap<String, SecretString>, Error> {
    let ttl = Duration::from_secs(bws.config.cache_ttl_secs);
    if bws.is_fresh(LISTING, ttl) {
        let cache = bws.cache.lock().unwrap_or_else(PoisonError::into_inner);
        return Ok(cache
            .iter()
            .filter(|(key, _)| key.as_str() != LISTING)
            .filter_map(|(key, (_, v))| v.clone().map(|v| (key.clone(), v)))
            .collect());
    }
    let failed = |why: String| {
        bws.forget();
        failure("bitwarden", &why)
    };
    let token = bootstrap(&bws.config.access_token_secret.0, token).map_err(failed)?;
    let values = bitwarden_list(&bws.config, &bws.program, &token).map_err(failed)?;
    bws.forget();
    bws.remember(LISTING, None);
    for (key, value) in &values {
        bws.remember(key, Some(value.clone()));
    }
    Ok(values)
}

/// Every secret the Bitwarden project of `config` supplies, by key, fetched
/// now with `program` (`bws secret list`) and `token`: keys that are
/// secret names and that `sync --apply` accepted (every key until it
/// recorded a list); the first claim of a key wins.
///
/// # Errors
///
/// A one-line reason (it never contains the token).
pub fn bitwarden_list(
    config: &BitwardenSourceConfig,
    program: &Path,
    token: &SecretString,
) -> Result<BTreeMap<String, SecretString>, String> {
    let mut args = vec!["secret", "list"];
    if let Some(project) = &config.project_id {
        args.push(project);
    }
    args.extend(["--output", "json"]);
    if let Some(url) = &config.server_url {
        args.extend(["--server-url", url]);
    }
    let out = run(program, &args, "BWS_ACCESS_TOKEN", token)?;
    let listed: Vec<serde_json::Value> = serde_json::from_slice(&out)
        .map_err(|_| "bws secret list did not print a JSON list".to_owned())?;
    let mut values = BTreeMap::new();
    for entry in &listed {
        let (Some(key), Some(value)) = (
            entry.get("key").and_then(serde_json::Value::as_str),
            entry.get("value").and_then(serde_json::Value::as_str),
        ) else {
            continue;
        };
        if is_secret_name(key) && accepts(config, key) && !values.contains_key(key) {
            values.insert(key.to_owned(), SecretString::new(value));
        }
    }
    Ok(values)
}

/// The stored bootstrap token named `name`.
fn bootstrap(name: &str, token: TokenReader<'_>) -> Result<SecretString, String> {
    match token(name) {
        Ok(Some(value)) => Ok(value),
        Ok(None) => Err(format!(
            "its access token secret {name} is not set; set it with: prometheus secrets set {name}"
        )),
        Err(err) => Err(format!(
            "its access token secret {name} cannot be read: {}",
            err.message()
        )),
    }
}

fn failure(source: &str, why: &str) -> Error {
    Error::new(
        &codes::SECRET_SOURCE_FAILED,
        format!("secrets.{source}: {why}"),
    )
    .with_hint(format!(
        "fix the source, or turn it off with secrets.{source}.enabled = false"
    ))
}

/// Runs `program args` with `token` in `variable` (and no other token
/// variable); its stdout when it exits 0. Stderr is discarded (a CLI may
/// echo what it was given).
///
/// # Errors
///
/// A one-line reason (it never contains the token).
pub fn run(
    program: &Path,
    args: &[&str],
    variable: &str,
    token: &SecretString,
) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let name = program.file_name().map_or_else(
        || program.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let mut command = Command::new(program);
    command.args(args);
    for var in TOKEN_VARS {
        command.env_remove(var);
    }
    let mut child = command
        .env(variable, token.expose())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            format!(
                "{name} could not start ({}); install it or fix PATH",
                e.kind()
            )
        })?;
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let limit = u64::try_from(MAX_OUTPUT + 1).unwrap_or(u64::MAX);
        if let Some(stdout) = stdout {
            let _ = stdout.take(limit).read_to_end(&mut out);
        }
        out
    });
    // A CLI waiting for input (an interactive sign-in) is a failure too.
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{name} did not finish within {} s",
                    TIMEOUT.as_secs()
                ));
            }
        }
    };
    let out = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(format!(
            "{name} failed ({status}); the access token may be invalid or revoked"
        ));
    }
    if out.len() > MAX_OUTPUT {
        return Err(format!("{name} printed more than {MAX_OUTPUT} bytes"));
    }
    Ok(out)
}

/// The CLI `name`: the newest managed install under
/// `<home>/runtimes/<name>/`, else the bare name (found on `PATH` when
/// run).
#[must_use]
pub fn program(home: &Path, name: &str) -> PathBuf {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    let mut versions: Vec<PathBuf> = std::fs::read_dir(home.join("runtimes").join(name))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    versions.sort();
    versions
        .iter()
        .rev()
        .flat_map(|dir| [dir.join(&exe), dir.join("bin").join(&exe)])
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(exe))
}

/// The vault's password-manager login sources (`vault.<name>`) and the
/// CLI each one runs.
pub const LOGIN_SOURCES: [(&str, &str); 2] = [("onepassword", "op"), ("bitwarden", "bw")];

/// Where the CLI `name` a session would run is: the managed install
/// ([`program`]), else the first `name` on `path` (the launch `PATH`);
/// `None` when neither exists. Sessions, `vault sources` and the
/// `vault.sources` RPC all ask this, so they agree on availability.
#[must_use]
pub fn locate(home: &Path, name: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let managed = program(home, name);
    if managed.is_absolute() {
        return Some(managed);
    }
    std::env::split_paths(path?)
        .map(|dir| dir.join(&managed))
        .find(|candidate| candidate.is_file())
}

/// Checks the sources' settings.
///
/// # Errors
///
/// `config.invalid` for a 1Password mapping whose name is not a secret
/// name or whose reference is not `op://...`.
pub fn check(config: &Config) -> Result<(), Error> {
    for (name, reference) in &config.secrets.onepassword.map {
        if !is_secret_name(name) || !reference.starts_with("op://") {
            return Err(Error::new(
                &codes::CONFIG_INVALID,
                format!(
                    "secrets.onepassword.map.{name}: map a secret name (^[A-Z][A-Z0-9_]{{0,127}}$) to an op://vault/item/field reference"
                ),
            ));
        }
    }
    Ok(())
}
