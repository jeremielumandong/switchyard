//! SQL API v2 request and response shapes, and decoding of its `jsonv2` cells (every
//! value arrives as a string or null).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::batch::{ColumnMeta, RowBatch, RowBatchBuilder};
use crate::stream::DEFAULT_BATCH_ROWS;
use crate::value::DataType;

/// `POST /api/v2/statements` body.
#[derive(Serialize)]
pub struct SubmitRequest<'a> {
    /// SQL text (several statements when `MULTI_STATEMENT_COUNT` is 0).
    pub statement: &'a str,
    /// Server-side timeout in seconds (0 = the account's maximum).
    pub timeout: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warehouse: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'a str>,
    /// Positional bindings `{"1": {"type": "TEXT", "value": "..."}}`.
    #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
    pub bindings: serde_json::Map<String, Json>,
    /// Session parameters for this request.
    pub parameters: serde_json::Map<String, Json>,
}

/// One column of `resultSetMetaData.rowType`.
#[derive(Clone, Debug, Deserialize)]
pub struct RowType {
    pub name: String,
    #[serde(rename = "type", default)]
    pub ty: String,
    #[serde(default)]
    pub scale: Option<i64>,
    #[serde(default)]
    pub precision: Option<i64>,
}

/// One result partition (only their number matters here).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Partition {}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    #[serde(default)]
    pub row_type: Vec<RowType>,
    #[serde(default)]
    pub partition_info: Vec<Partition>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    #[serde(default)]
    pub num_rows_inserted: u64,
    #[serde(default)]
    pub num_rows_updated: u64,
    #[serde(default)]
    pub num_rows_deleted: u64,
}

/// Any SQL API answer: a result set, "still running", or a failure.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub sql_state: Option<String>,
    #[serde(default)]
    pub statement_handle: Option<String>,
    /// Set when the request held several statements.
    #[serde(default)]
    pub statement_handles: Option<Vec<String>>,
    #[serde(default)]
    pub result_set_meta_data: Option<Metadata>,
    #[serde(default)]
    pub data: Option<Vec<Vec<Json>>>,
    #[serde(default)]
    pub stats: Option<Stats>,
}

/// "Asynchronous execution in progress".
pub const CODE_RUNNING: &str = "333334";

/// How one Snowflake type is carried in a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Int,
    Decimal,
    Float,
    Text,
    Bool,
    Binary,
    Date,
    Time,
    /// `TIMESTAMP_NTZ`.
    Timestamp,
    /// `TIMESTAMP_LTZ` (an instant).
    TimestampLtz,
    /// `TIMESTAMP_TZ` (`seconds.nanos offset+1440`).
    TimestampTz,
    Json,
    Other,
}

impl Kind {
    fn of(t: &RowType) -> Self {
        match t.ty.to_ascii_lowercase().as_str() {
            "fixed" if t.scale.unwrap_or(0) == 0 && t.precision.unwrap_or(38) <= 18 => Kind::Int,
            "fixed" => Kind::Decimal,
            "real" | "float" | "double" => Kind::Float,
            "text" => Kind::Text,
            "boolean" => Kind::Bool,
            "binary" => Kind::Binary,
            "date" => Kind::Date,
            "time" => Kind::Time,
            "timestamp_ntz" => Kind::Timestamp,
            "timestamp_ltz" => Kind::TimestampLtz,
            "timestamp_tz" => Kind::TimestampTz,
            "variant" | "object" | "array" => Kind::Json,
            _ => Kind::Other,
        }
    }

    fn data_type(self) -> DataType {
        match self {
            Kind::Int => DataType::Int64,
            Kind::Decimal => DataType::Numeric,
            Kind::Float => DataType::Float64,
            Kind::Text => DataType::Text,
            Kind::Bool => DataType::Bool,
            Kind::Binary => DataType::Bytes,
            Kind::Date => DataType::Date,
            Kind::Time => DataType::Time,
            Kind::Timestamp => DataType::Timestamp,
            Kind::TimestampLtz | Kind::TimestampTz => DataType::TimestampTz,
            Kind::Json => DataType::Json,
            Kind::Other => DataType::Other,
        }
    }
}

