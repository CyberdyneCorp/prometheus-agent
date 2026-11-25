//! Wall-clock time for request signatures. The sandbox has no clock, so
//! it is read from the HTTP `Date` header of the platform's own answer
//! (RFC 9110 IMF-fixdate) and written as Beijing time, the form the
//! token endpoint signs (`2006-01-02T15:04:05+08:00`).

use adapter_kit::net;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Seconds since the epoch of an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`).
pub(crate) fn parse_http_date(text: &str) -> Option<i64> {
    let mut parts = text.split_whitespace().skip(1);
    let day: i64 = parts.next()?.parse().ok()?;
    let name = parts.next()?;
    let month = MONTHS.iter().position(|m| *m == name)?;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':').map(|p| p.parse::<i64>().ok());
    let (h, m, s) = (clock.next()??, clock.next()??, clock.next()??);
    let days = days_from_civil(year, i64::try_from(month).ok()? + 1, day);
    Some(days * 86_400 + h * 3600 + m * 60 + s)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `secs` since the epoch as Beijing time.
pub(crate) fn beijing(secs: i64) -> String {
    let local = secs + 8 * 3600;
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    let t = local.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+08:00",
        t / 3600,
        t % 3600 / 60,
        t % 60
    )
}

/// Now, as `api` tells it, in Beijing time.
///
/// # Errors
///
/// The platform is unreachable or sent no usable `Date`.
pub(crate) fn beijing_timestamp(api: &str) -> Result<String, String> {
    let response = net::send("GET", &format!("{api}/"), &[], None)?;
    let date = response
        .header("date")
        .and_then(parse_http_date)
        .ok_or("server: the Yuanbao API sent no Date header to sign with")?;
    Ok(beijing(date))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_dates_become_beijing_time() {
        let secs = parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT").unwrap();
        assert_eq!(secs, 784_111_777);
        assert_eq!(beijing(secs), "1994-11-06T16:49:37+08:00");
        assert_eq!(beijing(0), "1970-01-01T08:00:00+08:00");
        assert!(parse_http_date("yesterday").is_none());
    }
}
