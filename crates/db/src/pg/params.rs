//! Binding [`Value`]s (usually text typed into a parameter prompt) to PostgreSQL types.

use std::error::Error;

use postgres_types::private::BytesMut;
use postgres_types::{IsNull, Kind, ToSql, Type, to_sql_checked};

use super::decode::{PG_EPOCH_DAYS, PG_EPOCH_MICROS};
use crate::value::{Value, days_from_civil};

type BoxErr = Box<dyn Error + Sync + Send>;

/// A value bound for the server-inferred parameter type.
#[derive(Debug)]
pub struct PgParam<'a>(pub &'a Value);

fn err(msg: impl Into<String>) -> BoxErr {
    msg.into().into()
}

fn text_of(v: &Value) -> String {
    v.to_display()
}

fn parse_bool(s: &str) -> Result<bool, BoxErr> {
    match s.trim().to_ascii_lowercase().as_str() {
        "t" | "true" | "1" | "yes" | "y" | "on" => Ok(true),
        "f" | "false" | "0" | "no" | "n" | "off" => Ok(false),
        other => Err(err(format!("invalid boolean: {other}"))),
    }
}

fn int_of(v: &Value) -> Result<i64, BoxErr> {
    match v {
        Value::Int(i) => Ok(*i),
        Value::Bool(b) => Ok(*b as i64),
        other => text_of(other)
            .trim()
            .parse::<i64>()
            .map_err(|e| err(format!("invalid integer: {e}"))),
    }
}

fn float_of(v: &Value) -> Result<f64, BoxErr> {
    match v {
        Value::Float(f) => Ok(*f),
        Value::Int(i) => Ok(*i as f64),
        other => text_of(other)
            .trim()
            .parse::<f64>()
            .map_err(|e| err(format!("invalid number: {e}"))),
    }
}

/// Encode a decimal string as binary NUMERIC.
pub fn encode_numeric(s: &str, out: &mut BytesMut) -> Result<(), BoxErr> {
    let s = s.trim();
    let special = match s.to_ascii_lowercase().as_str() {
        "nan" => Some(0xC000u16),
        "infinity" | "+infinity" => Some(0xD000),
        "-infinity" => Some(0xF000),
        _ => None,
    };
    if let Some(sign) = special {
        for v in [0u16, 0, sign, 0] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        return Ok(());
    }
    let (neg, body) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    if int_part.is_empty() && frac_part.is_empty()
        || !int_part
            .chars()
            .chain(frac_part.chars())
            .all(|c| c.is_ascii_digit())
    {
        return Err(err(format!("invalid numeric: {s}")));
    }
    let dscale = frac_part.len() as u16;
    let pad_left = (4 - int_part.len() % 4) % 4;
    let pad_right = (4 - frac_part.len() % 4) % 4;
    let digits_str = format!(
        "{}{int_part}{frac_part}{}",
        "0".repeat(pad_left),
        "0".repeat(pad_right)
    );
    let int_groups = (int_part.len() + pad_left) / 4;
    let mut groups: Vec<i16> = digits_str
        .as_bytes()
        .chunks(4)
        .map(|c| {
            std::str::from_utf8(c)
                .unwrap_or("0")
                .parse::<i16>()
                .unwrap_or(0)
        })
        .collect();
    let mut weight = int_groups as i32 - 1;
    while groups.first() == Some(&0) {
        groups.remove(0);
        weight -= 1;
    }
    while groups.last() == Some(&0) {
        groups.pop();
    }
    if groups.is_empty() {
        weight = 0;
    }
    let sign: u16 = if neg && !groups.is_empty() { 0x4000 } else { 0 };
    for v in [groups.len() as u16, weight as i16 as u16, sign, dscale] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    for g in groups {
        out.extend_from_slice(&g.to_be_bytes());
    }
    Ok(())
}

fn parse_uuid(s: &str) -> Result<[u8; 16], BoxErr> {
    let hex: String = s
        .chars()
        .filter(|c| *c != '-' && *c != '{' && *c != '}')
        .collect();
    if hex.len() != 32 {
        return Err(err(format!("invalid uuid: {s}")));
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| err("invalid uuid"))?;
    }
    Ok(out)
}

fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, BoxErr> {
    let h = s.trim_start_matches("\\x").trim_start_matches("0x");
    if !h.len().is_multiple_of(2) {
        return Err(err("odd number of hex digits"));
    }
    (0..h.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&h[i..i + 2], 16).map_err(|_| err("invalid hex")))
        .collect()
}

/// Parse `YYYY-MM-DD` into days since 1970-01-01.
pub fn parse_date(s: &str) -> Result<i32, BoxErr> {
    let mut it = s.trim().splitn(3, '-');
    let (y, m, d) = (it.next(), it.next(), it.next());
    let (Some(y), Some(m), Some(d)) = (y, m, d) else {
        return Err(err(format!("invalid date: {s}")));
    };
    let y: i64 = y.parse().map_err(|_| err("invalid year"))?;
    let m: u32 = m.parse().map_err(|_| err("invalid month"))?;
    let d: u32 = d.parse().map_err(|_| err("invalid day"))?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(err(format!("invalid date: {s}")));
    }
    Ok(days_from_civil(y, m, d) as i32)
}

/// Parse `HH:MM[:SS[.ffffff]]` into microseconds since midnight.
pub fn parse_time(s: &str) -> Result<i64, BoxErr> {
    let s = s.trim();
    let mut parts = s.split(':');
    let h: i64 = parts
        .next()
        .unwrap_or("")
        .parse()
        .map_err(|_| err("invalid hour"))?;
    let m: i64 = parts
        .next()
        .unwrap_or("0")
        .parse()
        .map_err(|_| err("invalid minute"))?;
    let sec = parts.next().unwrap_or("0");
    let (whole, frac) = sec.split_once('.').unwrap_or((sec, ""));
    let secs: i64 = whole.parse().map_err(|_| err("invalid second"))?;
    let mut f = frac.chars().take(6).collect::<String>();
    while f.len() < 6 {
        f.push('0');
    }
    let micros: i64 = f.parse().unwrap_or(0);
    Ok(((h * 60 + m) * 60 + secs) * 1_000_000 + micros)
}

/// Parse an ISO timestamp, optional zone (`Z`, `+02`, `-05:30`), into UTC µs since 1970.
pub fn parse_timestamp(s: &str) -> Result<i64, BoxErr> {
    let s = s.trim();
    let (date, rest) = s.split_once([' ', 'T']).unwrap_or((s, "00:00:00"));
    let days = parse_date(date)? as i64;
    let (time, offset_secs) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0)
    } else if let Some(pos) = rest.rfind(['+', '-']).filter(|p| *p >= 5) {
        let (t, z) = rest.split_at(pos);
        let sign = if z.starts_with('-') { -1 } else { 1 };
        let z = &z[1..];
        let (hh, mm) = z.split_once(':').unwrap_or((z, "0"));
        let hh: i64 = hh.parse().map_err(|_| err("invalid zone"))?;
        let mm: i64 = mm.parse().map_err(|_| err("invalid zone"))?;
        (t, sign * (hh * 3600 + mm * 60))
    } else {
        (rest, 0)
    };
    Ok(days * 86_400_000_000 + parse_time(time)? - offset_secs * 1_000_000)
}

