//! Themes are data (spec: cli, Themes are data): four built-in palettes,
//! user themes in `<profile>/themes/<name>.toml` that may extend a
//! built-in and override part of it, and colour switched off by
//! `--no-color`, `NO_COLOR` or a stdout that is not a terminal.
//!
//! ```toml
//! extends = "slate"
//! [colors]
//! prompt = "#88c0d0"
//! error = "bright_red"
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// What the line interface colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// The input prompt.
    Prompt,
    /// The assistant's answer.
    Assistant,
    /// Tool progress lines.
    Tool,
    /// Notices and slash command output.
    Notice,
    /// Errors.
    Error,
    /// Titles and highlights.
    Accent,
}

const ROLES: &[(&str, Role)] = &[
    ("prompt", Role::Prompt),
    ("assistant", Role::Assistant),
    ("tool", Role::Tool),
    ("notice", Role::Notice),
    ("error", Role::Error),
    ("accent", Role::Accent),
];

/// The built-in theme names.
pub const BUILTIN: &[&str] = &["default", "mono", "daylight", "slate"];

/// A resolved theme: an SGR sequence per role (empty: no colour).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// Its name.
    pub name: String,
    colors: BTreeMap<Role, String>,
}

fn builtin(name: &str) -> Option<Theme> {
    let palette: &[(Role, &str)] = match name {
        "default" => &[
            (Role::Prompt, "bold cyan"),
            (Role::Tool, "bright_black"),
            (Role::Notice, "yellow"),
            (Role::Error, "red"),
            (Role::Accent, "magenta"),
        ],
        "mono" => &[(Role::Prompt, "bold"), (Role::Error, "bold")],
        "daylight" => &[
            (Role::Prompt, "bold blue"),
            (Role::Tool, "black"),
            (Role::Notice, "magenta"),
            (Role::Error, "red"),
            (Role::Accent, "blue"),
        ],
        "slate" => &[
            (Role::Prompt, "bold #8fa1b3"),
            (Role::Assistant, "#c0c5ce"),
            (Role::Tool, "#65737e"),
            (Role::Notice, "#ebcb8b"),
            (Role::Error, "#bf616a"),
            (Role::Accent, "#b48ead"),
        ],
        _ => return None,
    };
    let colors = palette
        .iter()
        .map(|(role, spec)| (*role, sgr(spec).unwrap_or_default()))
        .collect();
    Some(Theme {
        name: name.to_owned(),
        colors,
    })
}

/// A user theme file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserTheme {
    #[serde(default)]
    extends: Option<String>,
    #[serde(default)]
    colors: BTreeMap<String, String>,
}

impl Theme {
    /// The theme named `name` for the profile at `profile_dir`: a
    /// built-in, else `<profile>/themes/<name>.toml`.
    ///
    /// # Errors
    ///
    /// Why it cannot be used (unknown, unreadable or invalid).
    pub fn load(profile_dir: &Path, name: &str) -> Result<Self, String> {
        if let Some(theme) = builtin(name) {
            return Ok(theme);
        }
        let path = dir(profile_dir).join(format!("{name}.toml"));
        let text = std::fs::read_to_string(&path).map_err(|_| format!("no theme '{name}'"))?;
        let file: UserTheme =
            toml::from_str(&text).map_err(|e| format!("theme '{name}': {}", e.message()))?;
        let base = file.extends.as_deref().unwrap_or("default");
        let mut theme =
            builtin(base).ok_or_else(|| format!("theme '{name}': unknown base '{base}'"))?;
        name.clone_into(&mut theme.name);
        for (key, spec) in &file.colors {
            let role = ROLES
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, r)| *r)
                .ok_or_else(|| format!("theme '{name}': unknown colour key '{key}'"))?;
            let code =
                sgr(spec).ok_or_else(|| format!("theme '{name}': unknown colour '{spec}'"))?;
            theme.colors.insert(role, code);
        }
        Ok(theme)
    }

    /// `text` in `role`'s colour, or unchanged when colour is off.
    #[must_use]
    pub fn paint(&self, role: Role, text: &str, color: bool) -> String {
        match self.colors.get(&role) {
            Some(code) if color && !code.is_empty() => format!("\x1b[{code}m{text}\x1b[0m"),
            _ => text.to_owned(),
        }
    }
}

fn dir(profile_dir: &Path) -> std::path::PathBuf {
    profile_dir.join("themes")
}

