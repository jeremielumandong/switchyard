//! SQL Server values to and from columnar batches.

use tiberius::{Column, ColumnData, ColumnType, Query};

use crate::batch::{ColumnMeta, RowBatchBuilder};
use crate::value::{DataType, Value, days_from_civil};

/// Days from 0001-01-01 (SQL Server `date` epoch) to 1970-01-01.
const DAYS_0001_TO_1970: i64 = 719_162;
/// Days from 1900-01-01 (legacy `datetime` epoch) to 1970-01-01.
const DAYS_1900_TO_1970: i64 = 25_567;
const US_PER_DAY: i64 = 86_400_000_000;

/// Engine type name and logical type for a result column.
pub(super) fn column_meta(c: &Column) -> ColumnMeta {
    let (name, dt) = match c.column_type() {
        ColumnType::Bit | ColumnType::Bitn => ("bit", DataType::Bool),
        ColumnType::Int1 => ("tinyint", DataType::Int16),
        ColumnType::Int2 => ("smallint", DataType::Int16),
        ColumnType::Int4 => ("int", DataType::Int32),
        ColumnType::Int8 => ("bigint", DataType::Int64),
        // Width unknown until a value arrives; 64 bits holds them all.
        ColumnType::Intn => ("int", DataType::Int64),
        ColumnType::Float4 => ("real", DataType::Float64),
        ColumnType::Float8 | ColumnType::Floatn => ("float", DataType::Float64),
        ColumnType::Money => ("money", DataType::Numeric),
        ColumnType::Money4 => ("smallmoney", DataType::Numeric),
        ColumnType::Decimaln => ("decimal", DataType::Numeric),
        ColumnType::Numericn => ("numeric", DataType::Numeric),
        ColumnType::Guid => ("uniqueidentifier", DataType::Uuid),
        ColumnType::Datetime4 => ("smalldatetime", DataType::Timestamp),
        ColumnType::Datetime | ColumnType::Datetimen => ("datetime", DataType::Timestamp),
        ColumnType::Datetime2 => ("datetime2", DataType::Timestamp),
        ColumnType::Daten => ("date", DataType::Date),
        ColumnType::Timen => ("time", DataType::Time),
        ColumnType::DatetimeOffsetn => ("datetimeoffset", DataType::TimestampTz),
        ColumnType::BigVarBin => ("varbinary", DataType::Bytes),
        ColumnType::BigBinary => ("binary", DataType::Bytes),
        ColumnType::Image => ("image", DataType::Bytes),
        ColumnType::BigVarChar => ("varchar", DataType::Text),
        ColumnType::BigChar => ("char", DataType::Text),
        ColumnType::NVarchar => ("nvarchar", DataType::Text),
        ColumnType::NChar => ("nchar", DataType::Text),
        ColumnType::Text => ("text", DataType::Text),
        ColumnType::NText => ("ntext", DataType::Text),
        ColumnType::Xml => ("xml", DataType::Xml),
        ColumnType::Udt => ("udt", DataType::Other),
        ColumnType::SSVariant => ("sql_variant", DataType::Other),
        ColumnType::Null => ("null", DataType::Other),
    };
    ColumnMeta::new(c.name(), name, dt)
}

fn date_days(days_since_0001: i64) -> i64 {
    days_since_0001 - DAYS_0001_TO_1970
}

fn time_us(increments: u64, scale: u8) -> i64 {
    // increments are 10^-scale seconds since midnight.
    let mut v = increments as i128 * 1_000_000;
    for _ in 0..scale {
        v /= 10;
    }
    v as i64
}

/// A timestamp from a legacy `datetime` (days since 1900, 1/300 s ticks), rounded to the
/// millisecond the way SQL Server shows it (`.123`, not `.123333`).
fn legacy_datetime(days: i64, ticks_300: i64) -> i64 {
    let ms = (ticks_300 * 10 + 1) / 3;
    (days - DAYS_1900_TO_1970) * US_PER_DAY + ms * 1000
}

