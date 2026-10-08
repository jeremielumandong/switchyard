//! Engine identifiers, logical data types and the owned [`Value`] type.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// A database engine Switchyard can talk to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    /// PostgreSQL.
    Postgres,
    /// Microsoft SQL Server.
    SqlServer,
    /// Cloudflare D1 (SQLite) over the Cloudflare REST API.
    D1,
    /// Snowflake over its SQL REST API.
    Snowflake,
    /// Oracle Database through Oracle Instant Client, loaded at runtime.
    Oracle,
    /// Redis (key-value; browsed with [`crate::redis`], not SQL).
    Redis,
}

impl Engine {
    /// Human-readable engine name.
    pub fn display_name(self) -> &'static str {
        match self {
            Engine::Postgres => "PostgreSQL",
            Engine::SqlServer => "SQL Server",
            Engine::D1 => "Cloudflare D1",
            Engine::Snowflake => "Snowflake",
            Engine::Oracle => "Oracle",
            Engine::Redis => "Redis",
        }
    }

    /// Short monogram used in the UI (`PG`, `MS`, `D1`).
    pub fn badge(self) -> &'static str {
        match self {
            Engine::Postgres => "PG",
            Engine::SqlServer => "MS",
            Engine::D1 => "D1",
            Engine::Snowflake => "SF",
            Engine::Oracle => "OR",
            Engine::Redis => "RD",
        }
    }

    /// Default TCP port.
    pub fn default_port(self) -> u16 {
        match self {
            Engine::Postgres => 5432,
            Engine::SqlServer => 1433,
            Engine::D1 | Engine::Snowflake => 443,
            Engine::Oracle => 1521,
            Engine::Redis => 6379,
        }
    }

    /// Whether the engine supports interactive transactions (BEGIN ... COMMIT across
    /// requests). D1's and Snowflake's HTTP APIs run every request on its own.
    pub fn supports_transactions(self) -> bool {
        !matches!(self, Engine::D1 | Engine::Snowflake | Engine::Redis)
    }

    /// Whether the engine is reached through a cloud HTTP API (account and database ids
    /// plus an API token) rather than host, port and user.
    pub fn is_cloud_api(self) -> bool {
        matches!(self, Engine::D1)
    }

    /// Whether the engine speaks SQL. Key-value stores (Redis) open a key browser instead
    /// of a SQL editor and have no catalog, plans or activity views.
    pub fn is_sql(self) -> bool {
        !matches!(self, Engine::Redis)
    }
}

/// Logical type of a result column, independent of the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DataType {
    /// Boolean.
    Bool,
    /// 16-bit integer.
    Int16,
    /// 32-bit integer.
    Int32,
    /// 64-bit integer.
    Int64,
    /// 32-bit float.
    Float32,
    /// 64-bit float.
    Float64,
    /// Exact decimal, carried as text.
    Numeric,
    /// Character data.
    Text,
    /// Binary data.
    Bytes,
    /// UUID / uniqueidentifier.
    Uuid,
    /// JSON document, carried as text.
    Json,
    /// XML document, carried as text.
    Xml,
    /// Calendar date (days since 1970-01-01).
    Date,
    /// Time of day (microseconds since midnight).
    Time,
    /// Timestamp without time zone (microseconds since 1970-01-01).
    Timestamp,
    /// Timestamp with time zone, normalized to UTC (microseconds since 1970-01-01).
    TimestampTz,
    /// Interval, carried as text.
    Interval,
    /// Anything else (arrays, ranges, engine-specific types), carried as text.
    Other,
}

impl DataType {
    /// Whether values are numbers (right-aligned with tabular figures in the grid).
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            DataType::Int16
                | DataType::Int32
                | DataType::Int64
                | DataType::Float32
                | DataType::Float64
                | DataType::Numeric
        )
    }

    /// Whether the value viewer should pretty-print this type as a document.
    pub fn is_document(self) -> bool {
        matches!(self, DataType::Json | DataType::Xml)
    }
}

/// An owned cell value, used for parameters, the value viewer and generated SQL.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Value {
    /// SQL NULL.
    Null,
    /// Boolean.
    Bool(bool),
    /// Any integer.
    Int(i64),
    /// Any float.
    Float(f64),
    /// Exact decimal as text.
    Numeric(String),
    /// Text.
    Text(String),
    /// Binary.
    Bytes(Vec<u8>),
    /// UUID bytes.
    Uuid([u8; 16]),
    /// JSON text.
    Json(String),
    /// Days since 1970-01-01.
    Date(i32),
    /// Microseconds since midnight.
    Time(i64),
    /// Microseconds since 1970-01-01, no zone.
    Timestamp(i64),
    /// Microseconds since 1970-01-01 UTC.
    TimestampTz(i64),
    /// Engine-formatted text for other types.
    Other(String),
}

