//! Dashboard themes (spec: web-dashboard, Dashboard themes): TOML files
//! with a palette, typography, layout, a layout variant, component colour
//! overrides and at most 32 KiB of custom CSS. Seven themes are built in;
//! user themes are discovered from `<home>/themes/dashboard/*.toml` and a
//! theme that does not validate is listed with its reason and never
//! served. Fonts come only from installed or embedded families, and CSS
//! with `@import` or a `url()` to another origin is rejected, so a theme
//! never makes the dashboard fetch anything.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::http::{Reply, bytes, error, json_reply};

/// The largest `custom_css`.
pub(crate) const MAX_CUSTOM_CSS: usize = 32 * 1024;
/// The directory of user themes below the home.
pub(crate) const DIR: &str = "themes/dashboard";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Theme {
    #[serde(default)]
    description: String,
    palette: Palette,
    #[serde(default)]
    typography: Typography,
    #[serde(default)]
    layout: Layout,
    #[serde(default = "standard")]
    layout_variant: String,
    #[serde(default)]
    components: BTreeMap<String, String>,
    #[serde(default)]
    custom_css: String,
}

fn standard() -> String {
    "standard".to_owned()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Palette {
    background: String,
    midground: String,
    foreground: String,
    #[serde(default)]
    warm_glow: Option<String>,
    #[serde(default)]
    noise_opacity: f64,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Typography {
    font_family: Vec<String>,
    base_size: u32,
    line_height: f64,
}

impl Default for Typography {
    fn default() -> Self {
        Self {
            font_family: vec!["system-ui".to_owned(), "sans-serif".to_owned()],
            base_size: 15,
            line_height: 1.5,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Layout {
    radius: u32,
    density: String,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            radius: 8,
            density: "comfortable".to_owned(),
        }
    }
}

/// The built-in themes.
const BUILTIN: &[(&str, &str)] = &[
    (
        "default",
        r##"palette = { background = "#f7f7f8ff", midground = "#ffffffff", foreground = "#1c1d21ff" }"##,
    ),
    (
        "default-large",
        r##"palette = { background = "#f7f7f8ff", midground = "#ffffffff", foreground = "#1c1d21ff" }
typography = { base_size = 18, line_height = 1.6 }
layout = { radius = 10, density = "spacious" }"##,
    ),
    (
        "midnight",
        r##"palette = { background = "#0b1020ff", midground = "#141a2eff", foreground = "#dfe6ffff", warm_glow = "#5b7cfa33" }"##,
    ),
    (
        "ember",
        r##"palette = { background = "#1a1210ff", midground = "#241815ff", foreground = "#f5e6dcff", warm_glow = "#fb923c40", noise_opacity = 0.3 }"##,
    ),
    (
        "mono",
        r##"palette = { background = "#ffffffff", midground = "#f2f2f2ff", foreground = "#000000ff" }
typography = { font_family = ["ui-monospace", "monospace"] }
layout = { radius = 0, density = "compact" }"##,
    ),
    (
        "cyberpunk",
        r##"palette = { background = "#0d0221ff", midground = "#1b0a3cff", foreground = "#f6f930ff", warm_glow = "#ff2a6d55", noise_opacity = 0.6 }
layout_variant = "cockpit"
components = { accent = "#05d9e8ff" }"##,
    ),
    (
        "rose",
        r##"palette = { background = "#fff5f7ff", midground = "#ffffffff", foreground = "#3f1d2bff", warm_glow = "#f43f5e33" }"##,
    ),
];

fn colour(key: &str, value: &str) -> Result<(), String> {
    let hex = value.strip_prefix('#').unwrap_or_default();
    let ok = matches!(hex.len(), 3 | 4 | 6 | 8) && hex.chars().all(|c| c.is_ascii_hexdigit());
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{key}: '{value}' is not a colour such as #1c1d21 or #1c1d21cc"
        ))
    }
}

fn font(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " -_".contains(c));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "typography.font_family: '{name}' is not an installed or embedded font family name (no URLs)"
        ))
    }
}

/// Refuses CSS that would load anything from another origin.
fn check_css(css: &str) -> Result<(), String> {
    if css.len() > MAX_CUSTOM_CSS {
        return Err(format!(
            "custom_css is {} bytes; the limit is 32 KiB ({MAX_CUSTOM_CSS} bytes)",
            css.len()
        ));
    }
    let lower = css.to_ascii_lowercase();
    if lower.contains("@import") {
        return Err("custom_css: @import is not allowed (no remote CSS)".to_owned());
    }
    for (at, _) in lower.match_indices("url(") {
        let target = lower[at + 4..].trim_start().trim_start_matches(['"', '\'']);
        if target.starts_with("http:") || target.starts_with("https:") || target.starts_with("//") {
            return Err("custom_css: url() to another origin is not allowed".to_owned());
        }
    }
    if lower.contains("</style") {
        return Err("custom_css: must not close the style sheet".to_owned());
    }
    Ok(())
}

impl Theme {
    fn validate(&self) -> Result<(), String> {
        colour("palette.background", &self.palette.background)?;
        colour("palette.midground", &self.palette.midground)?;
        colour("palette.foreground", &self.palette.foreground)?;
        if let Some(glow) = &self.palette.warm_glow {
            colour("palette.warm_glow", glow)?;
        }
        if !(0.0..=1.2).contains(&self.palette.noise_opacity) {
            return Err("palette.noise_opacity: must be between 0 and 1.2".to_owned());
        }
        self.typography
            .font_family
            .iter()
            .try_for_each(|f| font(f))?;
        if !(8..=32).contains(&self.typography.base_size) {
            return Err("typography.base_size: must be between 8 and 32".to_owned());
        }
        if !(1.0..=3.0).contains(&self.typography.line_height) {
            return Err("typography.line_height: must be between 1 and 3".to_owned());
        }
        if !["compact", "comfortable", "spacious"].contains(&self.layout.density.as_str()) {
            return Err("layout.density: expected compact, comfortable or spacious".to_owned());
        }
        if !["standard", "cockpit", "tiled"].contains(&self.layout_variant.as_str()) {
            return Err("layout_variant: expected standard, cockpit or tiled".to_owned());
        }
        for (name, value) in &self.components {
            colour(&format!("components.{name}"), value)?;
        }
        check_css(&self.custom_css)
    }

