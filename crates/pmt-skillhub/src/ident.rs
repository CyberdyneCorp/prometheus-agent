//! Hub source identifiers (spec: skills, Hub source identifiers): each
//! form maps to one source adapter.

use std::fmt;

use pmt_core::error::{Error, codes};

/// The forms, for the refusal of an identifier matching none.
pub const FORMS: &[&str] = &[
    "official/<category>/<name>",
    "skills-sh/<owner>/<repo>/<skill>",
    "well-known:<https url>",
    "<owner>/<repo>/<path> or github:<owner>/<repo>/<path>",
    "clawhub:<slug>",
    "lobehub:<slug>",
    "browse-sh:<slug>",
    "https://.../SKILL.md",
];

/// A parsed identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Identifier {
    /// The embedded official catalog.
    Official {
        /// The category directory.
        category: String,
        /// The skill.
        name: String,
    },
    /// The skills.sh directory (a GitHub repository underneath).
    SkillsSh {
        /// Repository owner.
        owner: String,
        /// Repository name.
        repo: String,
        /// The skill (alias slugs allowed).
        skill: String,
    },
    /// A site's `/.well-known/skills/` endpoint.
    WellKnown {
        /// `https://<site>/.well-known/skills/<name>` (or the site, or
        /// `.../index.json#<name>`).
        url: String,
    },
    /// A GitHub repository path (directly or through a tap).
    GitHub {
        /// Repository owner.
        owner: String,
        /// Repository name.
        repo: String,
        /// The skill's path in the repository (a tap's skill name).
        path: String,
    },
    /// ClawHub.
    ClawHub {
        /// The slug (`<owner>/<slug>` allowed).
        slug: String,
    },
    /// LobeHub's agent marketplace.
    LobeHub {
        /// The agent id.
        slug: String,
    },
    /// browse.sh.
    BrowseSh {
        /// The slug (`<host>/<skill>`).
        slug: String,
    },
    /// A direct `SKILL.md` URL.
    Url {
        /// The URL.
        url: String,
    },
}

fn usage(text: &str) -> Error {
    Error::new(
        &codes::CLI_USAGE,
        format!(
            "unrecognized skill identifier '{text}'; use one of: {}",
            FORMS.join(", ")
        ),
    )
}

fn segment_ok(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn path_ok(path: &str) -> bool {
    !path.is_empty() && path.split('/').all(segment_ok)
}

fn https(url: &str) -> bool {
    url.starts_with("https://") || (cfg!(debug_assertions) && url.starts_with("http://127.0.0.1"))
}

impl Identifier {
    /// Parses `text`.
    ///
    /// # Errors
    ///
    /// `cli.usage` (exit 2): `unrecognized skill identifier '<id>'` with
    /// the forms.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let text = text.trim();
        parse(text).ok_or_else(|| usage(text))
    }

    /// The canonical text (what the lockfile records as `source`).
    #[must_use]
    pub fn canonical(&self) -> String {
        self.to_string()
    }

    /// The adapter's name.
    #[must_use]
    pub fn source(&self) -> &'static str {
        match self {
            Self::Official { .. } => "official",
            Self::SkillsSh { .. } => "skills-sh",
            Self::WellKnown { .. } => "well-known",
            Self::GitHub { .. } => "github",
            Self::ClawHub { .. } => "clawhub",
            Self::LobeHub { .. } => "lobehub",
            Self::BrowseSh { .. } => "browse-sh",
            Self::Url { .. } => "url",
        }
    }

    /// The GitHub repository behind it (`owner/repo`), when there is one.
    #[must_use]
    pub fn repository(&self) -> Option<String> {
        match self {
            Self::SkillsSh { owner, repo, .. } | Self::GitHub { owner, repo, .. } => {
                Some(format!("{owner}/{repo}"))
            }
            _ => None,
        }
    }
}

