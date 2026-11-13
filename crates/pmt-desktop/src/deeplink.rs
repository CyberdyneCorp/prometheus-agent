//! `prometheus://` deep links (spec: desktop-app, Deep links). Exactly four
//! shapes are understood; anything else is ignored with a WARN log. A link
//! only ever opens something: it never carries input to submit or an
//! answer to a request, so extra parameters (a `prompt`, say) are dropped.

use serde::Serialize;
use url::Url;

/// The scheme the desktop registers.
pub const SCHEME: &str = "prometheus";

/// What a deep link opens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "open", rename_all = "snake_case")]
pub enum DeepLink {
    /// `prometheus://session/<id>`: that session, in the profile that owns
    /// it.
    Session {
        /// `ses_<ULID>`.
        session_id: String,
    },
    /// `prometheus://new?profile=<name>[&cwd=<path>]`: an empty draft.
    New {
        /// The profile, when given.
        #[serde(skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        /// The draft's working directory, when given.
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// `prometheus://plugins/install?id=<catalog id>`: the install flow.
    InstallPlugin {
        /// The catalog id as given (the install flow checks it).
        id: String,
    },
    /// `prometheus://skills/install?id=<catalog id>`: the install flow.
    InstallSkill {
        /// The catalog id as given.
        id: String,
    },
}

/// Parses a link the OS handed to the app; `None` (logged at WARN) for
/// anything that is not one of the four shapes.
#[must_use]
pub fn parse(raw: &str) -> Option<DeepLink> {
    let parsed = Url::parse(raw.trim())
        .ok()
        .filter(|u| u.scheme() == SCHEME)
        .and_then(|url| shape(&url));
    if parsed.is_none() {
        // The link may carry user text: log its shape, not its query.
        let shown = raw.split(['?', '#']).next().unwrap_or_default();
        tracing::warn!(link = shown, "ignored an unknown prometheus:// link");
    }
    parsed
}

fn shape(url: &Url) -> Option<DeepLink> {
    let query = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
            .filter(|v| !v.is_empty())
    };
    let path = url.path().trim_end_matches('/');
    match (url.host_str()?, path) {
        ("session", id) => {
            let id = id.strip_prefix('/')?;
            valid_session_id(id).then(|| DeepLink::Session {
                session_id: id.to_owned(),
            })
        }
        ("new", "") => Some(DeepLink::New {
            profile: query("profile"),
            cwd: query("cwd"),
        }),
        ("plugins", "/install") => query("id").map(|id| DeepLink::InstallPlugin { id }),
        ("skills", "/install") => query("id").map(|id| DeepLink::InstallSkill { id }),
        _ => None,
    }
}

/// `ses_` and 26 Crockford base32 characters.
fn valid_session_id(id: &str) -> bool {
    id.strip_prefix("ses_")
        .is_some_and(|ulid| ulid.len() == 26 && ulid.chars().all(|c| c.is_ascii_alphanumeric()))
}

#[cfg(test)]
mod tests {
    use super::{DeepLink, parse};

    #[test]
    fn a_prompt_payload_is_dropped() {
        assert_eq!(
            parse("prometheus://new?profile=work&prompt=rm%20-rf"),
            Some(DeepLink::New {
                profile: Some("work".into()),
                cwd: None
            })
        );
    }

    #[test]
    fn the_four_shapes_parse_and_nothing_else() {
        assert_eq!(
            parse("prometheus://session/ses_01J00000000000000000000000"),
            Some(DeepLink::Session {
                session_id: "ses_01J00000000000000000000000".into()
            })
        );
        assert_eq!(
            parse("prometheus://new?profile=a&cwd=%2Fsrc%2Fapi"),
            Some(DeepLink::New {
                profile: Some("a".into()),
                cwd: Some("/src/api".into())
            })
        );
        assert_eq!(
            parse("prometheus://plugins/install?id=kanban-extras"),
            Some(DeepLink::InstallPlugin {
                id: "kanban-extras".into()
            })
        );
        assert_eq!(
            parse("prometheus://skills/install?id=x"),
            Some(DeepLink::InstallSkill { id: "x".into() })
        );
        for ignored in [
            "prometheus://session/../../etc",
            "prometheus://session/",
            "prometheus://settings",
            "prometheus://plugins/install",
            "https://example.com/new",
            "garbage",
        ] {
            assert_eq!(parse(ignored), None, "{ignored}");
        }
    }
}
