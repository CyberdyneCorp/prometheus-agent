//! The bundled skill tree, embedded in the binary (spec: skills, Bundled
//! skills are read-only and ejectable).
//!
//! Bundled skills are read from here every time; nothing is copied into a
//! profile unless the user ejects a skill (`crate::eject`). Tests build
//! their own trees with [`Bundled::from_files`].

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock};

use crate::digest;

mod generated {
    include!(concat!(env!("OUT_DIR"), "/bundled_files.rs"));
}

/// An embedded file tree.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Bundled {
    files: BTreeMap<String, Cow<'static, [u8]>>,
}

static BUILTIN: LazyLock<Arc<Bundled>> = LazyLock::new(|| {
    Arc::new(Bundled {
        files: generated::FILES
            .iter()
            .map(|(path, bytes)| ((*path).to_owned(), Cow::Borrowed(*bytes)))
            .collect(),
    })
});

static OFFICIAL: LazyLock<Arc<Bundled>> = LazyLock::new(|| {
    Arc::new(Bundled {
        files: generated::OFFICIAL_FILES
            .iter()
            .map(|(path, bytes)| ((*path).to_owned(), Cow::Borrowed(*bytes)))
            .collect(),
    })
});

impl Bundled {
    /// The official optional catalog shipped with this binary
    /// (`official/<category>/<name>`; installable, never active until
    /// installed).
    #[must_use]
    pub fn official() -> Arc<Self> {
        Arc::clone(&OFFICIAL)
    }

    /// The tree shipped with this binary.
    #[must_use]
    pub fn builtin() -> Arc<Self> {
        Arc::clone(&BUILTIN)
    }

    /// A tree of `(relative path, content)` files.
    #[must_use]
    pub fn from_files<P: Into<String>, C: Into<Vec<u8>>>(
        files: impl IntoIterator<Item = (P, C)>,
    ) -> Self {
        Self {
            files: files
                .into_iter()
                .map(|(p, c)| (p.into(), Cow::Owned(c.into())))
                .collect(),
        }
    }

    /// Every file path, sorted.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// The files below directory `dir` (`""` = all) as paths relative to
    /// it.
    #[must_use]
    pub fn files_in(&self, dir: &str) -> Vec<String> {
        self.files
            .keys()
            .filter_map(|p| relative(p, dir))
            .map(str::to_owned)
            .collect()
    }

    /// The content of `dir/rel`.
    #[must_use]
    pub fn read(&self, dir: &str, rel: &str) -> Option<&[u8]> {
        let path = if dir.is_empty() {
            rel.to_owned()
        } else {
            format!("{dir}/{rel}")
        };
        self.files.get(&path).map(AsRef::as_ref)
    }

    /// The subtree of the skill directories named `dir_name` (a bundled
    /// skill's directory is named after the skill): what an opted-out
    /// profile still sees of the bundled tier.
    #[must_use]
    pub fn only_skill(&self, dir_name: &str) -> Self {
        Self {
            files: self
                .files
                .iter()
                .filter(|(path, _)| {
                    let mut dirs = path.split('/').rev().skip(1);
                    dirs.any(|d| d == dir_name)
                })
                .map(|(p, c)| (p.clone(), c.clone()))
                .collect(),
        }
    }

    /// The content digest of directory `dir` ([`digest::tree`]).
    #[must_use]
    pub fn digest(&self, dir: &str) -> String {
        digest::tree(
            self.files
                .iter()
                .filter_map(|(p, c)| relative(p, dir).map(|r| (r.to_owned(), c.as_ref()))),
        )
    }
}

fn relative<'a>(path: &'a str, dir: &str) -> Option<&'a str> {
    if dir.is_empty() {
        return Some(path);
    }
    path.strip_prefix(dir)?.strip_prefix('/')
}
