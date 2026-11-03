//! Whole-config checks of `[hooks]` (spec: plugin-system, Shell hooks with
//! first-use consent; Outbound webhooks; `transform_api_error_classification`
//! hook): every event is a name of the closed hook registry (and
//! `transform_api_error_classification` is not one shell hooks may take),
//! matchers are valid regular expressions, and each outbound webhook has a
//! unique name and an HTTPS (or loopback HTTP) URL.

use std::collections::BTreeSet;

use pmt_core::error::{Error, codes};

use crate::schema::Config;

/// The hook only provider plugins may answer.
const PLUGIN_ONLY: &str = "transform_api_error_classification";

fn invalid(message: String) -> Error {
    Error::new(&codes::CONFIG_INVALID, message)
}

/// Checks `[hooks]`.
///
/// # Errors
///
/// `config.invalid` naming the entry.
pub fn check(config: &Config) -> Result<(), Error> {
    check_shell(config)?;
    check_outbound(config)
}

fn check_shell(config: &Config) -> Result<(), Error> {
    for (event, hooks) in &config.hooks.events {
        if pmt_core::hooks::check(event).is_err() {
            return Err(invalid(format!("hooks.{event}: unknown hook '{event}'")));
        }
        if event == PLUGIN_ONLY {
            return Err(invalid(format!(
                "hooks.{event}: shell hooks cannot answer {PLUGIN_ONLY}; only plugins can"
            )));
        }
        for (index, hook) in hooks.iter().enumerate() {
            let key = format!("hooks.{event}[{index}]");
            if shell_words(&hook.command).is_none_or(|words| words.is_empty()) {
                return Err(invalid(format!(
                    "{key}.command: empty or unbalanced quotes"
                )));
            }
            matcher(&key, hook.matcher.as_deref())?;
        }
    }
    Ok(())
}

fn check_outbound(config: &Config) -> Result<(), Error> {
    let mut names = BTreeSet::new();
    for (index, hook) in config.hooks.outbound.iter().enumerate() {
        let key = format!("hooks.outbound[{index}] ('{}')", hook.name);
        if hook.name.trim().is_empty() {
            return Err(invalid(format!("hooks.outbound[{index}].name: empty")));
        }
        if !names.insert(hook.name.as_str()) {
            return Err(invalid(format!("{key}: the name is used twice")));
        }
        url(&key, &hook.url)?;
        if hook.events.is_empty() {
            return Err(invalid(format!("{key}.events: empty")));
        }
        if let Some(unknown) = hook
            .events
            .iter()
            .find(|e| pmt_core::hooks::check(e).is_err())
        {
            return Err(invalid(format!("{key}.events: unknown hook '{unknown}'")));
        }
        matcher(&key, hook.matcher.as_deref())?;
    }
    Ok(())
}

fn matcher(key: &str, matcher: Option<&str>) -> Result<(), Error> {
    match matcher.map(regex::Regex::new) {
        Some(Err(err)) => Err(invalid(format!("{key}.matcher: {err}"))),
        _ => Ok(()),
    }
}

fn url(key: &str, url: &str) -> Result<(), Error> {
    let refused = || {
        invalid(format!(
            "{key}.url: '{url}' must be https:// (http:// only to a loopback address)"
        ))
    };
    let (scheme, rest) = url.split_once("://").ok_or_else(refused)?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let host = match host_port.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host_port.split(':').next().unwrap_or_default(),
    };
    if host.is_empty() {
        return Err(refused());
    }
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match scheme.to_ascii_lowercase().as_str() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        _ => Err(refused()),
    }
}

/// `command` split into words like a shell would, without running one:
/// whitespace separates, single quotes keep text literally, double quotes
/// keep text with `\"` and `\\` escapes. `None` for unbalanced quotes.
#[must_use]
pub fn shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
            continue;
        }
        started = true;
        match c {
            '\'' => quoted(&mut chars, '\'', false, &mut word)?,
            '"' => quoted(&mut chars, '"', true, &mut word)?,
            '\\' => word.push(chars.next()?),
            c => word.push(c),
        }
    }
    if started {
        words.push(word);
    }
    Some(words)
}

/// Reads up to the closing `end` into `word` (`escapes`: `\\` keeps the
/// next character); `None` when it never closes.
fn quoted(
    chars: &mut std::str::Chars<'_>,
    end: char,
    escapes: bool,
    word: &mut String,
) -> Option<()> {
    loop {
        match chars.next()? {
            c if c == end => return Some(()),
            '\\' if escapes => word.push(chars.next()?),
            c => word.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn events_must_come_from_the_registry_and_classification_is_plugin_only() {
        let err = check(&config("[[hooks.pre_tool_cal]]\ncommand = \"x\"\n")).unwrap_err();
        assert!(
            err.message().contains("unknown hook 'pre_tool_cal'"),
            "{err}"
        );
        let err = check(&config(
            "[[hooks.transform_api_error_classification]]\ncommand = \"x\"\n",
        ))
        .unwrap_err();
        assert!(err.message().contains("only plugins can"), "{err}");
        assert!(
            check(&config(
                "[[hooks.pre_tool_call]]\ncommand = \"x\"\nmatcher = \"^term\"\n"
            ))
            .is_ok()
        );
    }

    #[test]
    fn a_malformed_outbound_entry_is_named() {
        let err = check(&config(
            "[[hooks.outbound]]\nname = \"ci\"\nurl = \"http://example.com/h\"\nevents = [\"on_session_end\"]\n",
        ))
        .unwrap_err();
        assert!(
            err.message().starts_with("hooks.outbound[0] ('ci').url"),
            "{err}"
        );
        assert!(check(&config(
            "[[hooks.outbound]]\nname = \"ci\"\nurl = \"http://127.0.0.1:9/h\"\nevents = [\"on_session_end\"]\n",
        ))
        .is_ok());
    }

    #[test]
    fn commands_split_like_a_shell_without_one() {
        assert_eq!(
            shell_words(r#"./hook.sh --name "a b" 'c d' e\ f"#).unwrap(),
            ["./hook.sh", "--name", "a b", "c d", "e f"]
        );
        assert!(shell_words("'open").is_none());
    }
}
