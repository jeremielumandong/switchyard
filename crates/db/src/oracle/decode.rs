//! Oracle column types to Switchyard's columnar batches.

use std::sync::Arc;

use oracle::sql_type::{OracleType, Timestamp};
use oracle::{ColumnInfo, Row};

use crate::batch::{ColumnMeta, RowBatchBuilder};
use crate::value::{DataType, days_from_civil};

/// How one column is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Int,
    Decimal,
    Float,
    Text,
    Bytes,
    Bool,
    /// `DATE` and `TIMESTAMP` (no zone).
    Timestamp,
    /// `TIMESTAMP WITH [LOCAL] TIME ZONE`, normalized to UTC.
    TimestampTz,
    Json,
    /// Intervals, objects and the rest, as the client renders them.
    Other,
}

impl Kind {
    fn of(t: &OracleType) -> Self {
        match t {
            // NUMBER(p, 0) that fits an i64.
            OracleType::Number(p, 0) if (1..=18).contains(p) => Kind::Int,
            OracleType::Int64 | OracleType::UInt64 => Kind::Int,
            OracleType::Number(..) | OracleType::Float(_) => Kind::Decimal,
            OracleType::BinaryFloat | OracleType::BinaryDouble => Kind::Float,
            OracleType::Varchar2(_)
            | OracleType::NVarchar2(_)
            | OracleType::Char(_)
            | OracleType::NChar(_)
            | OracleType::Long
            | OracleType::CLOB
            | OracleType::NCLOB
            | OracleType::Rowid
            | OracleType::Xml => Kind::Text,
            OracleType::Raw(_) | OracleType::LongRaw | OracleType::BLOB => Kind::Bytes,
            OracleType::Boolean => Kind::Bool,
            OracleType::Date | OracleType::Timestamp(_) => Kind::Timestamp,
            OracleType::TimestampTZ(_) | OracleType::TimestampLTZ(_) => Kind::TimestampTz,
            OracleType::Json => Kind::Json,
            _ => Kind::Other,
        }
    }

    fn data_type(self) -> DataType {
        match self {
            Kind::Int => DataType::Int64,
            Kind::Decimal => DataType::Numeric,
            Kind::Float => DataType::Float64,
            Kind::Text => DataType::Text,
            Kind::Bytes => DataType::Bytes,
            Kind::Bool => DataType::Bool,
            Kind::Timestamp => DataType::Timestamp,
            Kind::TimestampTz => DataType::TimestampTz,
            Kind::Json => DataType::Json,
            Kind::Other => DataType::Other,
        }
    }
}

/// Column metadata and readers for a result set.
pub(super) fn columns(info: &[ColumnInfo]) -> (Arc<[ColumnMeta]>, Vec<Kind>) {
    let kinds: Vec<Kind> = info.iter().map(|c| Kind::of(c.oracle_type())).collect();
    let meta: Vec<ColumnMeta> = info
        .iter()
        .zip(&kinds)
        .map(|(c, k)| ColumnMeta::new(c.name(), c.oracle_type().to_string(), k.data_type()))
        .collect();
    (Arc::from(meta), kinds)
}

/// Microseconds since 1970-01-01 for a wall-clock timestamp.
pub(super) fn micros(t: &Timestamp) -> i64 {
    let days = days_from_civil(i64::from(t.year()), t.month(), t.day());
    let secs = days * 86_400
        + i64::from(t.hour()) * 3600
        + i64::from(t.minute()) * 60
        + i64::from(t.second());
    secs * 1_000_000 + i64::from(t.nanosecond() / 1000)
}

/// Append one row.
pub(super) fn push_row(b: &mut RowBatchBuilder, kinds: &[Kind], row: &Row) {
    for (i, kind) in kinds.iter().enumerate() {
        let ok = match kind {
            Kind::Int => match row.get::<usize, Option<i64>>(i) {
                Ok(Some(v)) => {
                    b.push_i64(v);
                    true
                }
                Ok(None) => {
                    b.push_null();
                    true
                }
                Err(_) => false,
            },
            Kind::Float => match row.get::<usize, Option<f64>>(i) {
                Ok(Some(v)) => {
                    b.push_f64(v);
                    true
                }
                Ok(None) => {
                    b.push_null();
                    true
                }
                Err(_) => false,
            },
            Kind::Bool => match row.get::<usize, Option<bool>>(i) {
                Ok(Some(v)) => {
                    b.push_bool(v);
                    true
                }
                Ok(None) => {
                    b.push_null();
                    true
                }
                Err(_) => false,
            },
            Kind::Bytes => match row.get::<usize, Option<Vec<u8>>>(i) {
                Ok(Some(v)) => {
                    b.push_bytes(&v);
                    true
                }
                Ok(None) => {
                    b.push_null();
                    true
                }
                Err(_) => false,
            },
            Kind::Timestamp | Kind::TimestampTz => match row.get::<usize, Option<Timestamp>>(i) {
                Ok(Some(t)) => {
                    let mut us = micros(&t);
                    if *kind == Kind::TimestampTz {
                        us -= i64::from(t.tz_offset()) * 1_000_000;
                    }
                    b.push_i64(us);
                    true
                }
                Ok(None) => {
                    b.push_null();
                    true
                }
                Err(_) => false,
            },
            Kind::Decimal | Kind::Text | Kind::Json | Kind::Other => {
                match row.get::<usize, Option<String>>(i) {
                    Ok(Some(v)) => {
                        b.push_str(&v);
                        true
                    }
                    Ok(None) => {
                        b.push_null();
                        true
                    }
                    Err(_) => false,
                }
            }
        };
        if !ok {
            // A value the column type cannot hold: show what the client renders.
            match row.sql_values().get(i) {
                Some(v)
                    if matches!(kind, Kind::Decimal | Kind::Text | Kind::Json | Kind::Other) =>
                {
                    b.push_str(&v.to_string())
                }
                _ => b.push_null(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_for_common_types() {
        assert_eq!(Kind::of(&OracleType::Number(10, 0)), Kind::Int);
        assert_eq!(Kind::of(&OracleType::Number(38, 0)), Kind::Decimal);
        assert_eq!(Kind::of(&OracleType::Number(0, -127)), Kind::Decimal);
        assert_eq!(Kind::of(&OracleType::Number(8, 2)), Kind::Decimal);
        assert_eq!(Kind::of(&OracleType::BinaryDouble), Kind::Float);
        assert_eq!(Kind::of(&OracleType::Varchar2(20)), Kind::Text);
        assert_eq!(Kind::of(&OracleType::Date), Kind::Timestamp);
        assert_eq!(Kind::of(&OracleType::TimestampTZ(6)), Kind::TimestampTz);
        assert_eq!(Kind::of(&OracleType::BLOB), Kind::Bytes);
    }

    #[test]
    fn timestamp_micros() {
        let t = Timestamp::new(1970, 1, 2, 0, 0, 1, 500_000).expect("timestamp");
        assert_eq!(micros(&t), 86_401_000_500);
    }
}
