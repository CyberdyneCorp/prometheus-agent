//! Automation intervals: one or more `<integer><unit>` parts with units
//! `s`, `m`, `h` and `d` (`30s`, `5m`, `2h`, `1h30m`).

use std::fmt::Write as _;

/// Seconds of `text`; `None` when it is not an interval (or is zero).
#[must_use]
pub fn parse(text: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut number = String::new();
    for c in text.trim().chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let scale = match c {
            's' => 1,
            'm' => 60,
            'h' => 3_600,
            'd' => 86_400,
            _ => return None,
        };
        let value: u64 = number.parse().ok()?;
        total = total.checked_add(value.checked_mul(scale)?)?;
        number.clear();
    }
    (number.is_empty() && total > 0).then_some(total)
}

/// `secs` as the person would write it (`90` -> `1m30s`, `300` -> `5m`).
#[must_use]
pub fn format(secs: u64) -> String {
    if secs == 0 {
        return "0s".to_owned();
    }
    let mut left = secs;
    let mut out = String::new();
    for (scale, unit) in [(86_400, 'd'), (3_600, 'h'), (60, 'm'), (1, 's')] {
        if left >= scale {
            let _ = write!(out, "{}{unit}", left / scale);
            left %= scale;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{format, parse};

    #[test]
    fn intervals_parse_with_compound_units() {
        assert_eq!(parse("30s"), Some(30));
        assert_eq!(parse("5m"), Some(300));
        assert_eq!(parse("2h"), Some(7_200));
        assert_eq!(parse("1h30m"), Some(5_400));
        assert_eq!(parse("1d"), Some(86_400));
        for bad in ["", "m", "0m", "5", "1.5h", "10x", "1 h", "h1"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert_eq!(format(5_400), "1h30m");
        assert_eq!(format(300), "5m");
        assert_eq!(format(45), "45s");
    }
}
