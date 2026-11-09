//! Schedule durations: a positive integer and one unit of `s`, `m`, `h`,
//! `d` or `w` (`30m`, `2h`, `1w`).

/// Seconds of `text` (`30m` -> 1800); `None` when it is not a duration.
#[must_use]
pub fn parse(text: &str) -> Option<u64> {
    let text = text.trim();
    let unit = text.chars().last()?;
    let number = &text[..text.len() - unit.len_utf8()];
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number: u64 = number.parse().ok()?;
    let scale = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        'w' => 604_800,
        _ => return None,
    };
    number.checked_mul(scale).filter(|secs| *secs > 0)
}

/// Seconds of a limit such as a grace or timeout: like [`parse`], also
/// accepting zero (`0s`).
#[must_use]
pub fn parse_limit(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.len() > 1 && text[..text.len() - 1].bytes().all(|b| b == b'0') {
        return parse(&format!("1{}", &text[text.len() - 1..])).map(|_| 0);
    }
    parse(text)
}

/// `secs` in the largest unit that divides it exactly (`1800` -> `30m`).
#[must_use]
pub fn format(secs: u64) -> String {
    for (scale, unit) in [(604_800, 'w'), (86_400, 'd'), (3_600, 'h'), (60, 'm')] {
        if secs >= scale && secs.is_multiple_of(scale) {
            return format!("{}{unit}", secs / scale);
        }
    }
    format!("{secs}s")
}

#[cfg(test)]
mod tests {
    use super::{format, parse, parse_limit};

    #[test]
    fn durations_parse_and_print_in_their_largest_unit() {
        assert_eq!(parse("30m"), Some(1_800));
        assert_eq!(parse("1w"), Some(604_800));
        assert_eq!(parse("90s"), Some(90));
        for bad in ["", "m", "0m", "-1h", "1.5h", "10x", "1 h"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert_eq!(format(1_800), "30m");
        assert_eq!(format(86_400), "1d");
        assert_eq!(format(90), "90s");
        assert_eq!(format(7_200), "2h");
        assert_eq!(parse_limit("0s"), Some(0));
        assert_eq!(parse_limit("00m"), Some(0));
        assert_eq!(parse_limit("2h"), Some(7_200));
        assert_eq!(parse_limit("0x"), None);
    }
}
