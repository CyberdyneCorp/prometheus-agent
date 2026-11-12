//! `display.language` (spec: i18n, display.language selection; Language
//! changes apply without restart (upgrade)).
//!
//! Loading only checks the id's form: a language pack disabled after its
//! language was selected must not make the profile unloadable (its keys
//! fall back to English). Selecting a language is stricter: a write that
//! changes `display.language` to an id no catalog layer supplies is
//! refused (exit 78).
//!
//! [`strings`] serves a profile's current strings to every surface of the
//! process. It re-reads what decides them (the config file, the overlay
//! directory, the plugins and the packs' catalogs) at most once per
//! [`RECHECK`], and reloads only when one of them changed, so a language,
//! overlay or pack change shows within seconds without a restart. Nothing
//! here touches a session: UI strings never enter a model request.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use pmt_core::error::Error;
use pmt_i18n::{PackLocale, Sources, Strings, Surface};

use crate::layers::Overrides;
use crate::schema::Config;
use crate::write::ConfigFile;

/// How long [`strings`] trusts what it loaded before looking for changes.
pub const RECHECK: Duration = Duration::from_secs(1);

/// The enabled language packs of the profile in the directory under its
/// configuration, in plugin load order.
pub type PackResolver = Box<dyn Fn(&Config, &Path) -> Vec<PackLocale> + Send + Sync>;

/// The plugin system's pack lookup, set once by the binary that owns the
/// plugins. Unset (libraries, tests): no packs.
static PACKS: OnceLock<PackResolver> = OnceLock::new();

/// Registers the language pack lookup. The first call wins.
pub fn register_packs(resolve: PackResolver) {
    let _ = PACKS.set(resolve);
}

/// The sources of the profile at `profile_dir` under `config`.
#[must_use]
pub fn sources(config: &Config, profile_dir: &Path) -> Sources {
    let packs = PACKS
        .get()
        .map(|resolve| resolve(config, profile_dir))
        .unwrap_or_default();
    Sources::of_profile(&config.display.language, profile_dir, packs)
}

/// The load-time check: the id's form.
///
/// # Errors
///
/// `config.invalid` naming the malformed id.
pub fn check(config: &Config) -> Result<(), Error> {
    pmt_i18n::check_id(&config.display.language)
}

/// The write-time check: a write changing `display.language` from what
/// `old` (the file text before the write) holds must select an available
/// language.
///
/// # Errors
///
/// `config.invalid` (exit 78) listing the available ids.
pub(crate) fn check_change(old: &str, new: &Config, profile_dir: &Path) -> Result<(), Error> {
    let language = &new.display.language;
    if written_language(old)
        .as_deref()
        .unwrap_or(pmt_i18n::ENGLISH)
        == language
    {
        return Ok(());
    }
    pmt_i18n::check_available(language, &sources(new, profile_dir), profile_dir)
}

fn written_language(text: &str) -> Option<String> {
    let doc: toml::Table = text.parse().ok()?;
    doc.get("display")?
        .get("language")?
        .as_str()
        .map(str::to_owned)
}

/// What [`strings`] keeps per profile and surface.
struct Cached {
    checked: Instant,
    stamp: Vec<(PathBuf, Option<(SystemTime, u64)>)>,
    strings: Arc<Strings>,
}

/// The current strings of the profile at `profile_dir` for `surface`.
#[must_use]
pub fn strings(profile_dir: &Path, surface: Surface) -> Arc<Strings> {
    static CACHE: OnceLock<Mutex<HashMap<(PathBuf, Surface), Cached>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    let key = (profile_dir.to_path_buf(), surface);
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(cached) = cache.get_mut(&key) {
        if cached.checked.elapsed() < RECHECK {
            return Arc::clone(&cached.strings);
        }
        cached.checked = Instant::now();
        if stamp(&cached.stamp) == cached.stamp {
            return Arc::clone(&cached.strings);
        }
    }
    let sources = current_sources(profile_dir);
    let strings = Arc::new(Strings::load(&sources, surface));
    let watched = watched(profile_dir, &sources, surface);
    cache.insert(
        key,
        Cached {
            checked: Instant::now(),
            stamp: stamp(&watched),
            strings: Arc::clone(&strings),
        },
    );
    strings
}