/// Display type name for the grid header.
fn type_name(t: &RowType) -> String {
    let ty = t.ty.to_ascii_uppercase();
    match ty.as_str() {
        "FIXED" => match (t.precision, t.scale) {
            (Some(p), Some(s)) => format!("NUMBER({p},{s})"),
            _ => "NUMBER".into(),
        },
        "REAL" => "FLOAT".into(),
        "TEXT" => "VARCHAR".into(),
        _ => ty,
    }
}

/// Column metadata and per-column decoders for a result set.
pub fn columns(row_type: &[RowType]) -> (Arc<[ColumnMeta]>, Vec<Kind>) {
    let kinds: Vec<Kind> = row_type.iter().map(Kind::of).collect();
    let meta: Vec<ColumnMeta> = row_type
        .iter()
        .zip(&kinds)
        .map(|(t, k)| ColumnMeta::new(t.name.clone(), type_name(t), k.data_type()))
        .collect();
    (Arc::from(meta), kinds)
}

/// `seconds[.fraction]` to microseconds, keeping the sign of the whole value.
pub fn seconds_to_micros(s: &str) -> Option<i64> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let secs: i64 = int.parse().ok()?;
    let mut micros = 0i64;
    for (i, d) in frac.bytes().take(6).enumerate() {
        if !d.is_ascii_digit() {
            return None;
        }
        micros += i64::from(d - b'0') * 10i64.pow(5 - i as u32);
    }
    let total = secs.checked_mul(1_000_000)?.checked_add(micros)?;
    Some(if neg { -total } else { total })
}

fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn push_cell(b: &mut RowBatchBuilder, kind: Kind, v: Option<&Json>) {
    let text = match v {
        None | Some(Json::Null) => {
            b.push_null();
            return;
        }
        Some(Json::String(s)) => s.as_str(),
        Some(other) => {
            // jsonv2 sends strings; anything else is kept as its JSON text.
            match kind {
                Kind::Int | Kind::Decimal | Kind::Float | Kind::Bool | Kind::Json => {
                    let s = other.to_string();
                    return push_cell(b, kind, Some(&Json::String(s)));
                }
                _ => {
                    b.push_str(&other.to_string());
                    return;
                }
            }
        }
    };
    let ok = match kind {
        Kind::Int => text.parse::<i64>().map(|n| b.push_i64(n)).is_ok(),
        Kind::Float => match text {
            "inf" | "Infinity" => Some(f64::INFINITY),
            "-inf" | "-Infinity" => Some(f64::NEG_INFINITY),
            "NaN" | "nan" => Some(f64::NAN),
            t => t.parse::<f64>().ok(),
        }
        .map(|f| b.push_f64(f))
        .is_some(),
        Kind::Bool => match text.to_ascii_lowercase().as_str() {
            "true" | "1" => {
                b.push_bool(true);
                true
            }
            "false" | "0" => {
                b.push_bool(false);
                true
            }
            _ => false,
        },
        Kind::Binary => hex_bytes(text).map(|x| b.push_bytes(&x)).is_some(),
        Kind::Date => text.parse::<i64>().map(|d| b.push_i64(d)).is_ok(),
        Kind::Time | Kind::Timestamp | Kind::TimestampLtz => {
            seconds_to_micros(text).map(|t| b.push_i64(t)).is_some()
        }
        // The epoch part is already UTC; the offset only says how to display it.
        Kind::TimestampTz => seconds_to_micros(text.split(' ').next().unwrap_or(text))
            .map(|t| b.push_i64(t))
            .is_some(),
        Kind::Decimal | Kind::Text | Kind::Json | Kind::Other => {
            b.push_str(text);
            true
        }
    };
    if !ok {
        // A value the column type cannot hold: keep the row aligned.
        b.push_null();
    }
}

