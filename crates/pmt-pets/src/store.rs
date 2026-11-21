//! Installed pets (spec: pets, Pet storage and sprite format): each under
//! `<profile>/pets/<slug>/` with a `pet.toml` (slug, name, author, source,
//! the atlas's sha256, license) and the atlas file. An atlas is validated
//! before anything is written, and a pet is assembled in a temporary
//! directory and renamed into place, so `pets/` never holds half a pet.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use crate::atlas::{Atlas, AtlasError};

/// A pet's `pet.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PetMeta {
    /// The directory name and the id `pets.slug` names.
    pub slug: String,
    /// The display name.
    pub name: String,
    /// Who made it.
    #[serde(default)]
    pub author: String,
    /// Where it came from (the atlas URL, or `generated`).
    #[serde(default)]
    pub source: String,
    /// SHA-256 of the atlas file.
    #[serde(default)]
    pub sha256: String,
    /// Its license, when the gallery names one.
    #[serde(default)]
    pub license: String,
}

/// An installed pet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// Its `pet.toml`.
    pub meta: PetMeta,
    /// Its directory.
    pub dir: PathBuf,
    /// Its atlas file.
    pub atlas: PathBuf,
}

impl Installed {
    /// Reads and validates the atlas, checking it is the file recorded at
    /// install.
    ///
    /// # Errors
    ///
    /// `pet.invalid_atlas` for a missing, changed or invalid atlas.
    pub fn load_atlas(&self) -> Result<Atlas, Error> {
        let bytes = std::fs::read(&self.atlas).map_err(|e| {
            Error::from(AtlasError(format!(
                "cannot read {}: {e}",
                self.atlas.display()
            )))
        })?;
        let atlas = Atlas::decode(&bytes)?;
        if !self.meta.sha256.is_empty() && atlas.sha256() != self.meta.sha256 {
            return Err(AtlasError(format!(
                "{} changed since it was installed (sha256 mismatch)",
                self.atlas.display()
            ))
            .into());
        }
        Ok(atlas)
    }
}

