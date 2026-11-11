//! Layered lookup: the strings of one language and surface, resolved once
//! from every layer that supplies them.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::catalog::{self, Entries, placeholders};
use crate::{ENGLISH, Surface, bundled};

/// A language an enabled language-pack plugin declares
/// (`[[provides_locales]]`), with its catalogs in `<dir>/locales/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackLocale {
    /// The plugin id.
    pub plugin: String,
    /// The language id.
    pub id: String,
    /// The language's own name (default: the id).
    pub endonym: String,
    /// Written right to left.
    pub rtl: bool,
    /// The plugin directory.
    pub dir: PathBuf,
}

impl PackLocale {
    /// The pack's catalog file of `surface`.
    #[must_use]
    pub fn file(&self, surface: Surface) -> PathBuf {
        self.dir
            .join(crate::OVERLAY_DIR)
            .join(surface.file_name(&self.id))
    }
}

/// Everything that decides a profile's strings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sources {
    /// `display.language`.
    pub language: String,
    /// `<profile_home>/locales` (`None`: no overlay).
    pub overlay_dir: Option<PathBuf>,
    /// The enabled packs' languages, in plugin load order.
    pub packs: Vec<PackLocale>,
}

impl Sources {
    /// The sources of the profile at `profile_dir`.
    #[must_use]
    pub fn of_profile(language: &str, profile_dir: &Path, packs: Vec<PackLocale>) -> Self {
        Self {
            language: language.to_owned(),
            overlay_dir: Some(profile_dir.join(crate::OVERLAY_DIR)),
            packs,
        }
    }

    /// Every file a layer of this language or English may read, for
    /// change detection.
    #[must_use]
    pub fn files(&self, surface: Surface) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for id in [self.language.as_str(), ENGLISH] {
            files.extend(
                self.packs
                    .iter()
                    .filter(|p| p.id == id)
                    .map(|p| p.file(surface)),
            );
            if let Some(dir) = &self.overlay_dir {
                files.push(dir.join(surface.file_name(id)));
            }
        }
        files
    }
}

/// One layer: where it came from (for warnings) and its entries.
struct Layer {
    origin: String,
    entries: Entries,
}

/// The layers of language `id`, highest precedence first: packs (the last
/// loaded first), the overlay, the bundled catalog. Invalid files are
/// reported and skipped.
fn chain(sources: &Sources, id: &str, surface: Surface, warnings: &mut Vec<String>) -> Vec<Layer> {
    let mut files: Vec<PathBuf> = sources
        .packs
        .iter()
        .rev()
        .filter(|p| p.id == id)
        .map(|p| p.file(surface))
        .collect();
    if let Some(dir) = &sources.overlay_dir {
        files.push(dir.join(surface.file_name(id)));
    }
    let mut layers: Vec<Layer> = files
        .into_iter()
        .filter_map(|path| read_layer(&path, warnings))
        .collect();
    layers.push(Layer {
        origin: format!("bundled {}", surface.file_name(id)),
        entries: bundled::entries(id, surface).clone(),
    });
    layers
}

fn read_layer(path: &Path, warnings: &mut Vec<String>) -> Option<Layer> {
    let text = std::fs::read_to_string(path).ok()?;
    match catalog::parse(&text) {
        Ok(entries) => Some(Layer {
            origin: path.display().to_string(),
            entries,
        }),
        Err(reason) => {
            warnings.push(format!("{}: {reason}; the file is ignored", path.display()));
            None
        }
    }
}

/// The resolved strings of one language and surface.
#[derive(Debug, Clone)]
pub struct Strings {
    language: String,
    values: HashMap<String, String>,
    warnings: Vec<String>,
}

impl Strings {
    /// The bundled English strings of `surface` (libraries and tests that
    /// render without a profile).
    #[must_use]
    pub fn english(surface: Surface) -> Arc<Self> {
        static ENGLISH_STRINGS: OnceLock<[Arc<Strings>; 3]> = OnceLock::new();
        let all = ENGLISH_STRINGS
            .get_or_init(|| Surface::ALL.map(|s| Arc::new(Self::load(&Sources::default(), s))));
        Arc::clone(&all[Surface::ALL.iter().position(|s| *s == surface).unwrap_or(0)])
    }

    /// Resolves every key of `surface` from `sources`: per key, the first
    /// layer of the language that has it, unless its placeholders differ
    /// from English (then the English value, with one WARN for the key);
    /// then the English layers. Each warning is also logged.
    #[must_use]
    pub fn load(sources: &Sources, surface: Surface) -> Self {
        let language = if sources.language.is_empty() {
            ENGLISH.to_owned()
        } else {
            sources.language.clone()
        };
        let mut warnings = Vec::new();
        let english = chain(sources, ENGLISH, surface, &mut warnings);
        let own = if language == ENGLISH {
            Vec::new()
        } else {
            chain(sources, &language, surface, &mut warnings)
        };
        let reference = bundled::english(surface);
        let keys: BTreeSet<&String> = own
            .iter()
            .chain(&english)
            .flat_map(|l| l.entries.keys())
            .collect();
        let mut values = HashMap::with_capacity(keys.len());
        for key in keys {
            let expected = reference.get(key.as_str()).map(|t| placeholders(t));
            let fits = |text: &str| expected.as_ref().is_none_or(|e| placeholders(text) == *e);
            let pick = |layers: &[Layer], warnings: &mut Vec<String>| {
                let layer = layers
                    .iter()
                    .find(|l| l.entries.contains_key(key.as_str()))?;
                let text = &layer.entries[key.as_str()];
                if fits(text) {
                    return Some(text.clone());
                }
                warnings.push(format!(
                    "{}: {key} uses placeholders other than English's; English is shown",
                    layer.origin
                ));
                None
            };
            let value = pick(&own, &mut warnings).or_else(|| {
                // The English layers in order, skipping misfits.
                english
                    .iter()
                    .filter_map(|l| l.entries.get(key.as_str()))
                    .find(|t| fits(t))
                    .cloned()
            });
            if let Some(value) = value {
                values.insert(key.clone(), value);
            }
        }
        for warning in &warnings {
            tracing::warn!(target: "pmt_i18n", language = %language, "{warning}");
        }
        Self {
            language,
            values,
            warnings,
        }
    }