impl Value {
    /// Whether this is NULL.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Display text (NULL renders as `NULL`).
    pub fn to_display(&self) -> String {
        let mut s = String::new();
        self.write_display(&mut s);
        s
    }

    /// Append display text to `out`.
    pub fn write_display(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("NULL"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Int(i) => {
                let _ = write!(out, "{i}");
            }
            Value::Float(f) => write_float(out, *f),
            Value::Numeric(s) | Value::Text(s) | Value::Json(s) | Value::Other(s) => {
                out.push_str(s)
            }
            Value::Bytes(b) => write_hex(out, b),
            Value::Uuid(u) => write_uuid(out, u),
            Value::Date(d) => write_date(out, *d),
            Value::Time(t) => write_time(out, *t),
            Value::Timestamp(t) => write_timestamp(out, *t, false),
            Value::TimestampTz(t) => write_timestamp(out, *t, true),
        }
    }
}

/// Write a float without exponent noise for common magnitudes.
pub fn write_float(out: &mut String, f: f64) {
    if f.is_nan() {
        out.push_str("NaN");
    } else if f.is_infinite() {
        out.push_str(if f > 0.0 { "Infinity" } else { "-Infinity" });
    } else {
        let _ = write!(out, "{f}");
    }
}

/// Write bytes as `\x`-prefixed lowercase hex.
pub fn write_hex(out: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.reserve(2 + bytes.len() * 2);
    out.push_str("\\x");
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
}

/// Write a UUID in canonical 8-4-4-4-12 form.
pub fn write_uuid(out: &mut String, u: &[u8; 16]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, b) in u.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
}

/// Convert days since 1970-01-01 to a proleptic Gregorian (year, month, day).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Howard Hinnant's algorithm.
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

/// Convert a proleptic Gregorian date to days since 1970-01-01.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn write_ymd(out: &mut String, days: i64) {
    let (y, m, d) = civil_from_days(days);
    if y <= 0 {
        let _ = write!(out, "{:04}-{m:02}-{d:02} BC", 1 - y);
    } else {
        let _ = write!(out, "{y:04}-{m:02}-{d:02}");
    }
}

/// Write a date given as days since 1970-01-01.
pub fn write_date(out: &mut String, days: i32) {
    match days {
        i32::MAX => out.push_str("infinity"),
        i32::MIN => out.push_str("-infinity"),
        d => write_ymd(out, d as i64),
    }
}

/// Write a time of day given as microseconds since midnight.
pub fn write_time(out: &mut String, micros: i64) {
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let _ = write!(
        out,
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    );
    write_fraction(out, frac);
}

fn write_fraction(out: &mut String, frac: i64) {
    if frac != 0 {
        let mut s = format!("{frac:06}");
        while s.ends_with('0') {
            s.pop();
        }
        out.push('.');
        out.push_str(&s);
    }
}

/// Write a timestamp given as microseconds since the Unix epoch.
pub fn write_timestamp(out: &mut String, micros: i64, utc: bool) {
    match micros {
        i64::MAX => return out.push_str("infinity"),
        i64::MIN => return out.push_str("-infinity"),
        _ => {}
    }
    let days = micros.div_euclid(86_400_000_000);
    let rem = micros.rem_euclid(86_400_000_000);
    write_ymd(out, days);
    out.push(' ');
    write_time(out, rem);
    if utc {
        out.push_str("+00");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trip() {
        for days in [-800_000i64, -1, 0, 1, 10_957, 20_000, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
    }

    #[test]
    fn display_formats() {
        assert_eq!(Value::Date(20_366).to_display(), "2025-10-05");
        assert_eq!(Value::Time(3_723_500_000).to_display(), "01:02:03.5");
        assert_eq!(
            Value::TimestampTz(1_759_651_200_000_000).to_display(),
            "2025-10-05 08:00:00+00"
        );
        assert_eq!(Value::Bytes(vec![0xde, 0xad]).to_display(), "\\xdead");
        assert_eq!(
            Value::Uuid([
                0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66, 0x55, 0x44,
                0x00, 0x00
            ])
            .to_display(),
            "550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(Value::Null.to_display(), "NULL");
        assert_eq!(Value::Float(1.5).to_display(), "1.5");
        assert_eq!(Value::Date(i32::MAX).to_display(), "infinity");
    }
}
