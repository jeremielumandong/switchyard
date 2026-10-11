//! UTC timestamps in the formats the services use, without a date library.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Days since 1970-01-01 of a civil date.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

struct Parts {
    y: i64,
    mo: u32,
    d: u32,
    h: u64,
    mi: u64,
    s: u64,
    weekday: usize,
}

fn parts(t: SystemTime) -> Parts {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, mo, d) = civil(days);
    Parts {
        y,
        mo,
        d,
        h: rem / 3600,
        mi: rem % 3600 / 60,
        s: rem % 60,
        // 1970-01-01 was a Thursday.
        weekday: ((days + 4).rem_euclid(7)) as usize,
    }
}

/// `20150830T123600Z` (AWS `x-amz-date`).
pub(crate) fn amz_date(t: SystemTime) -> String {
    let p = parts(t);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        p.y, p.mo, p.d, p.h, p.mi, p.s
    )
}

/// `Sun, 30 Aug 2015 12:36:00 GMT` (HTTP date, Azure `x-ms-date`).
pub(crate) fn http_date(t: SystemTime) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let p = parts(t);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[p.weekday],
        p.d,
        MONTHS[(p.mo - 1) as usize],
        p.y,
        p.h,
        p.mi,
        p.s
    )
}

/// Milliseconds since the epoch of an ISO 8601 UTC time (`2009-10-12T17:50:30.000Z`, with
/// or without fraction; an offset other than `Z` is not expected from these services).
pub(crate) fn parse_iso_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut ms = 0;
    if s.as_bytes().get(19) == Some(&b'.') {
        let frac: String = s[20..].chars().take_while(char::is_ascii_digit).collect();
        let padded = format!("{frac:0<3}");
        ms = padded.get(..3)?.parse::<i64>().ok()?;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec) * 1000) + ms)
}

/// Milliseconds since the epoch of an HTTP date (`Wed, 12 Oct 2009 17:50:30 GMT`).
pub(crate) fn parse_http_date_ms(s: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut it = s.split_whitespace().skip(1);
    let d: u32 = it.next()?.parse().ok()?;
    let mon = it.next()?;
    let mo = MONTHS.iter().position(|m| *m == mon)? as u32 + 1;
    let y: i64 = it.next()?.parse().ok()?;
    let mut hms = it.next()?.split(':').map(|p| p.parse::<i64>().ok());
    let (h, mi, sec) = (hms.next()??, hms.next()??, hms.next()??);
    let days = days_from_civil(y, mo, d);
    Some((days * 86_400 + h * 3600 + mi * 60 + sec) * 1000)
}

/// A readable UTC time (`2026-10-10 12:00 UTC`) for lists.
pub fn display_ms(ms: i64) -> String {
    let t = UNIX_EPOCH + Duration::from_millis(ms.max(0) as u64);
    let p = parts(t);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        p.y, p.mo, p.d, p.h, p.mi
    )
}

/// Milliseconds since the epoch of a UTC time typed as `2026-03-01`, `2026-03-01 14:30`
/// (what [`display_ms`] shows, with or without ` UTC`) or ISO 8601.
pub fn parse_utc_ms(s: &str) -> Option<i64> {
    let s = s
        .trim()
        .trim_end_matches("UTC")
        .trim_end_matches('Z')
        .trim();
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let sep = |i: usize| s.as_bytes().get(i).copied();
    if s.len() < 10 || sep(4) != Some(b'-') || sep(7) != Some(b'-') {
        return None;
    }
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let (h, mi, sec) = match s.len() {
        10 => (0, 0, 0),
        16 if matches!(sep(10), Some(b' ' | b'T')) && sep(13) == Some(b':') => {
            (num(11, 13)?, num(14, 16)?, 0)
        }
        _ if matches!(sep(10), Some(b' ' | b'T'))
            && sep(13) == Some(b':')
            && sep(16) == Some(b':') =>
        {
            (num(11, 13)?, num(14, 16)?, num(17, 19)?)
        }
        _ => return None,
    };
    if h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    Some((days * 86_400 + h * 3600 + mi * 60 + sec) * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_times() {
        let ms = parse_utc_ms("2025-06-18 19:18 UTC").unwrap();
        assert_eq!(display_ms(ms), "2025-06-18 19:18 UTC");
        assert_eq!(
            parse_utc_ms("2025-06-18"),
            parse_utc_ms("2025-06-18T00:00:00Z")
        );
        assert_eq!(parse_utc_ms("2025-06-18 19:18:30").unwrap() - ms, 30_000);
        assert!(parse_utc_ms("18/06/2025").is_none());
        assert!(parse_utc_ms("2025-13-01").is_none());
    }

    #[test]
    fn formats() {
        let t = UNIX_EPOCH + Duration::from_secs(1_440_938_160);
        assert_eq!(amz_date(t), "20150830T123600Z");
        assert_eq!(http_date(t), "Sun, 30 Aug 2015 12:36:00 GMT");
        assert_eq!(display_ms(1_440_938_160_000), "2015-08-30 12:36 UTC");
    }

    #[test]
    fn parses() {
        assert_eq!(
            parse_iso_ms("2015-08-30T12:36:00.123Z"),
            Some(1_440_938_160_123)
        );
        assert_eq!(
            parse_iso_ms("2015-08-30T12:36:00Z"),
            Some(1_440_938_160_000)
        );
        assert_eq!(
            parse_iso_ms("2015-08-30T12:36:00.1234567+00:00"),
            Some(1_440_938_160_123)
        );
        assert_eq!(
            parse_http_date_ms("Sun, 30 Aug 2015 12:36:00 GMT"),
            Some(1_440_938_160_000)
        );
        assert_eq!(parse_iso_ms("garbage"), None);
        assert_eq!(
            parse_iso_ms("2024-02-29T00:00:00Z"),
            Some(1_709_164_800_000)
        );
    }
}