impl ToSql for PgParam<'_> {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxErr> {
        let v = self.0;
        if v.is_null() {
            return Ok(IsNull::Yes);
        }
        let base = match ty.kind() {
            Kind::Domain(b) => b,
            _ => ty,
        };
        match *base {
            Type::BOOL => {
                let b = match v {
                    Value::Bool(b) => *b,
                    other => parse_bool(&text_of(other))?,
                };
                out.extend_from_slice(&[b as u8]);
            }
            Type::INT2 => out.extend_from_slice(&(i16::try_from(int_of(v)?)?).to_be_bytes()),
            Type::INT4 => out.extend_from_slice(&(i32::try_from(int_of(v)?)?).to_be_bytes()),
            Type::INT8 => out.extend_from_slice(&int_of(v)?.to_be_bytes()),
            Type::OID => out.extend_from_slice(&(u32::try_from(int_of(v)?)?).to_be_bytes()),
            Type::FLOAT4 => out.extend_from_slice(&(float_of(v)? as f32).to_be_bytes()),
            Type::FLOAT8 => out.extend_from_slice(&float_of(v)?.to_be_bytes()),
            Type::NUMERIC => encode_numeric(&text_of(v), out)?,
            Type::UUID => {
                let u = match v {
                    Value::Uuid(u) => *u,
                    other => parse_uuid(&text_of(other))?,
                };
                out.extend_from_slice(&u);
            }
            Type::BYTEA => match v {
                Value::Bytes(b) => out.extend_from_slice(b),
                other => {
                    let t = text_of(other);
                    if t.starts_with("\\x") {
                        out.extend_from_slice(&parse_hex_bytes(&t)?)
                    } else {
                        out.extend_from_slice(t.as_bytes())
                    }
                }
            },
            Type::JSONB => {
                out.extend_from_slice(&[1]);
                out.extend_from_slice(text_of(v).as_bytes());
            }
            Type::DATE => {
                let d = match v {
                    Value::Date(d) => *d,
                    other => parse_date(&text_of(other))?,
                };
                out.extend_from_slice(&(d - PG_EPOCH_DAYS).to_be_bytes());
            }
            Type::TIME => {
                let t = match v {
                    Value::Time(t) => *t,
                    other => parse_time(&text_of(other))?,
                };
                out.extend_from_slice(&t.to_be_bytes());
            }
            Type::TIMESTAMP | Type::TIMESTAMPTZ => {
                let t = match v {
                    Value::Timestamp(t) | Value::TimestampTz(t) => *t,
                    other => parse_timestamp(&text_of(other))?,
                };
                out.extend_from_slice(&(t - PG_EPOCH_MICROS).to_be_bytes());
            }
            Type::TEXT
            | Type::VARCHAR
            | Type::BPCHAR
            | Type::NAME
            | Type::UNKNOWN
            | Type::JSON
            | Type::XML => out.extend_from_slice(text_of(v).as_bytes()),
            _ if matches!(base.kind(), Kind::Enum(_)) => {
                out.extend_from_slice(text_of(v).as_bytes())
            }
            _ => {
                return Err(err(format!(
                    "cannot bind a value to parameter type {}; cast the placeholder, e.g. $1::text",
                    ty.name()
                )));
            }
        }
        Ok(IsNull::No)
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

#[cfg(test)]
mod tests {
    use super::super::decode::render_numeric;
    use super::*;

    fn round_trip(s: &str) -> String {
        let mut b = BytesMut::new();
        encode_numeric(s, &mut b).unwrap();
        let mut out = String::new();
        render_numeric(&b, &mut out).unwrap();
        out
    }

    #[test]
    fn numeric_round_trip() {
        for (input, want) in [
            ("4812.40", "4812.40"),
            ("0", "0"),
            ("-12.5", "-12.5"),
            ("10000", "10000"),
            ("0.0005", "0.0005"),
            ("123456789.123456789", "123456789.123456789"),
            ("NaN", "NaN"),
            (".5", "0.5"),
        ] {
            assert_eq!(round_trip(input), want, "{input}");
        }
        assert!(encode_numeric("12a", &mut BytesMut::new()).is_err());
    }

    #[test]
    fn temporal_parsing() {
        assert_eq!(parse_date("1970-01-02").unwrap(), 1);
        assert_eq!(parse_time("01:02:03.5").unwrap(), 3_723_500_000);
        assert_eq!(
            parse_timestamp("1970-01-01T01:00:00Z").unwrap(),
            3_600_000_000
        );
        assert_eq!(
            parse_timestamp("1970-01-01 02:00:00+01").unwrap(),
            3_600_000_000
        );
        assert_eq!(parse_timestamp("1970-01-01").unwrap(), 0);
    }
}
