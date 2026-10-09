//! MySQL values to and from columnar batches.
//!
//! The text protocol (plain queries) sends every value as bytes; the binary protocol
//! (prepared statements with parameters) sends typed values. Both land here.

use mysql_async::Column;
use mysql_async::Value as MyValue;
use mysql_async::consts::{ColumnFlags, ColumnType};

use crate::batch::{ColumnMeta, RowBatchBuilder};
use crate::value::{self, DataType, Value, civil_from_days, days_from_civil};

/// Character set id of binary strings (`BINARY`, `VARBINARY`, `BLOB`).
const BINARY_CHARSET: u16 = 63;
const US_PER_DAY: i64 = 86_400_000_000;

/// Engine type name and logical type for a result column.
pub(super) fn column_meta(c: &Column) -> ColumnMeta {
    let (name, dt) = column_type(
        c.column_type(),
        c.flags(),
        c.character_set(),
        c.column_length(),
    );
    let name = if c.flags().contains(ColumnFlags::UNSIGNED_FLAG) && dt.is_numeric() {
        format!("{name} unsigned")
    } else {
        name.to_owned()
    };
    ColumnMeta::new(c.name_str(), name, dt)
}

/// Type name and logical type from the column definition.
fn column_type(
    ty: ColumnType,
    flags: ColumnFlags,
    charset: u16,
    length: u32,
) -> (&'static str, DataType) {
    let unsigned = flags.contains(ColumnFlags::UNSIGNED_FLAG);
    let binary = charset == BINARY_CHARSET;
    match ty {
        ColumnType::MYSQL_TYPE_TINY => ("tinyint", DataType::Int16),
        ColumnType::MYSQL_TYPE_SHORT if unsigned => ("smallint", DataType::Int32),
        ColumnType::MYSQL_TYPE_SHORT => ("smallint", DataType::Int16),
        ColumnType::MYSQL_TYPE_INT24 => ("mediumint", DataType::Int32),
        ColumnType::MYSQL_TYPE_LONG if unsigned => ("int", DataType::Int64),
        ColumnType::MYSQL_TYPE_LONG => ("int", DataType::Int32),
        // An unsigned bigint can exceed i64; keep it exact as text.
        ColumnType::MYSQL_TYPE_LONGLONG if unsigned => ("bigint", DataType::Numeric),
        ColumnType::MYSQL_TYPE_LONGLONG => ("bigint", DataType::Int64),
        ColumnType::MYSQL_TYPE_YEAR => ("year", DataType::Int16),
        ColumnType::MYSQL_TYPE_FLOAT => ("float", DataType::Float32),
        ColumnType::MYSQL_TYPE_DOUBLE => ("double", DataType::Float64),
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => {
            ("decimal", DataType::Numeric)
        }
        ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => ("date", DataType::Date),
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_DATETIME2 => {
            ("datetime", DataType::Timestamp)
        }
        ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2 => {
            ("timestamp", DataType::Timestamp)
        }
        // TIME is a duration (-838:59:59 to 838:59:59), not a time of day.
        ColumnType::MYSQL_TYPE_TIME | ColumnType::MYSQL_TYPE_TIME2 => ("time", DataType::Other),
        ColumnType::MYSQL_TYPE_BIT if length == 1 => ("bit", DataType::Bool),
        ColumnType::MYSQL_TYPE_BIT => ("bit", DataType::Bytes),
        ColumnType::MYSQL_TYPE_JSON => ("json", DataType::Json),
        ColumnType::MYSQL_TYPE_ENUM => ("enum", DataType::Text),
        ColumnType::MYSQL_TYPE_SET => ("set", DataType::Text),
        _ if flags.contains(ColumnFlags::ENUM_FLAG) => ("enum", DataType::Text),
        _ if flags.contains(ColumnFlags::SET_FLAG) => ("set", DataType::Text),
        ColumnType::MYSQL_TYPE_STRING if binary => ("binary", DataType::Bytes),
        ColumnType::MYSQL_TYPE_STRING => ("char", DataType::Text),
        ColumnType::MYSQL_TYPE_VAR_STRING | ColumnType::MYSQL_TYPE_VARCHAR if binary => {
            ("varbinary", DataType::Bytes)
        }
        ColumnType::MYSQL_TYPE_VAR_STRING | ColumnType::MYSQL_TYPE_VARCHAR => {
            ("varchar", DataType::Text)
        }
        ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB
            if binary =>
        {
            ("blob", DataType::Bytes)
        }
        ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB => ("text", DataType::Text),
        ColumnType::MYSQL_TYPE_GEOMETRY => ("geometry", DataType::Bytes),
        ColumnType::MYSQL_TYPE_VECTOR => ("vector", DataType::Bytes),
        ColumnType::MYSQL_TYPE_NULL => ("null", DataType::Other),
        _ => ("unknown", DataType::Other),
    }
}