/// Whether `slug` names a pet directory: lowercase letters, digits, `-`
/// and `_`, at most 64 characters, starting with a letter or digit.
#[must_use]
pub fn valid_slug(slug: &str) -> bool {
    let mut chars = slug.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && slug.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// A slug made from a display name or description.
#[must_use]
pub fn slugify(text: &str) -> String {
    let mut slug = String::new();
    for c in text.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_end_matches('-').chars().take(40).collect();
    if slug.is_empty() {
        "pet".to_owned()
    } else {
        slug
    }
}

/// The pets of one profile.
#[derive(Debug, Clone)]
pub struct Pets {
    dir: PathBuf,
}

fn io(context: &str, path: &Path, e: &std::io::Error) -> Error {
    Error::new(
        &codes::HOME_IO,
        format!("{context} {}: {e}", path.display()),
    )
}

/// `pet.not_found` for `slug`.
#[must_use]
pub fn not_found(slug: &str) -> Error {
    Error::new(
        &codes::PET_NOT_FOUND,
        format!("no pet '{slug}' is installed"),
    )
    .with_hint("see 'prometheus pets list --installed', or install one with 'prometheus pets install <slug>'")
}

impl Pets {
    /// The pets of the profile at `profile_dir`.
    #[must_use]
    pub fn new(profile_dir: &Path) -> Self {
        Self {
            dir: profile_dir.join("pets"),
        }
    }

    /// `<profile>/pets`.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Every installed pet whose `pet.toml` reads and whose atlas file
    /// exists, by slug.
    #[must_use]
    pub fn installed(&self) -> Vec<Installed> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut pets: Vec<Installed> = entries
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|name| valid_slug(name))
            .filter_map(|slug| self.get(&slug))
            .collect();
        pets.sort_by(|a, b| a.meta.slug.cmp(&b.meta.slug));
        pets
    }

    /// The installed pet `slug`.
    #[must_use]
    pub fn get(&self, slug: &str) -> Option<Installed> {
        if !valid_slug(slug) {
            return None;
        }
        let dir = self.dir.join(slug);
        let text = std::fs::read_to_string(dir.join("pet.toml")).ok()?;
        let meta: PetMeta = toml::from_str(&text).ok()?;
        let atlas = ["png", "webp"]
            .iter()
            .map(|ext| dir.join(format!("atlas.{ext}")))
            .find(|p| p.is_file())?;
        Some(Installed { meta, dir, atlas })
    }

    /// The pet surfaces show: `configured` when installed, else the first
    /// installed one.
    #[must_use]
    pub fn active(&self, configured: &str) -> Option<Installed> {
        if !configured.is_empty()
            && let Some(pet) = self.get(configured)
        {
            return Some(pet);
        }
        self.installed().into_iter().next()
    }

    /// Installs `meta.slug` from `bytes` (its sha256 is recorded here).
    ///
    /// # Errors
    ///
    /// `pet.invalid_atlas` (nothing written), `pet.exists` without
    /// `force`, `home.io`.
    pub fn install(&self, meta: &PetMeta, bytes: &[u8], force: bool) -> Result<Installed, Error> {
        if !valid_slug(&meta.slug) {
            return Err(Error::new(
                &codes::PET_NOT_FOUND,
                format!("'{}' is not a valid pet slug", meta.slug),
            ));
        }
        let atlas = Atlas::decode(bytes)?;
        let target = self.dir.join(&meta.slug);
        if target.exists() && !force {
            return Err(Error::new(
                &codes::PET_EXISTS,
                format!("pet '{}' is installed already", meta.slug),
            )
            .with_hint("pass --force to replace it"));
        }
        std::fs::create_dir_all(&self.dir).map_err(|e| io("cannot create", &self.dir, &e))?;
        let staging = self
            .dir
            .join(format!(".staging-{}-{}", meta.slug, std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        let written = write_pet(&staging, meta, &atlas, bytes);
        if let Err(err) = written {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(err);
        }
        self.swap_in(&staging, &target)?;
        self.get(&meta.slug)
            .ok_or_else(|| Error::new(&codes::HOME_IO, "the installed pet does not read back"))
    }

    /// Moves a staged pet into `target`, replacing what is there.
    fn swap_in(&self, staging: &Path, target: &Path) -> Result<(), Error> {
        let old = self.dir.join(format!(
            ".old-{}-{}",
            target.file_name().and_then(|n| n.to_str()).unwrap_or("pet"),
            std::process::id()
        ));
        let had = target.exists();
        if had {
            std::fs::rename(target, &old).map_err(|e| io("cannot replace", target, &e))?;
        }
        if let Err(e) = std::fs::rename(staging, target) {
            if had {
                let _ = std::fs::rename(&old, target);
            }
            let _ = std::fs::remove_dir_all(staging);
            return Err(io("cannot install", target, &e));
        }
        if had {
            let _ = std::fs::remove_dir_all(&old);
        }
        Ok(())
    }

    /// Deletes `slug`; whether it was installed.
    ///
    /// # Errors
    ///
    /// `home.io`.
    pub fn remove(&self, slug: &str) -> Result<bool, Error> {
        if self.get(slug).is_none() {
            return Ok(false);
        }
        let dir = self.dir.join(slug);
        std::fs::remove_dir_all(&dir).map_err(|e| io("cannot remove", &dir, &e))?;
        Ok(true)
    }
}

fn write_pet(dir: &Path, meta: &PetMeta, atlas: &Atlas, bytes: &[u8]) -> Result<(), Error> {
    std::fs::create_dir_all(dir).map_err(|e| io("cannot create", dir, &e))?;
    let file = dir.join(format!("atlas.{}", atlas.format().extension()));
    std::fs::write(&file, bytes).map_err(|e| io("cannot write", &file, &e))?;
    let recorded = PetMeta {
        sha256: atlas.sha256().to_owned(),
        ..meta.clone()
    };
    let text = toml::to_string(&recorded).unwrap_or_default();
    let toml_path = dir.join("pet.toml");
    std::fs::write(&toml_path, text).map_err(|e| io("cannot write", &toml_path, &e))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{ImageFormat, RgbaImage};

    use super::*;
    use crate::atlas::{HEIGHT, WIDTH};

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        RgbaImage::new(width, height)
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    fn meta(slug: &str) -> PetMeta {
        PetMeta {
            slug: slug.into(),
            name: slug.to_uppercase(),
            author: "someone".into(),
            source: "https://petdex.dev/x.png".into(),
            sha256: String::new(),
            license: "CC-BY-4.0".into(),
        }
    }

    #[test]
    fn a_pet_installs_with_its_hash_and_is_listed() {
        let profile = tempfile::tempdir().unwrap();
        let pets = Pets::new(profile.path());
        let bytes = png(WIDTH, HEIGHT);
        let pet = pets.install(&meta("boba"), &bytes, false).unwrap();
        assert_eq!(pet.meta.sha256, crate::atlas::sha256_hex(&bytes));
        assert!(profile.path().join("pets/boba/pet.toml").is_file());
        assert!(profile.path().join("pets/boba/atlas.png").is_file());
        assert_eq!(pets.installed().len(), 1);
        pet.load_atlas().unwrap();
        let again = pets.install(&meta("boba"), &bytes, false).unwrap_err();
        assert_eq!(again.code().code, "pet.exists");
        pets.install(&meta("boba"), &bytes, true).unwrap();
        assert!(pets.remove("boba").unwrap());
        assert!(!pets.remove("boba").unwrap());
    }

    /// Scenario: Wrong dimensions (nothing is written under `pets/`).
    #[test]
    fn an_invalid_atlas_writes_nothing() {
        let profile = tempfile::tempdir().unwrap();
        let pets = Pets::new(profile.path());
        let err = pets
            .install(&meta("boba"), &png(1000, 1000), false)
            .unwrap_err();
        assert!(
            err.message()
                .starts_with("invalid pet atlas: expected 1536x1872"),
            "{err:?}"
        );
        assert!(!profile.path().join("pets").exists());
    }

    #[test]
    fn the_active_pet_is_the_configured_one_else_the_first() {
        let profile = tempfile::tempdir().unwrap();
        let pets = Pets::new(profile.path());
        let bytes = png(WIDTH, HEIGHT);
        pets.install(&meta("mochi"), &bytes, false).unwrap();
        pets.install(&meta("boba"), &bytes, false).unwrap();
        assert_eq!(pets.active("").unwrap().meta.slug, "boba");
        assert_eq!(pets.active("mochi").unwrap().meta.slug, "mochi");
        assert_eq!(pets.active("gone").unwrap().meta.slug, "boba");
    }

    #[test]
    fn slugs_are_single_safe_segments() {
        assert!(valid_slug("boba"));
        assert!(valid_slug("sleepy-fox_2"));
        for bad in ["", "..", "a/b", "Boba", "-x", &"a".repeat(65)] {
            assert!(!valid_slug(bad), "{bad}");
        }
        assert_eq!(slugify("A Sleepy Fox!"), "a-sleepy-fox");
        assert_eq!(slugify("!!!"), "pet");
    }
}
