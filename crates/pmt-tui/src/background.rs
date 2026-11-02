//! Terminal background detection (spec: tui, Terminal background
//! detection): with `ui.tui.theme = "auto"` the theme follows `COLORFGBG`
//! when it is set, else the terminal's answer to an OSC 11 background-colour
//! query given within [`QUERY_TIMEOUT`], else dark. An explicit theme skips
//! detection, so startup never waits on a terminal that does not answer for
//! longer than the timeout.

use std::time::Duration;

use pmt_config::schema::TuiTheme;

/// The longest the query may delay startup.
pub const QUERY_TIMEOUT: Duration = Duration::from_millis(100);

/// The theme to use: `configured` unless it is `auto`, then `COLORFGBG`,
/// then `query` (run only when needed), then dark.
pub fn resolve(
    configured: TuiTheme,
    colorfgbg: Option<&str>,
    query: impl FnOnce() -> Option<TuiTheme>,
) -> TuiTheme {
    if configured != TuiTheme::Auto {
        return configured;
    }
    colorfgbg
        .and_then(from_colorfgbg)
        .or_else(query)
        .unwrap_or(TuiTheme::Dark)
}

/// `COLORFGBG` (`<fg>;<bg>` or `<fg>;<other>;<bg>`): background colours 7
/// and 9 to 15 are light, the others dark.
#[must_use]
pub fn from_colorfgbg(value: &str) -> Option<TuiTheme> {
    let bg: u8 = value.rsplit(';').next()?.trim().parse().ok()?;
    Some(if bg == 7 || (9..=15).contains(&bg) {
        TuiTheme::Light
    } else {
        TuiTheme::Dark
    })
}

/// The theme for an OSC 11 reply (`ESC ] 11 ; rgb:RRRR/GGGG/BBBB` ended by
/// BEL or ST, each channel 1 to 4 hex digits): light when its relative
/// luminance is above one half.
#[must_use]
pub fn from_osc11(reply: &[u8]) -> Option<TuiTheme> {
    let text = std::str::from_utf8(reply).ok()?;
    let start = text.find("]11;")? + 4;
    let rest = &text[start..];
    let end = rest.find(['\u{7}', '\u{1b}']).unwrap_or(rest.len());
    let rgb = rest[..end].strip_prefix("rgb:")?;
    let mut channels = rgb.split('/').map(channel);
    let (r, g, b) = (channels.next()??, channels.next()??, channels.next()??);
    let luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    Some(if luminance > 0.5 {
        TuiTheme::Light
    } else {
        TuiTheme::Dark
    })
}

/// One hex channel scaled to 0..=1.
fn channel(hex: &str) -> Option<f64> {
    if hex.is_empty() || hex.len() > 4 {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    let max = (1u32 << (4 * hex.len())) - 1;
    Some(f64::from(value) / f64::from(max))
}

/// Asks the terminal for its background colour. The terminal must be in
/// raw mode and nothing else may be reading it; a terminal that does not
/// answer within [`QUERY_TIMEOUT`] gives `None`.
#[cfg(unix)]
#[must_use]
pub fn query() -> Option<TuiTheme> {
    use std::io::Write as _;
    use std::os::fd::AsFd as _;

    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

    let mut out = std::io::stdout();
    out.write_all(b"\x1b]11;?\x07").ok()?;
    out.flush().ok()?;
    let stdin = std::io::stdin();
    let deadline = std::time::Instant::now() + QUERY_TIMEOUT;
    let mut reply = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let ms = u16::try_from(left.as_millis()).unwrap_or(u16::MAX);
        if ms == 0 {
            return None;
        }
        let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
        if poll(&mut fds, PollTimeout::from(ms)).ok()? == 0 {
            return None;
        }
        let mut buf = [0u8; 64];
        // Unbuffered: what is not the reply stays for the key reader.
        let n = nix::unistd::read(stdin.as_fd(), &mut buf).ok()?;
        if n == 0 {
            return None;
        }
        reply.extend_from_slice(&buf[..n]);
        if let Some(theme) = complete(&reply).then(|| from_osc11(&reply)) {
            return theme;
        }
    }
}

/// Windows consoles are not queried: `auto` falls back to `COLORFGBG`
/// and then dark.
#[cfg(not(unix))]
#[must_use]
pub fn query() -> Option<TuiTheme> {
    None
}

/// The reply has its terminator (BEL or ST) after the `]11;` prefix.
#[cfg_attr(not(unix), allow(dead_code))]
fn complete(reply: &[u8]) -> bool {
    let Some(at) = reply.windows(4).position(|w| w == b"]11;") else {
        return false;
    };
    let rest = &reply[at..];
    rest.contains(&0x07) || rest.windows(2).any(|w| w == b"\x1b\\")
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::TuiTheme;

    use super::{complete, from_colorfgbg, from_osc11, resolve};

    #[test]
    fn colorfgbg_names_the_background_last() {
        assert_eq!(from_colorfgbg("0;15"), Some(TuiTheme::Light));
        assert_eq!(from_colorfgbg("15;0"), Some(TuiTheme::Dark));
        assert_eq!(from_colorfgbg("12;default;7"), Some(TuiTheme::Light));
        assert_eq!(from_colorfgbg("garbage"), None);
    }

    #[test]
    fn an_osc11_reply_gives_the_background_brightness() {
        let white = b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\";
        assert_eq!(from_osc11(white), Some(TuiTheme::Light));
        let dark = b"\x1b]11;rgb:1c1c/1c1c/1c1c\x07";
        assert_eq!(from_osc11(dark), Some(TuiTheme::Dark));
        assert_eq!(
            from_osc11(b"\x1b]11;rgb:ee/ee/ee\x07"),
            Some(TuiTheme::Light)
        );
        assert_eq!(from_osc11(b"\x1b]11;?\x07"), None);
        assert!(complete(white) && complete(dark));
        assert!(!complete(b"\x1b]11;rgb:ff"));
    }

    #[test]
    fn detection_runs_only_for_auto_and_in_order() {
        let never = || -> Option<TuiTheme> { panic!("an explicit theme is never queried") };
        assert_eq!(resolve(TuiTheme::Dark, Some("0;15"), never), TuiTheme::Dark);
        let unused = || -> Option<TuiTheme> { panic!("COLORFGBG answers first") };
        assert_eq!(
            resolve(TuiTheme::Auto, Some("0;15"), unused),
            TuiTheme::Light
        );
        assert_eq!(
            resolve(TuiTheme::Auto, None, || Some(TuiTheme::Light)),
            TuiTheme::Light
        );
        assert_eq!(resolve(TuiTheme::Auto, None, || None), TuiTheme::Dark);
    }
}
