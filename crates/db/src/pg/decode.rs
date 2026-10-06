//! PostgreSQL binary wire format decoding into columnar batches.
//!
//! Values are read straight from the row buffer (`RawCell`) and appended to typed column
//! buffers; text renderings for non-native types reuse one scratch string.

use std::fmt::Write as _;

use postgres_types::{FromSql, Kind, Type};

use crate::batch::RowBatchBuilder;
use crate::value::{self, DataType};

/// Microseconds between 1970-01-01 and PostgreSQL's epoch, 2000-01-01.
pub const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;
/// Days between 1970-01-01 and 2000-01-01.
pub const PG_EPOCH_DAYS: i32 = 10_957;

/// The raw bytes of one value, accepted for every type.
pub struct RawCell<'a>(pub &'a [u8]);

impl<'a> FromSql<'a> for RawCell<'a> {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        Ok(RawCell(raw))
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }
}

/// Logical type for a PostgreSQL type.
pub fn data_type(ty: &Type) -> DataType {
    match *ty {
        Type::BOOL => DataType::Bool,
        Type::INT2 => DataType::Int16,
        Type::INT4 => DataType::Int32,
        Type::INT8 | Type::OID => DataType::Int64,
        Type::FLOAT4 => DataType::Float32,
        Type::FLOAT8 => DataType::Float64,
        Type::NUMERIC => DataType::Numeric,
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::CHAR | Type::UNKNOWN => {
            DataType::Text
        }
        Type::BYTEA => DataType::Bytes,
        Type::UUID => DataType::Uuid,
        Type::JSON | Type::JSONB => DataType::Json,
        Type::XML => DataType::Xml,
        Type::DATE => DataType::Date,
        Type::TIME => DataType::Time,
        Type::TIMESTAMP => DataType::Timestamp,
        Type::TIMESTAMPTZ => DataType::TimestampTz,
        Type::INTERVAL => DataType::Interval,
        _ => match ty.kind() {
            Kind::Enum(_) => DataType::Text,
            Kind::Domain(base) => data_type(base),
            _ => DataType::Other,
        },
    }
}