/// Every theme the profile can use: the built-ins, then the user themes
/// (a user file named like a built-in is ignored with a WARN).
#[must_use]
pub fn available(profile_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = BUILTIN.iter().map(|n| (*n).to_owned()).collect();
    let Ok(entries) = std::fs::read_dir(dir(profile_dir)) else {
        return names;
    };
    let mut user: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let stem = path.file_stem()?.to_string_lossy().into_owned();
            (path.extension()? == "toml").then_some(stem)
        })
        .collect();
    user.sort();
    for name in user {
        if BUILTIN.contains(&name.as_str()) {
            tracing::warn!(theme = %name, "user theme named like a built-in is ignored");
        } else {
            names.push(name);
        }
    }
    names
}

/// Whether to colour: never with `--no-color`, a set `NO_COLOR` or a
/// stdout that is not a terminal.
#[must_use]
pub fn color_enabled(no_color_flag: bool, no_color_env: bool, stdout_terminal: bool) -> bool {
    !no_color_flag && !no_color_env && stdout_terminal
}

/// The SGR parameters of `spec`: space-separated `bold`, `dim`,
/// `italic`, `underline`, a named colour, `bright_<colour>` or `#rrggbb`.
fn sgr(spec: &str) -> Option<String> {
    let mut codes = Vec::new();
    for word in spec.split_whitespace() {
        codes.push(match word {
            "none" | "default" => continue,
            "bold" => "1".to_owned(),
            "dim" => "2".to_owned(),
            "italic" => "3".to_owned(),
            "underline" => "4".to_owned(),
            hex if hex.starts_with('#') => truecolor(hex)?,
            named => named_color(named)?,
        });
    }
    Some(codes.join(";"))
}

fn named_color(name: &str) -> Option<String> {
    const NAMES: [&str; 8] = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ];
    let (bright, base) = match name.strip_prefix("bright_") {
        Some(base) => (true, base),
        None => (false, name),
    };
    let index = NAMES.iter().position(|n| *n == base)?;
    Some((if bright { 90 } else { 30 } + index).to_string())
}

fn truecolor(hex: &str) -> Option<String> {
    let digits = hex.strip_prefix('#').filter(|d| d.len() == 6)?;
    let channel = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).ok();
    Some(format!(
        "38;2;{};{};{}",
        channel(0)?,
        channel(2)?,
        channel(4)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_theme_extends_a_built_in_with_partial_overrides() {
        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        std::fs::create_dir_all(dir.path().join("themes")).unwrap();
        std::fs::write(
            dir.path().join("themes/neon.toml"),
            "extends = \"slate\"\n[colors]\nprompt = \"bright_green\"\n",
        )
        .unwrap();
        let theme = Theme::load(dir.path(), "neon").unwrap();
        let slate = Theme::load(dir.path(), "slate").unwrap();
        assert_eq!(theme.paint(Role::Prompt, "x", true), "\x1b[92mx\x1b[0m");
        assert_eq!(
            theme.paint(Role::Error, "x", true),
            slate.paint(Role::Error, "x", true)
        );
        assert_eq!(theme.paint(Role::Prompt, "x", false), "x");
        assert_eq!(
            available(dir.path()),
            ["default", "mono", "daylight", "slate", "neon"]
        );
    }

    #[test]
    fn a_user_theme_named_like_a_built_in_is_ignored() {
        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        std::fs::create_dir_all(dir.path().join("themes")).unwrap();
        std::fs::write(
            dir.path().join("themes/mono.toml"),
            "[colors]\nprompt = \"red\"\n",
        )
        .unwrap();
        assert_eq!(available(dir.path()), BUILTIN);
        assert_eq!(
            Theme::load(dir.path(), "mono").unwrap(),
            builtin("mono").unwrap()
        );
    }

    #[test]
    fn unknown_themes_and_colours_are_refused() {
        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        assert_eq!(
            Theme::load(dir.path(), "neon").unwrap_err(),
            "no theme 'neon'"
        );
        std::fs::create_dir_all(dir.path().join("themes")).unwrap();
        std::fs::write(
            dir.path().join("themes/bad.toml"),
            "[colors]\nprompt = \"plaid\"\n",
        )
        .unwrap();
        assert!(
            Theme::load(dir.path(), "bad")
                .unwrap_err()
                .contains("unknown colour 'plaid'")
        );
    }

    #[test]
    fn colour_is_off_for_the_flag_the_variable_or_a_pipe() {
        assert!(color_enabled(false, false, true));
        assert!(!color_enabled(true, false, true));
        assert!(!color_enabled(false, true, true));
        assert!(!color_enabled(false, false, false));
    }
}
