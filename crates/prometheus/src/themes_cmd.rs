//! `prometheus themes list|use|set|show` (spec: cli, Theme commands and
//! branding keys): the profile's themes, the one in use (`display.theme`)
//! and edits of `<profile>/themes/custom.toml`, a user theme that extends
//! the active one. Every write is checked by loading the theme it makes; a
//! theme that does not load is never left behind.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use serde_json::json;

use crate::chat::theme::{self, BUILTIN, Theme};
use crate::context::Context;

/// The theme `set` edits.
pub const CUSTOM: &str = "custom";

/// `prometheus themes` subcommands.
#[derive(Debug, Subcommand)]
pub enum ThemesCommand {
    /// List the themes (the one in use is marked).
    #[command(visible_alias = "ls")]
    List,
    /// Use a theme from now on (`display.theme`).
    Use {
        /// The theme.
        name: String,
    },
    /// Set one key of the custom theme (`colors.<role>`, `spinner.frames`,
    /// `spinner.phrases`, `branding.agent_name`, `branding.prompt_symbol`,
    /// `branding.banner`) and use it; lists take a TOML array or a
    /// comma-separated value.
    Set {
        /// The key.
        key: String,
        /// Its value.
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Show the theme in use: its base, spinner and branding.
    Show,
}

/// Runs `prometheus themes`.
///
/// # Errors
///
/// `config.invalid` for an unknown theme or an invalid key or value;
/// configuration write errors.
pub fn run(command: &ThemesCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    match command {
        ThemesCommand::List => list(&scope, ctx.json),
        ThemesCommand::Use { name } => use_theme(&scope, name),
        ThemesCommand::Set { key, value } => set(&scope, key, value),
        ThemesCommand::Show => show(&scope, ctx.json),
    }
}

fn invalid(message: String) -> Error {
    Error::new(&codes::CONFIG_INVALID, message)
}

fn list(scope: &ProfileScope, json: bool) -> Result<u8, Error> {
    let active = &scope.config().display.theme;
    let names = theme::available(scope.dir());
    if json {
        return crate::output::json(&json!({"themes": names, "active": active}));
    }
    let lines: Vec<String> = names
        .iter()
        .map(|n| {
            if n == active {
                format!("{n} (in use)")
            } else {
                n.clone()
            }
        })
        .collect();
    crate::output::print(&lines.join("\n"))
}

fn use_theme(scope: &ProfileScope, name: &str) -> Result<u8, Error> {
    let available = theme::available(scope.dir());
    if !available.iter().any(|n| n == name) {
        return Err(invalid(format!(
            "no theme '{name}' (available: {})",
            available.join(", ")
        )));
    }
    Theme::load(scope.dir(), name).map_err(invalid)?;
    scope.profile().config_file().set("display.theme", name)?;
    eprintln!("theme '{name}' in use");
    Ok(0)
}

fn show(scope: &ProfileScope, json: bool) -> Result<u8, Error> {
    let name = &scope.config().display.theme;
    let theme = Theme::load(scope.dir(), name).map_err(invalid)?;
    if json {
        return crate::output::json(&json!({
            "name": theme.name,
            "extends": theme.base,
            "spinner": theme.spinner,
            "branding": theme.branding,
        }));
    }
    let mut text = format!("theme: {}\nextends: {}", theme.name, theme.base);
    let spinner = &theme.spinner;
    if !spinner.frames.is_empty() {
        let _ = write!(text, "\nspinner.frames: {}", spinner.frames.join(" "));
    }
    if !spinner.phrases.is_empty() {
        let _ = write!(text, "\nspinner.phrases: {}", spinner.phrases.join(", "));
    }
    let branding = &theme.branding;
    for (key, value) in [
        ("branding.agent_name", &branding.agent_name),
        ("branding.prompt_symbol", &branding.prompt_symbol),
    ] {
        if let Some(value) = value {
            let _ = write!(text, "\n{key}: {value}");
        }
    }
    if let Some(banner) = &branding.banner {
        let _ = write!(text, "\nbranding.banner:\n{banner}");
    }
    crate::output::print(&text)
}

/// The keys `set` takes: `(section, key, is a list)`.
fn parse_key(key: &str) -> Result<(&str, &str, bool), Error> {
    let (section, name) = key.split_once('.').ok_or_else(|| unknown_key(key))?;
    let list = match (section, name) {
        ("colors", _) if !name.is_empty() => false,
        ("spinner", "frames" | "phrases") => true,
        ("branding", "agent_name" | "prompt_symbol" | "banner") => false,
        _ => return Err(unknown_key(key)),
    };
    Ok((section, name, list))
}

fn unknown_key(key: &str) -> Error {
    invalid(format!(
        "unknown theme key '{key}' (colors.<role>, spinner.frames, spinner.phrases, branding.agent_name, branding.prompt_symbol, branding.banner)"
    ))
}

/// A list value: a TOML array of strings, else comma-separated words.
fn list_value(value: &str) -> Result<toml::Value, Error> {
    let trimmed = value.trim();
    if trimmed.starts_with('[') {
        let parsed: toml::Table = toml::from_str(&format!("v = {trimmed}"))
            .map_err(|e| invalid(format!("not a list of strings: {}", e.message())))?;
        return Ok(parsed["v"].clone());
    }
    let items = trimmed
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| toml::Value::String(s.to_owned()))
        .collect();
    Ok(toml::Value::Array(items))
}

