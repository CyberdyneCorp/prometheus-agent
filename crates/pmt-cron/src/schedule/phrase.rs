//! Day/time phrases that are exact cron expressions (`every monday 9am`,
//! `weekdays at 9:30`, `every day at 18:00`) and the time of day they and
//! the one-shot forms (`at 9am`, `tomorrow at 18:00`) share.

/// The time of day of `text` (`9am`, `9:30`, `18:00`, `9:30 pm`, `noon`,
/// `midnight`).
#[must_use]
pub(super) fn time_of_day(text: &str) -> Option<(i8, i8)> {
    let text = text.trim();
    match text {
        "noon" => return Some((12, 0)),
        "midnight" => return Some((0, 0)),
        _ => {}
    }
    let (clock, meridiem) = if let Some(rest) = text.strip_suffix("am") {
        (rest.trim_end(), Some(false))
    } else if let Some(rest) = text.strip_suffix("pm") {
        (rest.trim_end(), Some(true))
    } else {
        (text, None)
    };
    let (hour, minute) = match clock.split_once(':') {
        Some((h, m)) if m.len() == 2 => (h.parse::<i8>().ok()?, m.parse::<i8>().ok()?),
        None if meridiem.is_some() => (clock.parse::<i8>().ok()?, 0),
        _ => return None,
    };
    if !(0..60).contains(&minute) {
        return None;
    }
    let hour = match meridiem {
        None if (0..24).contains(&hour) => hour,
        Some(pm) if (1..=12).contains(&hour) => (hour % 12) + if pm { 12 } else { 0 },
        _ => return None,
    };
    Some((hour, minute))
}

const WEEKDAYS: [(&str, &str, u8); 7] = [
    ("sunday", "sun", 0),
    ("monday", "mon", 1),
    ("tuesday", "tue", 2),
    ("wednesday", "wed", 3),
    ("thursday", "thu", 4),
    ("friday", "fri", 5),
    ("saturday", "sat", 6),
];

/// The day-of-week field of a day word (`weekdays`, `mondays`, `day`).
fn days(word: &str) -> Option<String> {
    match word {
        "day" | "daily" => return Some("*".to_owned()),
        "weekday" | "weekdays" => return Some("1-5".to_owned()),
        "weekend" | "weekends" => return Some("0,6".to_owned()),
        _ => {}
    }
    let word = word.strip_suffix('s').unwrap_or(word);
    WEEKDAYS
        .iter()
        .find(|(long, short, _)| word == *long || word == *short)
        .map(|(_, _, n)| n.to_string())
}

/// The cron expression of a day/time phrase; `None` when `text` is not
/// one.
#[must_use]
pub(super) fn cron_of(text: &str) -> Option<String> {
    let text = text.strip_prefix("every ").unwrap_or(text);
    let (day_word, rest) = text.split_once(' ')?;
    let weekdays = days(day_word)?;
    let rest = rest.trim();
    let rest = rest.strip_prefix("at ").unwrap_or(rest);
    let (hour, minute) = time_of_day(rest)?;
    Some(format!("{minute} {hour} * * {weekdays}"))
}

#[cfg(test)]
mod tests {
    use super::{cron_of, time_of_day};

    #[test]
    fn phrases_are_exact_cron_expressions() {
        for (phrase, cron) in [
            ("every monday 9am", "0 9 * * 1"),
            ("every monday at 9am", "0 9 * * 1"),
            ("weekdays at 9:30", "30 9 * * 1-5"),
            ("every day at 18:00", "0 18 * * *"),
            ("daily at noon", "0 12 * * *"),
            ("every weekend at 10:15 am", "15 10 * * 0,6"),
            ("fridays at 5pm", "0 17 * * 5"),
        ] {
            assert_eq!(cron_of(phrase).as_deref(), Some(cron), "{phrase}");
        }
        for not in [
            "every 30m",
            "monday",
            "every monday",
            "every blursday at 9am",
            "weekdays at 25:00",
        ] {
            assert_eq!(cron_of(not), None, "{not}");
        }
    }

    #[test]
    fn times_of_day() {
        assert_eq!(time_of_day("12am"), Some((0, 0)));
        assert_eq!(time_of_day("12pm"), Some((12, 0)));
        assert_eq!(time_of_day("9:05"), Some((9, 5)));
        assert_eq!(time_of_day("9"), None);
        assert_eq!(time_of_day("13pm"), None);
        assert_eq!(time_of_day("9:5"), None);
    }
}