fn parse(text: &str) -> Option<Identifier> {
    if let Some(rest) = text.strip_prefix("official/") {
        let (category, name) = rest.split_once('/')?;
        return (segment_ok(category) && segment_ok(name)).then(|| Identifier::Official {
            category: category.to_owned(),
            name: name.to_owned(),
        });
    }
    if let Some(rest) = text.strip_prefix("skills-sh/") {
        let mut parts = rest.splitn(3, '/');
        let (owner, repo, skill) = (parts.next()?, parts.next()?, parts.next()?);
        return [owner, repo, skill]
            .iter()
            .all(|p| segment_ok(p))
            .then(|| Identifier::SkillsSh {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                skill: skill.to_owned(),
            });
    }
    if let Some(url) = text.strip_prefix("well-known:") {
        return https(url).then(|| Identifier::WellKnown {
            url: url.trim_end_matches('/').to_owned(),
        });
    }
    for (prefix, make) in SLUG_FORMS {
        if let Some(slug) = text.strip_prefix(prefix) {
            return path_ok(slug).then(|| make(slug.to_owned()));
        }
    }
    if https(text) {
        let path = text.split(['?', '#']).next().unwrap_or(text);
        return std::path::Path::new(path)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("md"))
            .then(|| Identifier::Url {
                url: text.to_owned(),
            });
    }
    let rest = text.strip_prefix("github:").unwrap_or(text);
    let mut parts = rest.splitn(3, '/');
    let (owner, repo, path) = (parts.next()?, parts.next()?, parts.next()?);
    let path = path.trim_end_matches('/');
    (segment_ok(owner) && segment_ok(repo) && path_ok(path)).then(|| Identifier::GitHub {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        path: path.to_owned(),
    })
}

type SlugForm = (&'static str, fn(String) -> Identifier);

const SLUG_FORMS: [SlugForm; 3] = [
    ("clawhub:", |slug| Identifier::ClawHub { slug }),
    ("lobehub:", |slug| Identifier::LobeHub { slug }),
    ("browse-sh:", |slug| Identifier::BrowseSh { slug }),
];

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Official { category, name } => write!(f, "official/{category}/{name}"),
            Self::SkillsSh { owner, repo, skill } => write!(f, "skills-sh/{owner}/{repo}/{skill}"),
            Self::WellKnown { url } => write!(f, "well-known:{url}"),
            Self::GitHub { owner, repo, path } => write!(f, "{owner}/{repo}/{path}"),
            Self::ClawHub { slug } => write!(f, "clawhub:{slug}"),
            Self::LobeHub { slug } => write!(f, "lobehub:{slug}"),
            Self::BrowseSh { slug } => write!(f, "browse-sh:{slug}"),
            Self::Url { url } => f.write_str(url),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_form_maps_to_its_adapter() {
        for (text, source) in [
            ("official/mlops/axolotl", "official"),
            ("skills-sh/vercel-labs/agent-skills/react", "skills-sh"),
            ("well-known:https://example.com", "well-known"),
            ("openai/skills/k8s", "github"),
            ("github:my-org/agent-skills/skills/deploy", "github"),
            ("clawhub:weather", "clawhub"),
            ("lobehub:travel-planner", "lobehub"),
            ("browse-sh:airbnb.com/search", "browse-sh"),
            ("https://example.com/x/SKILL.md", "url"),
        ] {
            let id = Identifier::parse(text).unwrap();
            assert_eq!(id.source(), source, "{text}");
            if source != "github" || !text.starts_with("github:") {
                assert_eq!(id.canonical(), text);
            }
        }
    }

    #[test]
    fn an_unknown_form_is_a_usage_error_listing_the_forms() {
        for text in [
            "nonsense",
            "a/b",
            "ftp://x/SKILL.md",
            "owner/repo/../etc",
            "clawhub:",
        ] {
            let err = Identifier::parse(text).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{text}");
            assert!(
                err.message()
                    .contains(&format!("unrecognized skill identifier '{text}'")),
                "{}",
                err.message()
            );
            assert!(err.message().contains("clawhub:<slug>"));
        }
    }
}