    /// The language these strings are in.
    #[must_use]
    pub fn language(&self) -> &str {
        &self.language
    }

    /// What loading reported: invalid files and misfit placeholders.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The text of `key`; the key itself when no layer has it.
    #[must_use]
    pub fn get<'a>(&'a self, key: &'a str) -> &'a str {
        self.values.get(key).map_or(key, String::as_str)
    }

    /// The text of `key` with its `{placeholders}` substituted.
    #[must_use]
    pub fn fmt(&self, key: &str, args: &[(&str, &dyn std::fmt::Display)]) -> String {
        catalog::render(self.get(key), args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlay(files: &[(&str, &str)]) -> (tempfile::TempDir, Sources) {
        let dir = tempfile::tempdir().unwrap();
        let locales = dir.path().join("locales");
        std::fs::create_dir_all(&locales).unwrap();
        for (name, text) in files {
            std::fs::write(locales.join(name), text).unwrap();
        }
        let sources = Sources::of_profile("en", dir.path(), Vec::new());
        (dir, sources)
    }

    /// Scenario: Overlay changes one string.
    #[test]
    fn an_overlay_changes_one_string_and_keeps_the_rest_bundled() {
        let (_dir, sources) = overlay(&[("en.toml", "[approval]\ndenied = \"Nope.\"\n")]);
        let strings = Strings::load(&sources, Surface::Core);
        assert_eq!(strings.get("approval.denied"), "Nope.");
        let english = Strings::english(Surface::Core);
        assert_eq!(
            strings.get("gateway.stopped"),
            english.get("gateway.stopped")
        );
        assert_eq!(english.get("approval.denied"), "Denied");
    }

    /// Scenario: Misspelled placeholder falls back.
    #[test]
    fn a_misspelled_placeholder_falls_back_to_english_with_a_warning() {
        let (_dir, mut sources) = overlay(&[(
            "de.toml",
            "[gateway]\nbusy_rejected = \"Agent läuft: /{befehl}\"\n",
        )]);
        sources.language = "de".into();
        let strings = Strings::load(&sources, Surface::Core);
        assert_eq!(
            strings.fmt("gateway.busy_rejected", &[("command", &"model")]),
            "Agent is running - /model can't run mid-turn. Wait for the response or /stop first."
        );
        assert_eq!(strings.warnings().len(), 1, "{:?}", strings.warnings());
        assert!(strings.warnings()[0].contains("gateway.busy_rejected"));
        // Every other key stays German.
        assert_eq!(strings.get("approval.denied"), "Abgelehnt");
    }

    #[test]
    fn a_file_with_a_value_that_is_not_text_is_ignored_and_reported() {
        let (_dir, sources) =
            overlay(&[("en.toml", "[approval]\ndenied = \"Nope.\"\ncount = 3\n")]);
        let strings = Strings::load(&sources, Surface::Core);
        assert_eq!(strings.get("approval.denied"), "Denied");
        assert!(strings.warnings()[0].contains("approval.count: value is not text"));
    }

    #[test]
    fn the_last_loaded_pack_wins_then_the_overlay_then_bundled() {
        let (dir, mut sources) = overlay(&[(
            "fr.toml",
            "[approval]\ndenied = \"overlay\"\nallowed_once = \"overlay once\"\n",
        )]);
        for (n, plugin) in ["first", "second"].iter().enumerate() {
            let pack = dir.path().join(plugin);
            std::fs::create_dir_all(pack.join("locales")).unwrap();
            std::fs::write(
                pack.join("locales/fr.toml"),
                format!("[approval]\ndenied = \"{plugin}\"\n[gateway]\nstopped = \"pack {n}\"\n"),
            )
            .unwrap();
            sources.packs.push(PackLocale {
                plugin: (*plugin).to_owned(),
                id: "fr".into(),
                endonym: "Français".into(),
                rtl: false,
                dir: pack,
            });
        }
        sources.language = "fr".into();
        let strings = Strings::load(&sources, Surface::Core);
        assert_eq!(strings.get("approval.denied"), "second");
        assert_eq!(strings.get("gateway.stopped"), "pack 1");
        assert_eq!(strings.get("approval.allowed_once"), "overlay once");
        assert_eq!(
            strings.get("approval.expired"),
            Strings::load(
                &Sources {
                    language: "fr".into(),
                    ..Sources::default()
                },
                Surface::Core
            )
            .get("approval.expired")
        );
    }

    #[test]
    fn an_unknown_key_reads_as_itself() {
        assert_eq!(
            Strings::english(Surface::Core).get("no.such.key"),
            "no.such.key"
        );
    }

    #[test]
    fn a_language_only_an_overlay_supplies_falls_back_to_english_per_key() {
        let (_dir, mut sources) = overlay(&[("pl.toml", "[approval]\ndenied = \"Odrzucono\"\n")]);
        sources.language = "pl".into();
        let strings = Strings::load(&sources, Surface::Core);
        assert_eq!(strings.get("approval.denied"), "Odrzucono");
        assert_eq!(strings.get("gateway.new_session"), "New session started.");
    }
}
