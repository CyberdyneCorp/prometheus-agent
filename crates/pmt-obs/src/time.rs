//! Time arguments shared by `prometheus logs` and `prometheus usage`
//! (`<N>[smhd]` before now, or an RFC 3339 time) and the profile time zone
//! that days and months are evaluated in.

use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use pmt_config::schema::CronConfig;
use pmt_core::error::{Error, codes};

/// Parses `<N>[smhd]` (that long before `now`) or an RFC 3339 time; `None`
/// when `value` is neither.
#[must_use]
pub fn parse_point(value: &str, now: Timestamp) -> Option<Timestamp> {
    if let Ok(ts) = value.parse::<Timestamp>() {
        return Some(ts);
    }
    // Split before the last character, not the last byte: the suffix may
    // be any (multibyte) character the user typed.
    let (split, _) = value.char_indices().last()?;
    let (number, unit) = value.split_at(split);
    let number: u64 = number.parse().ok()?;
    let seconds = match unit {
        "s" => Some(number),
        "m" => number.checked_mul(60),
        "h" => number.checked_mul(3_600),
        "d" => number.checked_mul(86_400),
        _ => None,
    }?;
    now.checked_sub(Duration::from_secs(seconds)).ok()
}

/// The profile's time zone: `cron.timezone`, else the host's zone.
///
/// # Errors
///
/// `config.timezone` (78) for a name the time zone database lacks.
pub fn profile_zone(cron: &CronConfig) -> Result<TimeZone, Error> {
    match cron.timezone.as_deref().filter(|name| !name.is_empty()) {
        None => Ok(TimeZone::system()),
        Some(name) => TimeZone::get(name).map_err(|_| {
            Error::new(
                &codes::CONFIG_TIMEZONE,
                format!("cron.timezone: unknown time zone '{name}'"),
            )
            .with_hint("use an IANA name such as Europe/Lisbon or America/New_York")
        }),
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use pmt_config::schema::CronConfig;

    use super::{parse_point, profile_zone};

    #[test]
    fn points_are_durations_before_now_or_rfc3339_times() {
        let now: Timestamp = "2026-05-01T12:00:00Z".parse().unwrap();
        assert_eq!(
            parse_point("7d", now),
            "2026-04-24T12:00:00Z".parse::<Timestamp>().ok()
        );
        assert_eq!(
            parse_point("2026-04-01T00:00:00+02:00", now),
            "2026-03-31T22:00:00Z".parse::<Timestamp>().ok()
        );
        for bad in ["", "d", "5w", "-1h", "5é", "日"] {
            assert_eq!(parse_point(bad, now), None, "{bad}");
        }
    }

    #[test]
    fn an_unknown_zone_is_a_configuration_error() {
        let zone = |name: &str| CronConfig {
            timezone: Some(name.to_owned()),
        };
        assert!(profile_zone(&zone("Europe/Lisbon")).is_ok());
        assert!(profile_zone(&CronConfig::default()).is_ok());
        let err = profile_zone(&zone("Mars/Olympus")).unwrap_err();
        assert_eq!(err.exit_code(), 78);
        assert_eq!(
            err.message(),
            "cron.timezone: unknown time zone 'Mars/Olympus'"
        );
    }
}
