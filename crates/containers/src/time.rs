//! Kubernetes timestamps and kubectl-style ages.

/// Days from 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Unix seconds of an RFC 3339 time (`2026-10-10T09:00:00Z`, fractions and offsets
/// allowed); `None` when it does not parse.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        rest = &frac[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        o if o.len() == 6 && matches!(&o[..1], "+" | "-") && &o[3..4] == ":" => {
            let oh: i64 = o[1..3].parse().ok()?;
            let om: i64 = o[4..6].parse().ok()?;
            let sign = if o.starts_with('-') { -1 } else { 1 };
            sign * (oh * 3600 + om * 60)
        }
        _ => return None,
    };
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - offset)
}

/// An age like kubectl prints it: `45s`, `3m12s`, `47m`, `5h3m`, `20h`, `3d4h`, `45d`,
/// `2y30d`.
pub fn format_age(secs: i64) -> String {
    let s = secs.max(0);
    let (m, h, d) = (s / 60, s / 3600, s / 86_400);
    let y = d / 365;
    if s < 120 {
        format!("{s}s")
    } else if m < 10 {
        let r = s % 60;
        if r == 0 {
            format!("{m}m")
        } else {
            format!("{m}m{r}s")
        }
    } else if h < 3 {
        format!("{m}m")
    } else if h < 8 {
        let r = m % 60;
        if r == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{r}m")
        }
    } else if h < 48 {
        format!("{h}h")
    } else if d < 8 {
        let r = h % 24;
        if r == 0 {
            format!("{d}d")
        } else {
            format!("{d}d{r}h")
        }
    } else if y < 2 {
        format!("{d}d")
    } else if y < 8 {
        let r = d % 365;
        if r == 0 {
            format!("{y}y")
        } else {
            format!("{y}y{r}d")
        }
    } else {
        format!("{y}y")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-10-10T09:00:00Z"), Some(1_791_622_800));
        assert_eq!(
            parse_rfc3339("2026-10-10T11:00:00.123456+02:00"),
            Some(1_791_622_800)
        );
        assert_eq!(parse_rfc3339("2000-02-29T12:00:00Z"), Some(951_825_600));
        assert_eq!(parse_rfc3339(""), None);
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339("2026-10-10T09:00:00"), None);
    }

    #[test]
    fn ages() {
        assert_eq!(format_age(-5), "0s");
        assert_eq!(format_age(45), "45s");
        assert_eq!(format_age(192), "3m12s");
        assert_eq!(format_age(300), "5m");
        assert_eq!(format_age(47 * 60), "47m");
        assert_eq!(format_age(5 * 3600 + 3 * 60), "5h3m");
        assert_eq!(format_age(20 * 3600), "20h");
        assert_eq!(format_age(3 * 86_400 + 4 * 3600), "3d4h");
        assert_eq!(format_age(45 * 86_400), "45d");
        assert_eq!(format_age(2 * 365 * 86_400 + 30 * 86_400), "2y30d");
        assert_eq!(format_age(9 * 365 * 86_400), "9y");
    }
}