/// The sources the profile's configuration names now; English alone when
/// the configuration cannot be read (its error surfaces elsewhere).
fn current_sources(profile_dir: &Path) -> Sources {
    let file = ConfigFile::new(
        profile_dir.join("config.toml"),
        profile_dir.join("backups").join("config"),
    );
    match file.load_unmigrated(&Overrides::default()) {
        Ok(layered) => sources(layered.config(), profile_dir),
        Err(_) => Sources::of_profile(pmt_i18n::ENGLISH, profile_dir, Vec::new()),
    }
}

/// Every path whose change can change the strings.
fn watched(
    profile_dir: &Path,
    sources: &Sources,
    surface: Surface,
) -> Vec<(PathBuf, Option<(SystemTime, u64)>)> {
    let mut paths = vec![
        profile_dir.join("config.toml"),
        profile_dir.join("grants.toml"),
        profile_dir.join("plugins"),
        profile_dir.join(pmt_i18n::OVERLAY_DIR),
    ];
    if let Ok(entries) = std::fs::read_dir(profile_dir.join(pmt_i18n::OVERLAY_DIR)) {
        paths.extend(entries.filter_map(Result::ok).map(|e| e.path()));
    }
    paths.extend(sources.files(surface));
    paths.sort();
    paths.dedup();
    paths.into_iter().map(|p| (p, None)).collect()
}

/// The modification time and length of each watched path, as now.
fn stamp(
    watched: &[(PathBuf, Option<(SystemTime, u64)>)],
) -> Vec<(PathBuf, Option<(SystemTime, u64)>)> {
    watched
        .iter()
        .map(|(path, _)| {
            let meta = std::fs::metadata(path)
                .ok()
                .and_then(|m| Some((m.modified().ok()?, m.len())));
            (path.clone(), meta)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(language: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            format!(
                "schema_version = {}\n\n[display]\nlanguage = \"{language}\"\n",
                crate::SCHEMA_VERSION
            ),
        )
        .unwrap();
        dir
    }

    fn after_recheck() {
        std::thread::sleep(RECHECK + Duration::from_millis(50));
    }

    #[test]
    fn strings_follow_the_profile_language_and_overlay_edits_without_a_restart() {
        let dir = profile("es");
        let strings = strings(dir.path(), Surface::Core);
        assert_eq!(strings.language(), "es");
        assert_eq!(strings.get("approval.denied"), "Denegado");
        std::fs::create_dir_all(dir.path().join("locales")).unwrap();
        std::fs::write(
            dir.path().join("locales/es.toml"),
            "[approval]\ndenied = \"¡No!\"\n",
        )
        .unwrap();
        after_recheck();
        assert_eq!(
            super::strings(dir.path(), Surface::Core).get("approval.denied"),
            "¡No!"
        );
        std::fs::write(
            dir.path().join("config.toml"),
            format!(
                "schema_version = {}\n\n[display]\nlanguage = \"ja\"\n",
                crate::SCHEMA_VERSION
            ),
        )
        .unwrap();
        after_recheck();
        assert_eq!(super::strings(dir.path(), Surface::Core).language(), "ja");
    }

    #[test]
    fn a_malformed_id_does_not_load() {
        let mut config = Config::default();
        config.display.language = "Deutsch".into();
        assert!(check(&config).is_err());
        config.display.language = "pl".into();
        assert!(
            check(&config).is_ok(),
            "an unavailable but well-formed id loads"
        );
    }

    #[test]
    fn only_a_changed_language_must_be_available() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.display.language = "pl".into();
        let err = check_change("", &config, dir.path()).unwrap_err();
        assert!(
            err.message().starts_with("Unknown language 'pl'"),
            "{}",
            err.message()
        );
        assert!(check_change("[display]\nlanguage = \"pl\"\n", &config, dir.path()).is_ok());
        config.display.language = "de".into();
        assert!(check_change("", &config, dir.path()).is_ok());
    }
}
