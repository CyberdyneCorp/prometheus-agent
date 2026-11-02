//! The busy indicator (spec: tui, Busy indicator styles): while a turn
//! runs the status line leads with a glyph of the `ui.tui.busy_indicator`
//! style that changes every [`PERIOD`]. Every glyph of a style has the same
//! display width, so the rest of the line never shifts; under
//! `ui.tui.reduced_motion` the first glyph stays put.

use std::time::Duration;

use pmt_config::schema::BusyIndicator;

/// How long each glyph shows.
pub const PERIOD: Duration = Duration::from_millis(2500);

/// The style names, as `/indicator` takes them.
pub const NAMES: &[&str] = &["kaomoji", "emoji", "unicode", "ascii"];

/// The glyphs of `style`, in order.
#[must_use]
pub const fn frames(style: BusyIndicator) -> &'static [&'static str] {
    match style {
        BusyIndicator::Kaomoji => &["(•_•)", "(•o•)", "(•‿•)", "(-_-)"],
        BusyIndicator::Emoji => &["🌑", "🌓", "🌕", "🌗"],
        BusyIndicator::Unicode => &["◐", "◓", "◑", "◒"],
        BusyIndicator::Ascii => &["|", "/", "-", "\\"],
    }
}

/// The glyph shown `elapsed` into a busy period.
#[must_use]
pub fn glyph(style: BusyIndicator, elapsed: Duration, reduced_motion: bool) -> &'static str {
    let frames = frames(style);
    if reduced_motion {
        return frames[0];
    }
    let step = elapsed.as_millis() / PERIOD.as_millis();
    let index = usize::try_from(step % frames.len() as u128).unwrap_or(0);
    frames[index]
}

/// The style a `/indicator` argument names.
///
/// # Errors
///
/// `unknown indicator '<x>' (kaomoji, emoji, unicode, ascii)`.
pub fn parse(name: &str) -> Result<BusyIndicator, String> {
    match name {
        "kaomoji" => Ok(BusyIndicator::Kaomoji),
        "emoji" => Ok(BusyIndicator::Emoji),
        "unicode" => Ok(BusyIndicator::Unicode),
        "ascii" => Ok(BusyIndicator::Ascii),
        other => Err(format!(
            "unknown indicator '{other}' ({})",
            NAMES.join(", ")
        )),
    }
}

/// The name of `style`.
#[must_use]
pub const fn name(style: BusyIndicator) -> &'static str {
    match style {
        BusyIndicator::Kaomoji => "kaomoji",
        BusyIndicator::Emoji => "emoji",
        BusyIndicator::Unicode => "unicode",
        BusyIndicator::Ascii => "ascii",
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pmt_config::schema::BusyIndicator;
    use unicode_width::UnicodeWidthStr as _;

    use super::{PERIOD, frames, glyph, name, parse};

    const ALL: [BusyIndicator; 4] = [
        BusyIndicator::Kaomoji,
        BusyIndicator::Emoji,
        BusyIndicator::Unicode,
        BusyIndicator::Ascii,
    ];

    #[test]
    fn every_glyph_of_a_style_has_the_same_width() {
        for style in ALL {
            let widths: Vec<usize> = frames(style).iter().map(|g| g.width()).collect();
            assert!(
                widths.iter().all(|w| *w == widths[0]),
                "{style:?} {widths:?}"
            );
        }
    }

    #[test]
    fn the_glyph_changes_every_period_unless_motion_is_reduced() {
        let style = BusyIndicator::Ascii;
        assert_eq!(glyph(style, Duration::ZERO, false), "|");
        assert_eq!(glyph(style, Duration::from_millis(2499), false), "|");
        assert_eq!(glyph(style, PERIOD, false), "/");
        assert_eq!(glyph(style, PERIOD * 5, false), "/");
        assert_eq!(glyph(style, PERIOD * 3, true), "|");
    }

    #[test]
    fn styles_parse_by_name_and_an_unknown_one_lists_the_four() {
        for style in ALL {
            assert_eq!(parse(name(style)), Ok(style));
        }
        assert_eq!(
            parse("dots"),
            Err("unknown indicator 'dots' (kaomoji, emoji, unicode, ascii)".to_owned())
        );
    }
}