/// The table the custom theme starts from: itself while it is in use,
/// else one extending the active theme (a user theme's file is copied,
/// since only built-ins can be extended).
fn starting_table(scope: &ProfileScope) -> Result<toml::Table, Error> {
    let active = scope.config().display.theme.as_str();
    if BUILTIN.contains(&active) {
        let mut table = toml::Table::new();
        table.insert("extends".to_owned(), toml::Value::String(active.to_owned()));
        return Ok(table);
    }
    let path = file(scope.dir(), active);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    toml::from_str(&text).map_err(|e| invalid(format!("{}: {}", path.display(), e.message())))
}

fn file(profile_dir: &Path, name: &str) -> PathBuf {
    theme::dir(profile_dir).join(format!("{name}.toml"))
}

fn set(scope: &ProfileScope, key: &str, value: &str) -> Result<u8, Error> {
    let (section, name, list) = parse_key(key)?;
    let mut table = starting_table(scope)?;
    let value = if list {
        list_value(value)?
    } else {
        toml::Value::String(value.to_owned())
    };
    let entry = table
        .entry(section.to_owned())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let Some(section_table) = entry.as_table_mut() else {
        return Err(invalid(format!(
            "the custom theme's '{section}' is not a table"
        )));
    };
    section_table.insert(name.to_owned(), value);
    let path = file(scope.dir(), CUSTOM);
    let previous = std::fs::read(&path).ok();
    let write = |bytes: &[u8]| {
        std::fs::create_dir_all(theme::dir(scope.dir()))
            .and_then(|()| std::fs::write(&path, bytes))
            .map_err(|e| {
                Error::new(
                    &codes::OUTPUT_IO,
                    format!("cannot write {}: {e}", path.display()),
                )
            })
    };
    write(toml::to_string(&table).unwrap_or_default().as_bytes())?;
    if let Err(why) = Theme::load(scope.dir(), CUSTOM) {
        // Put back what was there: a theme that does not load is never kept.
        match &previous {
            Some(bytes) => write(bytes)?,
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
        return Err(invalid(why));
    }
    scope.profile().config_file().set("display.theme", CUSTOM)?;
    eprintln!("{key} set in theme '{CUSTOM}' (now in use)");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_list_values_parse() {
        assert_eq!(
            parse_key("branding.prompt_symbol").unwrap(),
            ("branding", "prompt_symbol", false)
        );
        assert_eq!(
            parse_key("spinner.frames").unwrap(),
            ("spinner", "frames", true)
        );
        assert_eq!(
            parse_key("colors.prompt").unwrap(),
            ("colors", "prompt", false)
        );
        for bad in ["branding", "branding.logo", "desktop.css", "spinner.speed"] {
            assert!(parse_key(bad).is_err(), "{bad}");
        }
        assert_eq!(
            list_value("a, b,c").unwrap(),
            toml::Value::Array(vec!["a".into(), "b".into(), "c".into()])
        );
        assert_eq!(
            list_value("[\"x, y\", \"z\"]").unwrap(),
            toml::Value::Array(vec!["x, y".into(), "z".into()])
        );
    }
}