/// The value as an owned [`Value`].
pub(super) fn to_value(d: &ColumnData<'_>) -> Value {
    match d {
        ColumnData::U8(v) => v.map_or(Value::Null, |x| Value::Int(i64::from(x))),
        ColumnData::I16(v) => v.map_or(Value::Null, |x| Value::Int(i64::from(x))),
        ColumnData::I32(v) => v.map_or(Value::Null, |x| Value::Int(i64::from(x))),
        ColumnData::I64(v) => v.map_or(Value::Null, Value::Int),
        ColumnData::F32(v) => v.map_or(Value::Null, |x| Value::Float(f64::from(x))),
        ColumnData::F64(v) => v.map_or(Value::Null, Value::Float),
        ColumnData::Bit(v) => v.map_or(Value::Null, Value::Bool),
        ColumnData::String(v) => v
            .as_ref()
            .map_or(Value::Null, |s| Value::Text(s.to_string())),
        ColumnData::Guid(v) => v.map_or(Value::Null, |u| Value::Uuid(*u.as_bytes())),
        ColumnData::Binary(v) => v.as_ref().map_or(Value::Null, |b| Value::Bytes(b.to_vec())),
        ColumnData::Numeric(v) => v.map_or(Value::Null, |n| Value::Numeric(n.to_string())),
        ColumnData::Xml(v) => v
            .as_ref()
            .map_or(Value::Null, |x| Value::Other(x.to_string())),
        ColumnData::DateTime(v) => v.map_or(Value::Null, |t| {
            Value::Timestamp(legacy_datetime(
                i64::from(t.days()),
                i64::from(t.seconds_fragments()),
            ))
        }),
        ColumnData::SmallDateTime(v) => v.map_or(Value::Null, |t| {
            Value::Timestamp(
                (i64::from(t.days()) - DAYS_1900_TO_1970) * US_PER_DAY
                    + i64::from(t.seconds_fragments()) * 60_000_000,
            )
        }),
        ColumnData::Time(v) => v.map_or(Value::Null, |t| {
            Value::Time(time_us(t.increments(), t.scale()))
        }),
        ColumnData::Date(v) => v.map_or(Value::Null, |d| {
            Value::Date(date_days(i64::from(d.days())) as i32)
        }),
        ColumnData::DateTime2(v) => v.map_or(Value::Null, |t| {
            Value::Timestamp(
                date_days(i64::from(t.date().days())) * US_PER_DAY
                    + time_us(t.time().increments(), t.time().scale()),
            )
        }),
        ColumnData::DateTimeOffset(v) => v.map_or(Value::Null, |t| {
            // The stored date and time are UTC; the offset only says how it was written.
            let dt = t.datetime2();
            Value::TimestampTz(
                date_days(i64::from(dt.date().days())) * US_PER_DAY
                    + time_us(dt.time().increments(), dt.time().scale()),
            )
        }),
    }
}

/// Append one cell to a column of logical type `dt`.
pub(super) fn push(b: &mut RowBatchBuilder, dt: DataType, d: &ColumnData<'_>) {
    match (dt, d) {
        // Hot paths without an intermediate `Value`.
        (_, ColumnData::I32(Some(x))) => b.push_i64(i64::from(*x)),
        (_, ColumnData::I64(Some(x))) => b.push_i64(*x),
        (DataType::Text, ColumnData::String(Some(s))) => b.push_str(s),
        _ => b.push_value(&to_value(d)),
    }
}

/// Bind a parameter value.
pub(super) fn bind(q: &mut Query<'_>, v: &Value) {
    match v {
        Value::Null => q.bind(Option::<String>::None),
        Value::Bool(x) => q.bind(*x),
        Value::Int(x) => q.bind(*x),
        Value::Float(x) => q.bind(*x),
        Value::Bytes(x) => q.bind(x.clone()),
        Value::Text(s) | Value::Numeric(s) | Value::Json(s) | Value::Other(s) => q.bind(s.clone()),
        other => q.bind(other.to_display()),
    }
}

/// `YYYY-MM-DD` → days since 1970 (tests).
#[allow(dead_code)]
pub(super) fn civil_days(y: i64, m: u32, d: u32) -> i64 {
    days_from_civil(y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epochs() {
        assert_eq!(date_days(DAYS_0001_TO_1970), 0);
        assert_eq!(civil_days(1900, 1, 1), -DAYS_1900_TO_1970);
        assert_eq!(civil_days(1, 1, 1), -DAYS_0001_TO_1970);
        // 1234567 × 10^-7 s = 123456.7 µs
        assert_eq!(time_us(1_234_567, 7), 123_456);
        assert_eq!(time_us(5, 0), 5_000_000);
        // 1900-01-01 00:00:01 = 300 ticks.
        assert_eq!(
            legacy_datetime(0, 300),
            -DAYS_1900_TO_1970 * US_PER_DAY + 1_000_000
        );
    }
}