fn be_i16(b: &[u8], at: usize) -> Option<i16> {
    Some(i16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn be_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn be_i32(b: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn be_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn be_i64(b: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Convert a PostgreSQL timestamp (µs since 2000) to µs since 1970, preserving infinities.
pub fn pg_ts_to_unix(v: i64) -> i64 {
    match v {
        i64::MAX | i64::MIN => v,
        v => v.saturating_add(PG_EPOCH_MICROS),
    }
}

/// Convert a PostgreSQL date (days since 2000) to days since 1970, preserving infinities.
pub fn pg_date_to_unix(v: i32) -> i32 {
    match v {
        i32::MAX | i32::MIN => v,
        v => v.saturating_add(PG_EPOCH_DAYS),
    }
}

/// Append one non-NULL value of type `ty` to `out`.
pub fn push_value(
    out: &mut RowBatchBuilder,
    dt: DataType,
    ty: &Type,
    raw: &[u8],
    scratch: &mut String,
) {
    let ok = match dt {
        DataType::Bool => raw.first().map(|b| out.push_bool(*b != 0)),
        DataType::Int16 => be_i16(raw, 0).map(|v| out.push_i64(v as i64)),
        DataType::Int32 => be_i32(raw, 0).map(|v| out.push_i64(v as i64)),
        DataType::Int64 if *ty == Type::OID => be_u32(raw, 0).map(|v| out.push_i64(v as i64)),
        DataType::Int64 => be_i64(raw, 0).map(|v| out.push_i64(v)),
        DataType::Float32 => be_u32(raw, 0).map(|v| out.push_f64(f32::from_bits(v) as f64)),
        DataType::Float64 => be_i64(raw, 0).map(|v| out.push_f64(f64::from_bits(v as u64))),
        DataType::Date => be_i32(raw, 0).map(|v| out.push_i64(pg_date_to_unix(v) as i64)),
        DataType::Time => be_i64(raw, 0).map(|v| out.push_i64(v)),
        DataType::Timestamp | DataType::TimestampTz => {
            be_i64(raw, 0).map(|v| out.push_i64(pg_ts_to_unix(v)))
        }
        DataType::Uuid => raw
            .get(..16)
            .and_then(|u| <[u8; 16]>::try_from(u).ok())
            .map(|u| out.push_uuid(u)),
        DataType::Bytes => {
            out.push_bytes(raw);
            Some(())
        }
        DataType::Text if matches!(ty.kind(), Kind::Domain(_)) => {
            scratch.clear();
            render(ty, raw, scratch);
            {
                out.push_str(scratch);
                Some(())
            }
        }
        DataType::Text => {
            out.push_str(&String::from_utf8_lossy(raw));
            Some(())
        }
        DataType::Json if *ty == Type::JSONB => {
            out.push_str(&String::from_utf8_lossy(raw.get(1..).unwrap_or_default()));
            Some(())
        }
        DataType::Json | DataType::Xml => {
            out.push_str(&String::from_utf8_lossy(raw));
            Some(())
        }
        DataType::Numeric | DataType::Interval | DataType::Other => {
            scratch.clear();
            render(ty, raw, scratch);
            {
                out.push_str(scratch);
                Some(())
            }
        }
    };
    if ok.is_none() {
        // Malformed or truncated value: keep the row shape and show it as NULL.
        out.push_null();
    }
}

/// Render any value as PostgreSQL-style text.
pub fn render(ty: &Type, raw: &[u8], out: &mut String) {
    if render_known(ty, raw, out).is_none() {
        out.clear();
        match std::str::from_utf8(raw) {
            Ok(s) => out.push_str(s),
            Err(_) => value::write_hex(out, raw),
        }
    }
}

fn render_known(ty: &Type, raw: &[u8], out: &mut String) -> Option<()> {
    match *ty {
        Type::BOOL => out.push_str(if *raw.first()? != 0 { "t" } else { "f" }),
        Type::INT2 => write!(out, "{}", be_i16(raw, 0)?).ok()?,
        Type::INT4 => write!(out, "{}", be_i32(raw, 0)?).ok()?,
        Type::INT8 => write!(out, "{}", be_i64(raw, 0)?).ok()?,
        Type::OID => write!(out, "{}", be_u32(raw, 0)?).ok()?,
        Type::FLOAT4 => value::write_float(out, f32::from_bits(be_u32(raw, 0)?) as f64),
        Type::FLOAT8 => value::write_float(out, f64::from_bits(be_i64(raw, 0)? as u64)),
        Type::NUMERIC => render_numeric(raw, out)?,
        Type::MONEY => {
            let cents = be_i64(raw, 0)?;
            let sign = if cents < 0 { "-" } else { "" };
            write!(
                out,
                "{sign}{}.{:02}",
                (cents / 100).abs(),
                (cents % 100).abs()
            )
            .ok()?
        }
        Type::BYTEA => value::write_hex(out, raw),
        Type::UUID => value::write_uuid(out, raw.get(..16)?.try_into().ok()?),
        Type::JSONB => out.push_str(std::str::from_utf8(raw.get(1..)?).ok()?),
        Type::DATE => value::write_date(out, pg_date_to_unix(be_i32(raw, 0)?)),
        Type::TIME => value::write_time(out, be_i64(raw, 0)?),
        Type::TIMETZ => {
            value::write_time(out, be_i64(raw, 0)?);
            write_offset(out, -be_i32(raw, 8)?);
        }
        Type::TIMESTAMP => value::write_timestamp(out, pg_ts_to_unix(be_i64(raw, 0)?), false),
        Type::TIMESTAMPTZ => value::write_timestamp(out, pg_ts_to_unix(be_i64(raw, 0)?), true),
        Type::INTERVAL => render_interval(be_i64(raw, 0)?, be_i32(raw, 8)?, be_i32(raw, 12)?, out),
        Type::INET | Type::CIDR => render_inet(raw, out)?,
        _ => match ty.kind() {
            Kind::Array(elem) => render_array(elem, raw, out)?,
            Kind::Domain(base) => render(base, raw, out),
            Kind::Range(elem) => render_range(elem, raw, out)?,
            Kind::Composite(fields) => {
                let types: Vec<&Type> = fields.iter().map(|f| f.type_()).collect();
                render_composite(&types, raw, out)?
            }
            _ if ty.name() == "hstore" => render_hstore(raw, out)?,
            _ => out.push_str(std::str::from_utf8(raw).ok()?),
        },
    }
    Some(())
}

fn write_offset(out: &mut String, secs_east: i32) {
    let sign = if secs_east < 0 { '-' } else { '+' };
    let s = secs_east.abs();
    let _ = write!(out, "{sign}{:02}", s / 3600);
    if s % 3600 != 0 {
        let _ = write!(out, ":{:02}", (s / 60) % 60);
    }
}

/// Render PostgreSQL's binary NUMERIC.
pub fn render_numeric(raw: &[u8], out: &mut String) -> Option<()> {
    let ndigits = be_i16(raw, 0)? as i32;
    let weight = be_i16(raw, 2)? as i32;
    let sign = be_u16(raw, 4)?;
    let dscale = be_u16(raw, 6)? as usize;
    match sign {
        0xC000 => {
            out.push_str("NaN");
            return Some(());
        }
        0xD000 => {
            out.push_str("Infinity");
            return Some(());
        }
        0xF000 => {
            out.push_str("-Infinity");
            return Some(());
        }
        _ => {}
    }
    let digit = |i: i32| -> Option<i16> {
        if i < 0 || i >= ndigits {
            Some(0)
        } else {
            be_i16(raw, 8 + 2 * i as usize)
        }
    };
    if sign == 0x4000 && ndigits > 0 {
        out.push('-');
    }
    if weight < 0 {
        out.push('0');
    } else {
        for i in 0..=weight {
            let d = digit(i)?;
            if i == 0 {
                let _ = write!(out, "{d}");
            } else {
                let _ = write!(out, "{d:04}");
            }
        }
    }
    if dscale > 0 {
        out.push('.');
        let start = out.len();
        let mut i = weight + 1;
        while out.len() - start < dscale {
            let _ = write!(out, "{:04}", digit(i)?);
            i += 1;
        }
        out.truncate(start + dscale);
    }
    Some(())
}

fn render_interval(micros: i64, days: i32, months: i32, out: &mut String) {
    let mut parts: Vec<String> = Vec::new();
    let (years, mons) = (months / 12, months % 12);
    let plural =
        |n: i64, one: &str, many: &str| format!("{n} {}", if n.abs() == 1 { one } else { many });
    if years != 0 {
        parts.push(plural(years as i64, "year", "years"));
    }
    if mons != 0 {
        parts.push(plural(mons as i64, "mon", "mons"));
    }
    if days != 0 {
        parts.push(plural(days as i64, "day", "days"));
    }
    if micros != 0 || parts.is_empty() {
        let mut t = String::new();
        if micros < 0 {
            t.push('-');
        }
        let m = micros.unsigned_abs() as i64;
        let secs = m / 1_000_000;
        let _ = write!(
            t,
            "{:02}:{:02}:{:02}",
            secs / 3600,
            (secs / 60) % 60,
            secs % 60
        );
        let frac = m % 1_000_000;
        if frac != 0 {
            let mut f = format!("{frac:06}");
            while f.ends_with('0') {
                f.pop();
            }
            t.push('.');
            t.push_str(&f);
        }
        parts.push(t);
    }
    out.push_str(&parts.join(" "));
}

fn render_inet(raw: &[u8], out: &mut String) -> Option<()> {
    let family = *raw.first()?;
    let bits = *raw.get(1)?;
    let is_cidr = *raw.get(2)? != 0;
    let nb = *raw.get(3)? as usize;
    let addr = raw.get(4..4 + nb)?;
    let full = match family {
        2 => {
            let a: [u8; 4] = addr.try_into().ok()?;
            write!(out, "{}", std::net::Ipv4Addr::from(a)).ok()?;
            32
        }
        3 => {
            let a: [u8; 16] = addr.try_into().ok()?;
            write!(out, "{}", std::net::Ipv6Addr::from(a)).ok()?;
            128
        }
        _ => return None,
    };
    if is_cidr || bits != full {
        write!(out, "/{bits}").ok()?;
    }
    Some(())
}

fn needs_quotes(s: &str) -> bool {
    s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars()
            .any(|c| matches!(c, ',' | '{' | '}' | '"' | '\\' | '(' | ')') || c.is_whitespace())
}

fn push_quoted(out: &mut String, s: &str) {
    if needs_quotes(s) {
        out.push('"');
        for c in s.chars() {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
    } else {
        out.push_str(s);
    }
}

fn render_array(elem: &Type, raw: &[u8], out: &mut String) -> Option<()> {
    let ndim = be_i32(raw, 0)? as usize;
    if ndim == 0 {
        out.push_str("{}");
        return Some(());
    }
    let mut dims = Vec::with_capacity(ndim);
    for d in 0..ndim {
        dims.push(be_i32(raw, 12 + d * 8)?.max(0) as usize);
    }
    let mut pos = 12 + ndim * 8;
    let mut scratch = String::new();
    fn level(
        elem: &Type,
        raw: &[u8],
        dims: &[usize],
        pos: &mut usize,
        out: &mut String,
        scratch: &mut String,
    ) -> Option<()> {
        out.push('{');
        for i in 0..dims[0] {
            if i > 0 {
                out.push(',');
            }
            if dims.len() > 1 {
                level(elem, raw, &dims[1..], pos, out, scratch)?;
            } else {
                let len = be_i32(raw, *pos)?;
                *pos += 4;
                if len < 0 {
                    out.push_str("NULL");
                } else {
                    let v = raw.get(*pos..*pos + len as usize)?;
                    *pos += len as usize;
                    scratch.clear();
                    render(elem, v, scratch);
                    push_quoted(out, scratch);
                }
            }
        }
        out.push('}');
        Some(())
    }
    level(elem, raw, &dims, &mut pos, out, &mut scratch)
}

fn render_range(elem: &Type, raw: &[u8], out: &mut String) -> Option<()> {
    let flags = *raw.first()?;
    if flags & 0x01 != 0 {
        out.push_str("empty");
        return Some(());
    }
    let mut pos = 1;
    let mut bound = |out: &mut String, inf: bool| -> Option<()> {
        if inf {
            return Some(());
        }
        let len = be_i32(raw, pos)? as usize;
        pos += 4;
        let mut s = String::new();
        render(elem, raw.get(pos..pos + len)?, &mut s);
        pos += len;
        push_quoted(out, &s);
        Some(())
    };
    out.push(if flags & 0x02 != 0 { '[' } else { '(' });
    bound(out, flags & 0x08 != 0)?;
    out.push(',');
    bound(out, flags & 0x10 != 0)?;
    out.push(if flags & 0x04 != 0 { ']' } else { ')' });
    Some(())
}

fn render_composite(types: &[&Type], raw: &[u8], out: &mut String) -> Option<()> {
    let n = be_i32(raw, 0)? as usize;
    let mut pos = 4;
    out.push('(');
    for i in 0..n {
        if i > 0 {
            out.push(',');
        }
        pos += 4; // field type oid
        let len = be_i32(raw, pos)?;
        pos += 4;
        if len >= 0 {
            let v = raw.get(pos..pos + len as usize)?;
            pos += len as usize;
            let mut s = String::new();
            match types.get(i) {
                Some(t) => render(t, v, &mut s),
                None => s.push_str(&String::from_utf8_lossy(v)),
            }
            push_quoted(out, &s);
        }
    }
    out.push(')');
    Some(())
}

fn render_hstore(raw: &[u8], out: &mut String) -> Option<()> {
    let n = be_i32(raw, 0)? as usize;
    let mut pos = 4;
    for i in 0..n {
        if i > 0 {
            out.push_str(", ");
        }
        let klen = be_i32(raw, pos)? as usize;
        pos += 4;
        let k = std::str::from_utf8(raw.get(pos..pos + klen)?).ok()?;
        pos += klen;
        let vlen = be_i32(raw, pos)?;
        pos += 4;
        write!(out, "\"{k}\"=>").ok()?;
        if vlen < 0 {
            out.push_str("NULL");
        } else {
            let v = std::str::from_utf8(raw.get(pos..pos + vlen as usize)?).ok()?;
            pos += vlen as usize;
            write!(out, "\"{v}\"").ok()?;
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric(ndigits: i16, weight: i16, sign: u16, dscale: u16, digits: &[i16]) -> String {
        let mut raw = Vec::new();
        for v in [ndigits as u16, weight as u16, sign, dscale] {
            raw.extend_from_slice(&v.to_be_bytes());
        }
        for d in digits {
            raw.extend_from_slice(&d.to_be_bytes());
        }
        let mut s = String::new();
        render_numeric(&raw, &mut s).unwrap();
        s
    }

    #[test]
    fn numeric_rendering() {
        assert_eq!(numeric(2, 0, 0, 2, &[4812, 4000]), "4812.40");
        assert_eq!(numeric(1, 1, 0, 0, &[1]), "10000");
        assert_eq!(numeric(1, -1, 0, 4, &[5]), "0.0005");
        assert_eq!(numeric(1, -2, 0x4000, 6, &[12]), "-0.000000");
        assert_eq!(numeric(0, 0, 0, 0, &[]), "0");
        assert_eq!(numeric(0, 0, 0xC000, 0, &[]), "NaN");
        assert_eq!(numeric(3, 1, 0, 3, &[12, 3456, 7890]), "123456.789");
    }

    #[test]
    fn interval_rendering() {
        let mut s = String::new();
        render_interval(3_723_500_000, 3, 14, &mut s);
        assert_eq!(s, "1 year 2 mons 3 days 01:02:03.5");
        s.clear();
        render_interval(0, 0, 0, &mut s);
        assert_eq!(s, "00:00:00");
    }

    #[test]
    fn array_rendering() {
        // int4[] {1,NULL,3}
        let mut raw = Vec::new();
        for v in [1i32, 1, 23, 3, 1] {
            raw.extend_from_slice(&v.to_be_bytes());
        }
        for v in [Some(1i32), None, Some(3)] {
            match v {
                Some(x) => {
                    raw.extend_from_slice(&4i32.to_be_bytes());
                    raw.extend_from_slice(&x.to_be_bytes());
                }
                None => raw.extend_from_slice(&(-1i32).to_be_bytes()),
            }
        }
        let mut s = String::new();
        render(&Type::INT4_ARRAY, &raw, &mut s);
        assert_eq!(s, "{1,NULL,3}");
    }

    #[test]
    fn inet_rendering() {
        let mut s = String::new();
        render(&Type::INET, &[2, 32, 0, 4, 10, 0, 4, 12], &mut s);
        assert_eq!(s, "10.0.4.12");
        s.clear();
        render(&Type::CIDR, &[2, 24, 1, 4, 10, 0, 4, 0], &mut s);
        assert_eq!(s, "10.0.4.0/24");
    }
}
