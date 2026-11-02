//! The checks of `[gateway.platforms.<name>]` tables that depend on the
//! platform (spec: platform-adapters, First-party platforms and typed
//! platform names; Default first-party message limits; Platform
//! configuration shape): the name must be a known platform, `options`
//! must fit the adapter's published schema, a limit may only be lowered,
//! and a credential is referenced by secret name, never written inline.

use std::ops::Range;

use pmt_core::error::codes;

use crate::platforms;
use crate::validate::{Checker, Issue, Kind, Node};

/// Keys that would hold a credential value in a platform table or its
/// `options`.
const CREDENTIAL_KEYS: &[&str] = &[
    "token",
    "bot_token",
    "app_token",
    "access_token",
    "password",
    "api_key",
    "secret",
    "signing_secret",
    "client_secret",
    "webhook_secret",
];

type Entries = [(String, Option<Range<usize>>, Node)];

fn table<'a>(node: &'a Node, key: &str) -> Option<&'a Node> {
    let Kind::Table(entries) = &node.kind else {
        return None;
    };
    entries.iter().find(|(k, ..)| k == key).map(|(.., n)| n)
}

/// Adds the platform issues of `root` to `issues` (replacing the generic
/// "unknown key" of an inline credential with the credential refusal).
pub(crate) fn check(root: &Node, issues: &mut Vec<Issue>) {
    let Some(Kind::Table(platforms)) = table(root, "gateway")
        .and_then(|g| table(g, "platforms"))
        .map(|n| &n.kind)
    else {
        return;
    };
    for (name, span, node) in platforms {
        let key = format!("gateway.platforms.{name}");
        let lower = name.to_lowercase();
        if !platforms::is_known(&lower) {
            issues.push(Issue {
                code: &codes::CONFIG_INVALID,
                message: format!(
                    "{key}: unknown platform '{lower}'; install an adapter plugin that provides it"
                ),
                key,
                span: span.clone(),
            });
            continue;
        }
        if let Kind::Table(entries) = &node.kind {
            check_table(&lower, &key, entries, issues);
        }
    }
}

fn check_table(platform: &str, key: &str, entries: &Entries, issues: &mut Vec<Issue>) {
    for (name, span, node) in entries {
        let child = format!("{key}.{name}");
        if CREDENTIAL_KEYS.contains(&name.as_str()) {
            issues.retain(|i| i.key != child);
            issues.push(inline_credential(&child, span.clone(), "token_secret"));
        } else if name == "max_message_length" {
            check_limit(platform, &child, node, issues);
        } else if name == "options" {
            check_options(platform, &child, node, issues);
        }
    }
}

fn inline_credential(key: &str, span: Option<Range<usize>>, reference: &str) -> Issue {
    Issue {
        code: &codes::CONFIG_CREDENTIAL_IN_FILE,
        key: key.to_owned(),
        span,
        message: format!(
            "{key}: credentials belong in the secrets store (prometheus secrets set <NAME>) and are referenced by `{reference}`"
        ),
    }
}

/// A user may lower a first-party limit, never raise it.
fn check_limit(platform: &str, key: &str, node: &Node, issues: &mut Vec<Issue>) {
    let (Some(first_party), Kind::Scalar(value, _)) =
        (platforms::first_party(platform), &node.kind)
    else {
        return;
    };
    if value
        .as_u64()
        .is_some_and(|v| v > u64::from(first_party.max_message_length))
    {
        issues.push(Issue {
            code: &codes::CONFIG_INVALID,
            key: key.to_owned(),
            span: node.span.clone(),
            message: format!(
                "{key}: {}'s maximum is {}; a limit can only be lowered",
                first_party.display, first_party.max_message_length
            ),
        });
    }
}

fn check_options(platform: &str, key: &str, node: &Node, issues: &mut Vec<Issue>) {
    if let Kind::Table(entries) = &node.kind {
        for (name, span, _) in entries {
            if CREDENTIAL_KEYS.contains(&name.as_str()) {
                let child = format!("{key}.{name}");
                issues.push(inline_credential(
                    &child,
                    span.clone(),
                    &format!("{name}_secret"),
                ));
            }
        }
    }
    let Some(schema) = platforms::options_schema(platform) else {
        return;
    };
    let mut found = Vec::new();
    Checker { issues: &mut found }.check(node, schema, key);
    // A credential key was already refused above with its own message.
    found.retain(|i| {
        !CREDENTIAL_KEYS
            .iter()
            .any(|c| i.key == format!("{key}.{c}"))
    });
    issues.extend(found);
}

#[cfg(test)]
mod tests {
    use crate::validate::{Node, document};

    fn issues(text: &str) -> Vec<String> {
        let doc: toml_edit::DocumentMut = text.parse().unwrap();
        document(&Node::from_item(doc.as_item()))
            .into_iter()
            .map(|i| i.message)
            .collect()
    }

    #[test]
    fn an_unknown_platform_table_fails_validation() {
        let found = issues("[gateway.platforms.myspace]\nenabled = true\n");
        assert_eq!(
            found,
            vec![
                "gateway.platforms.myspace: unknown platform 'myspace'; install an adapter plugin that provides it"
            ]
        );
        assert!(issues("[gateway.platforms.Telegram]\nenabled = true\n").is_empty());
    }

    #[test]
    fn raising_a_first_party_limit_is_rejected() {
        let found = issues("[gateway.platforms.discord]\nmax_message_length = 4000\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("Discord's maximum is 2000"),
            "{}",
            found[0]
        );
        assert!(issues("[gateway.platforms.discord]\nmax_message_length = 1500\n").is_empty());
    }

    #[test]
    fn an_inline_token_points_to_the_secrets_store() {
        let found = issues("[gateway.platforms.telegram]\ntoken = \"123:ABC\"\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].contains("credentials belong in the secrets store")
                && found[0].contains("token_secret"),
            "{}",
            found[0]
        );
    }

    #[test]
    fn adapter_options_are_validated_against_the_adapter_schema() {
        let found = issues("[gateway.platforms.email.options]\nimap_port = \"abc\"\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].starts_with("gateway.platforms.email.options.imap_port"),
            "{}",
            found[0]
        );
        let found = issues("[gateway.platforms.email.options]\nimap_hots = \"x\"\n");
        assert!(found[0].contains("unknown key"), "{found:?}");
        assert!(issues("[gateway.platforms.email.options]\nimap_port = 993\n").is_empty());
    }

    #[test]
    fn batching_out_of_range_names_the_maximum() {
        let found = issues("[gateway.platforms.telegram.batching]\ndelay_ms = 5000\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("2000"), "{}", found[0]);
    }
}