/// Days since 1970 of a calendar date; `None` for MySQL's zero dates (`0000-00-00`,
/// `2024-00-15`), which have no calendar day.
fn days(y: i64, m: u32, d: u32) -> Option<i64> {
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then(|| days_from_civil(y, m, d))
}

/// `YYYY-MM-DD[ HH:MM:SS[.ffffff]]` as (days, microseconds into the day).
fn parse_datetime(s: &str) -> Option<(i64, i64)> {
    let s = s.trim();
    let (date, time) = match s.split_once([' ', 'T']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut it = date.splitn(3, '-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    let day = days(y, m, d)?;
    let us = match time {
        Some(t) => parse_time_of_day(t)?,
        None => 0,
    };
    Some((day, us))
}

/// `HH:MM:SS[.ffffff]` as microseconds.
fn parse_time_of_day(t: &str) -> Option<i64> {
    let (hms, frac) = match t.split_once('.') {
        Some((a, b)) => (a, b),
        None => (t, ""),
    };
    let mut it = hms.splitn(3, ':');
    let h: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let s: i64 = it.next()?.parse().ok()?;
    let mut us: i64 = 0;
    let mut scale = 100_000;
    for c in frac.chars().take(6) {
        us += i64::from(c.to_digit(10)?) * scale;
        scale /= 10;
    }
    Some(((h * 60 + m) * 60 + s) * 1_000_000 + us)
}

/// A binary-protocol TIME as text: `[-]HHH:MM:SS[.ffffff]`.
fn time_text(neg: bool, days: u32, h: u8, m: u8, s: u8, us: u32) -> String {
    let hours = u64::from(days) * 24 + u64::from(h);
    let mut out = format!("{}{hours:02}:{m:02}:{s:02}", if neg { "-" } else { "" });
    if us != 0 {
        let mut f = format!("{us:06}");
        while f.ends_with('0') {
            f.pop();
        }
        out.push('.');
        out.push_str(&f);
    }
    out
}

/// A binary-protocol DATE / DATETIME as text (zero dates stay as the server writes them).
fn date_text(y: u16, mo: u8, d: u8, h: u8, mi: u8, s: u8, us: u32) -> String {
    let mut out = format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}");
    if us != 0 {
        out.push_str(&format!(".{us:06}"));
    }
    out
}

fn utf8(b: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(b)
}

/// Append one cell to a column of logical type `dt`. Zero dates (`0000-00-00`) become
/// NULL in date columns: they name no calendar day.
pub(super) fn push(b: &mut RowBatchBuilder, dt: DataType, v: Option<&MyValue>) {
    let Some(v) = v else {
        return b.push_null();
    };
    match (dt, v) {
        (_, MyValue::NULL) => b.push_null(),
        (DataType::Bool, MyValue::Int(i)) => b.push_bool(*i != 0),
        (DataType::Bool, MyValue::UInt(i)) => b.push_bool(*i != 0),
        (DataType::Bool, MyValue::Bytes(x)) => {
            // BIT(1) arrives as one raw byte; a text `0` / `1` from an expression too.
            b.push_bool(x.iter().any(|c| *c != 0 && *c != b'0'))
        }
        (DataType::Numeric, MyValue::UInt(u)) => b.push_str(&u.to_string()),
        (DataType::Numeric, MyValue::Int(i)) => b.push_str(&i.to_string()),
        (_, MyValue::Int(i)) => b.push_i64(*i),
        (_, MyValue::UInt(u)) => match i64::try_from(*u) {
            Ok(i) => b.push_i64(i),
            Err(_) => b.push_str(&u.to_string()),
        },
        (_, MyValue::Float(f)) => b.push_f64(f64::from(*f)),
        (_, MyValue::Double(f)) => b.push_f64(*f),
        (DataType::Date, MyValue::Date(y, mo, d, ..)) => {
            match days(i64::from(*y), u32::from(*mo), u32::from(*d)) {
                Some(n) => b.push_i64(n),
                None => b.push_null(),
            }
        }
        (DataType::Timestamp, MyValue::Date(y, mo, d, h, mi, s, us)) => {
            match days(i64::from(*y), u32::from(*mo), u32::from(*d)) {
                Some(n) => b.push_i64(
                    n * US_PER_DAY
                        + ((i64::from(*h) * 60 + i64::from(*mi)) * 60 + i64::from(*s)) * 1_000_000
                        + i64::from(*us),
                ),
                None => b.push_null(),
            }
        }
        (_, MyValue::Date(y, mo, d, h, mi, s, us)) => {
            b.push_str(&date_text(*y, *mo, *d, *h, *mi, *s, *us))
        }
        (_, MyValue::Time(neg, d, h, m, s, us)) => {
            b.push_str(&time_text(*neg, *d, *h, *m, *s, *us))
        }
        (DataType::Bytes, MyValue::Bytes(x)) => b.push_bytes(x),
        (DataType::Int16 | DataType::Int32 | DataType::Int64, MyValue::Bytes(x)) => {
            match utf8(x).trim().parse::<i64>() {
                Ok(i) => b.push_i64(i),
                Err(_) => b.push_null(),
            }
        }
        (DataType::Float32 | DataType::Float64, MyValue::Bytes(x)) => {
            match utf8(x).trim().parse::<f64>() {
                Ok(f) => b.push_f64(f),
                Err(_) => b.push_null(),
            }
        }
        (DataType::Date, MyValue::Bytes(x)) => match parse_datetime(&utf8(x)) {
            Some((d, _)) => b.push_i64(d),
            None => b.push_null(),
        },
        (DataType::Timestamp, MyValue::Bytes(x)) => match parse_datetime(&utf8(x)) {
            Some((d, us)) => b.push_i64(d * US_PER_DAY + us),
            None => b.push_null(),
        },
        (_, MyValue::Bytes(x)) => b.push_str(&utf8(x)),
    }
}

/// The value as an owned [`Value`] (catalog and version queries).
pub(super) fn to_value(v: &MyValue) -> Value {
    match v {
        MyValue::NULL => Value::Null,
        MyValue::Int(i) => Value::Int(*i),
        MyValue::UInt(u) => match i64::try_from(*u) {
            Ok(i) => Value::Int(i),
            Err(_) => Value::Numeric(u.to_string()),
        },
        MyValue::Float(f) => Value::Float(f64::from(*f)),
        MyValue::Double(f) => Value::Float(*f),
        MyValue::Bytes(b) => match String::from_utf8(b.clone()) {
            Ok(s) => Value::Text(s),
            Err(_) => Value::Bytes(b.clone()),
        },
        MyValue::Date(y, mo, d, h, mi, s, us) => {
            Value::Other(date_text(*y, *mo, *d, *h, *mi, *s, *us))
        }
        MyValue::Time(neg, d, h, m, s, us) => Value::Other(time_text(*neg, *d, *h, *m, *s, *us)),
    }
}

/// A date and time of day from days since 1970 and microseconds into the day.
fn my_date(days: i64, us: i64) -> MyValue {
    let (y, m, d) = civil_from_days(days);
    let secs = us / 1_000_000;
    MyValue::Date(
        y.clamp(0, 9999) as u16,
        m as u8,
        d as u8,
        (secs / 3600) as u8,
        ((secs / 60) % 60) as u8,
        (secs % 60) as u8,
        (us % 1_000_000) as u32,
    )
}

/// A parameter value for the binary protocol.
pub(super) fn param(v: &Value) -> MyValue {
    match v {
        Value::Null => MyValue::NULL,
        Value::Bool(b) => MyValue::Int(i64::from(*b)),
        Value::Int(i) => MyValue::Int(*i),
        Value::Float(f) => MyValue::Double(*f),
        Value::Numeric(s) | Value::Text(s) | Value::Json(s) | Value::Other(s) => {
            MyValue::Bytes(s.as_bytes().to_vec())
        }
        Value::Bytes(b) => MyValue::Bytes(b.clone()),
        Value::Uuid(_) => MyValue::Bytes(v.to_display().into_bytes()),
        Value::Date(d) => my_date(i64::from(*d), 0),
        Value::Time(t) => {
            let neg = *t < 0;
            let us = t.unsigned_abs();
            let secs = us / 1_000_000;
            MyValue::Time(
                neg,
                (secs / 86_400) as u32,
                ((secs / 3600) % 24) as u8,
                ((secs / 60) % 60) as u8,
                (secs % 60) as u8,
                (us % 1_000_000) as u32,
            )
        }
        Value::Timestamp(t) | Value::TimestampTz(t) => {
            my_date(t.div_euclid(US_PER_DAY), t.rem_euclid(US_PER_DAY))
        }
    }
}

/// Display text of a catalog value (empty for NULL).
pub(super) fn text(v: Option<&MyValue>) -> String {
    match v.map(to_value) {
        None | Some(Value::Null) => String::new(),
        Some(Value::Bytes(b)) => {
            let mut s = String::new();
            value::write_hex(&mut s, &b);
            s
        }
        Some(other) => other.to_display(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::ColumnMeta;

    fn cell(dt: DataType, v: MyValue) -> Value {
        let meta = [ColumnMeta::new("c", "t", dt)];
        let mut b = RowBatchBuilder::for_columns(&meta, 1);
        push(&mut b, dt, Some(&v));
        b.finish().cell(0, 0).to_value(dt)
    }

    #[test]
    fn text_protocol_values() {
        let bytes = |s: &str| MyValue::Bytes(s.as_bytes().to_vec());
        assert_eq!(cell(DataType::Int32, bytes("-42")), Value::Int(-42));
        assert_eq!(cell(DataType::Float64, bytes("1.5")), Value::Float(1.5));
        assert_eq!(
            cell(DataType::Numeric, bytes("18446744073709551615")),
            Value::Numeric("18446744073709551615".into())
        );
        assert_eq!(
            cell(DataType::Date, bytes("2025-10-05")),
            Value::Date(20_366)
        );
        assert_eq!(
            cell(DataType::Timestamp, bytes("2025-10-05 08:00:00.25")),
            Value::Timestamp(1_759_651_200_250_000)
        );
        assert_eq!(cell(DataType::Date, bytes("0000-00-00")), Value::Null);
        assert_eq!(
            cell(DataType::Bool, MyValue::Bytes(vec![1])),
            Value::Bool(true)
        );
        assert_eq!(
            cell(DataType::Bool, MyValue::Bytes(vec![0])),
            Value::Bool(false)
        );
        assert_eq!(
            cell(DataType::Text, bytes("héllo")),
            Value::Text("héllo".into())
        );
    }

    #[test]
    fn binary_protocol_values() {
        assert_eq!(
            cell(DataType::Timestamp, MyValue::Date(2025, 10, 5, 8, 0, 0, 1)),
            Value::Timestamp(1_759_651_200_000_001)
        );
        assert_eq!(
            cell(DataType::Date, MyValue::Date(0, 0, 0, 0, 0, 0, 0)),
            Value::Null
        );
        assert_eq!(
            cell(DataType::Other, MyValue::Time(true, 1, 2, 3, 4, 500_000)),
            Value::Other("-26:03:04.5".into())
        );
        assert_eq!(
            cell(DataType::Numeric, MyValue::UInt(u64::MAX)),
            Value::Numeric(u64::MAX.to_string())
        );
    }

    #[test]
    fn params_round_trip_dates() {
        assert_eq!(
            param(&Value::Timestamp(1_759_651_200_250_000)),
            MyValue::Date(2025, 10, 5, 8, 0, 0, 250_000)
        );
        assert_eq!(
            param(&Value::Date(20_366)),
            MyValue::Date(2025, 10, 5, 0, 0, 0, 0)
        );
        assert_eq!(
            param(&Value::Time(-3_723_000_000)),
            MyValue::Time(true, 0, 1, 2, 3, 0)
        );
        assert_eq!(param(&Value::Bool(true)), MyValue::Int(1));
    }

    #[test]
    fn column_types() {
        let none = ColumnFlags::empty();
        assert_eq!(
            column_type(
                ColumnType::MYSQL_TYPE_LONGLONG,
                ColumnFlags::UNSIGNED_FLAG,
                63,
                20
            )
            .1,
            DataType::Numeric
        );
        assert_eq!(
            column_type(ColumnType::MYSQL_TYPE_VAR_STRING, none, 63, 16).1,
            DataType::Bytes
        );
        assert_eq!(
            column_type(ColumnType::MYSQL_TYPE_VAR_STRING, none, 255, 16).1,
            DataType::Text
        );
        assert_eq!(
            column_type(
                ColumnType::MYSQL_TYPE_STRING,
                ColumnFlags::ENUM_FLAG,
                255,
                4
            ),
            ("enum", DataType::Text)
        );
        assert_eq!(
            column_type(ColumnType::MYSQL_TYPE_BIT, none, 63, 1).1,
            DataType::Bool
        );
    }
}