/// Columnar batches for some rows of a result set.
pub fn batches(kinds: &[Kind], meta: &[ColumnMeta], rows: &[Vec<Json>]) -> Vec<RowBatch> {
    rows.chunks(DEFAULT_BATCH_ROWS)
        .map(|chunk| {
            let mut b = RowBatchBuilder::for_columns(meta, chunk.len());
            for row in chunk {
                for (i, k) in kinds.iter().enumerate() {
                    push_cell(&mut b, *k, row.get(i));
                }
            }
            b.finish()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::BatchList;
    use crate::value::Value;
    use serde_json::json;

    #[test]
    fn decodes_every_jsonv2_kind() {
        let row_type: Vec<RowType> = serde_json::from_value(json!([
            {"name": "ID", "type": "fixed", "precision": 18, "scale": 0},
            {"name": "AMOUNT", "type": "fixed", "precision": 38, "scale": 2},
            {"name": "F", "type": "real"},
            {"name": "OK", "type": "boolean"},
            {"name": "B", "type": "binary"},
            {"name": "D", "type": "date"},
            {"name": "T", "type": "time"},
            {"name": "TS", "type": "timestamp_ntz"},
            {"name": "TZ", "type": "timestamp_tz"},
            {"name": "V", "type": "variant"},
            {"name": "S", "type": "text"}
        ]))
        .expect("row type");
        let (meta, kinds) = columns(&row_type);
        assert_eq!(meta[1].type_name, "NUMBER(38,2)");
        let rows = vec![
            vec![
                json!("42"),
                json!("12.50"),
                json!("1.5"),
                json!("true"),
                json!("dead"),
                json!("1"),
                json!("3661.250000000"),
                json!("86400.000001000"),
                json!("0.000000000 1500"),
                json!("{\"a\":1}"),
                json!("hi"),
            ],
            vec![Json::Null; 11],
        ];
        let mut list = BatchList::default();
        for b in batches(&kinds, &meta, &rows) {
            list.push(b);
        }
        let v = |r: usize, c: usize| {
            list.cell(r, c)
                .map(|cell| cell.to_value(meta[c].data_type))
                .expect("cell")
        };
        assert_eq!(v(0, 0), Value::Int(42));
        assert_eq!(v(0, 1), Value::Numeric("12.50".into()));
        assert_eq!(v(0, 2), Value::Float(1.5));
        assert_eq!(v(0, 3), Value::Bool(true));
        assert_eq!(v(0, 4), Value::Bytes(vec![0xde, 0xad]));
        assert_eq!(v(0, 5), Value::Date(1));
        assert_eq!(v(0, 6), Value::Time(3_661_250_000));
        assert_eq!(v(0, 7), Value::Timestamp(86_400_000_001));
        assert_eq!(v(0, 8), Value::TimestampTz(0));
        assert_eq!(v(0, 9), Value::Json("{\"a\":1}".into()));
        assert_eq!(v(0, 10), Value::Text("hi".into()));
        for c in 0..11 {
            assert_eq!(v(1, c), Value::Null, "column {c}");
        }
    }

    #[test]
    fn negative_and_short_fractions() {
        assert_eq!(seconds_to_micros("-1.5"), Some(-1_500_000));
        assert_eq!(seconds_to_micros("2"), Some(2_000_000));
        assert_eq!(seconds_to_micros("0.123456789"), Some(123_456));
        assert_eq!(seconds_to_micros("x"), None);
    }

    #[test]
    fn wide_integers_stay_exact() {
        let row_type: Vec<RowType> = serde_json::from_value(
            json!([{"name": "N", "type": "fixed", "precision": 38, "scale": 0}]),
        )
        .expect("row type");
        let (meta, kinds) = columns(&row_type);
        assert_eq!(meta[0].data_type, DataType::Numeric);
        assert_eq!(kinds, [Kind::Decimal]);
    }
}