    /// The theme as a style sheet over the dashboard's variables.
    fn css(&self) -> String {
        let p = &self.palette;
        let t = &self.typography;
        let mut css = format!(
            ":root {{ --bg: {}; --panel: {}; --fg: {}; --radius: {}px; --glow: {}; --noise: {}; \
             color-scheme: normal; }}\nbody {{ font-family: {}; font-size: {}px; line-height: {}; }}\n\
             body {{ --density: {}; }}\n",
            p.background,
            p.midground,
            p.foreground,
            self.layout.radius,
            p.warm_glow.as_deref().unwrap_or("transparent"),
            p.noise_opacity,
            t.font_family
                .iter()
                .map(|f| format!("{f:?}"))
                .collect::<Vec<_>>()
                .join(", "),
            t.base_size,
            t.line_height,
            self.layout.density,
        );
        for (name, value) in &self.components {
            let _ = writeln!(css, ":root {{ --{name}: {value}; }}");
        }
        css.push_str(&self.custom_css);
        css
    }
}

/// A theme as listed: its name, origin and whether it validated.
struct Listed {
    name: String,
    builtin: bool,
    theme: Result<Theme, String>,
}

fn parse(text: &str) -> Result<Theme, String> {
    let theme: Theme = toml::from_str(text).map_err(|e| e.message().to_owned())?;
    theme.validate()?;
    Ok(theme)
}

fn all(home: Option<&Path>) -> Vec<Listed> {
    let mut listed: Vec<Listed> = BUILTIN
        .iter()
        .map(|(name, text)| Listed {
            name: (*name).to_owned(),
            builtin: true,
            theme: parse(text),
        })
        .collect();
    let dir = home.map(|h| h.join(DIR));
    let mut user: Vec<(String, std::path::PathBuf)> = dir
        .and_then(|d| std::fs::read_dir(d).ok())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .filter_map(|p| Some((p.file_stem()?.to_string_lossy().into_owned(), p)))
        .collect();
    user.sort();
    for (name, path) in user {
        if listed.iter().any(|l| l.name == name) {
            continue;
        }
        let theme = std::fs::read_to_string(&path)
            .map_err(|e| e.to_string())
            .and_then(|text| parse(&text));
        listed.push(Listed {
            name,
            builtin: false,
            theme,
        });
    }
    listed
}

/// `GET /api/themes`.
pub(crate) fn index(home: Option<&Path>) -> Reply {
    let themes: Vec<Value> = all(home)
        .iter()
        .map(|l| match &l.theme {
            Ok(theme) => json!({"name": l.name, "builtin": l.builtin, "valid": true,
                "description": theme.description, "layout_variant": theme.layout_variant}),
            Err(reason) => {
                json!({"name": l.name, "builtin": l.builtin, "valid": false, "error": reason})
            }
        })
        .collect();
    json_reply(
        200,
        &json!({"themes": themes, "directory": home.map(|h| h.join(DIR))}),
    )
}

/// `GET /api/themes/{name}/css`: only a theme that validates.
pub(crate) fn css(home: Option<&Path>, name: &str) -> Reply {
    match all(home).into_iter().find(|l| l.name == name) {
        Some(Listed {
            theme: Ok(theme), ..
        }) => bytes(200, "text/css; charset=utf-8", theme.css()),
        Some(Listed {
            theme: Err(reason), ..
        }) => error(422, "invalid_theme", &reason),
        None => error(404, "not_found", &format!("no theme {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{BUILTIN, MAX_CUSTOM_CSS, parse};

    #[test]
    fn every_built_in_theme_validates() {
        let names: Vec<&str> = BUILTIN.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            [
                "default",
                "default-large",
                "midnight",
                "ember",
                "mono",
                "cyberpunk",
                "rose"
            ]
        );
        for (name, text) in BUILTIN {
            assert!(parse(text).is_ok(), "{name}: {:?}", parse(text).err());
        }
    }

    const BASE: &str = "palette = { background = \"#000000\", midground = \"#111111\", foreground = \"#ffffff\" }\n";

    /// Scenario: Theme imports remote CSS.
    #[test]
    fn remote_css_is_rejected_with_the_reason() {
        let theme = format!("{BASE}custom_css = \"@import url(https://x.example/a.css);\"\n");
        assert!(parse(&theme).unwrap_err().contains("@import"));
        let theme = format!(
            "{BASE}custom_css = \"body {{ background: url('https://x.example/a.png') }}\"\n"
        );
        assert!(parse(&theme).unwrap_err().contains("another origin"));
        let local = format!(
            "{BASE}custom_css = \"body {{ background: url(data:image/png;base64,AA==) }}\"\n"
        );
        assert!(parse(&local).is_ok());
    }

    /// Scenario: Oversized custom CSS.
    #[test]
    fn custom_css_over_32_kib_is_rejected_naming_the_limit() {
        let css = "a{}".repeat(40 * 1024 / 3);
        assert!(css.len() > MAX_CUSTOM_CSS);
        let theme = format!("{BASE}custom_css = {css:?}\n");
        assert!(parse(&theme).unwrap_err().contains("32 KiB"));
    }
}
