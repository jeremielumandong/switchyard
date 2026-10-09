//! Documents as grid rows.
//!
//! Columns come from the first batch of documents: nested documents are flattened into
//! dotted paths (`address.city`) up to [`MAX_DEPTH`] levels, arrays stay whole as JSON,
//! and `_id` comes first. Each path gets the logical type its values share (`int`,
//! `double`, `date`, `objectId`, …) or `mixed`, shown as text. A last `(document)`
//! column holds every document as relaxed Extended JSON, so fields that only appear in
//! later batches, and values that do not fit their column's type, are never lost.

use std::collections::HashMap;
use std::sync::Arc;

use mongodb::bson::spec::BinarySubtype;
use mongodb::bson::{Bson, Document};

use crate::batch::{ColumnMeta, RowBatch, RowBatchBuilder};
use crate::value::DataType;

pub use crate::batch::DOCUMENT_COLUMN;

/// Nested documents deeper than this stay whole (as JSON) in one column.
pub const MAX_DEPTH: usize = 3;

/// Most flattened columns; further paths are only in the document column.
pub const MAX_COLUMNS: usize = 200;

/// The type a column's values share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Nothing,
    Bool,
    Int32,
    Int64,
    Double,
    Decimal,
    String,
    ObjectId,
    Date,
    Uuid,
    Binary,
    Json,
    Mixed,
}

fn kind_of(v: &Bson) -> Option<Kind> {
    Some(match v {
        Bson::Null | Bson::Undefined => return None,
        Bson::Boolean(_) => Kind::Bool,
        Bson::Int32(_) => Kind::Int32,
        Bson::Int64(_) => Kind::Int64,
        Bson::Double(_) => Kind::Double,
        Bson::Decimal128(_) => Kind::Decimal,
        Bson::String(_) | Bson::Symbol(_) => Kind::String,
        Bson::ObjectId(_) => Kind::ObjectId,
        Bson::DateTime(_) => Kind::Date,
        Bson::Binary(b) if b.subtype == BinarySubtype::Uuid && b.bytes.len() == 16 => Kind::Uuid,
        Bson::Binary(_) => Kind::Binary,
        Bson::Array(_) | Bson::Document(_) => Kind::Json,
        _ => Kind::Mixed,
    })
}

fn merge(a: Kind, b: Kind) -> Kind {
    match (a, b) {
        (Kind::Nothing, x) => x,
        (x, y) if x == y => x,
        (Kind::Int32, Kind::Int64) | (Kind::Int64, Kind::Int32) => Kind::Int64,
        (Kind::Int32 | Kind::Int64 | Kind::Double, Kind::Int32 | Kind::Int64 | Kind::Double) => {
            Kind::Double
        }
        _ => Kind::Mixed,
    }
}

fn meta(name: &str, kind: Kind) -> ColumnMeta {
    let (type_name, dt) = match kind {
        Kind::Bool => ("bool", DataType::Bool),
        Kind::Int32 => ("int", DataType::Int32),
        Kind::Int64 => ("long", DataType::Int64),
        Kind::Double => ("double", DataType::Float64),
        Kind::Decimal => ("decimal", DataType::Numeric),
        Kind::String => ("string", DataType::Text),
        Kind::ObjectId => ("objectId", DataType::Text),
        Kind::Date => ("date", DataType::TimestampTz),
        Kind::Uuid => ("uuid", DataType::Uuid),
        Kind::Binary => ("binData", DataType::Bytes),
        Kind::Json => ("array/object", DataType::Json),
        Kind::Mixed => ("mixed", DataType::Text),
        Kind::Nothing => ("null", DataType::Text),
    };
    ColumnMeta::new(name, type_name, dt)
}

/// Visit every leaf of `doc` with its dotted path.
fn walk<'a>(doc: &'a Document, prefix: &str, depth: usize, f: &mut impl FnMut(String, &'a Bson)) {
    for (k, v) in doc {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            Bson::Document(d) if depth < MAX_DEPTH && !d.is_empty() && !is_extended(d) => {
                walk(d, &path, depth + 1, f);
            }
            _ => f(path, v),
        }
    }
}

/// A sub-document that is really one value (`{ $numberLong: … }` style keys).
fn is_extended(d: &Document) -> bool {
    d.keys().next().is_some_and(|k| k.starts_with('$'))
}

/// The value at a dotted path, following the same flattening as [`walk`].
pub(crate) fn lookup<'a>(doc: &'a Document, path: &str) -> Option<&'a Bson> {
    if let Some(v) = doc.get(path) {
        return Some(v);
    }
    let mut cur = doc;
    let mut rest = path;
    loop {
        let (head, tail) = rest.split_once('.')?;
        cur = cur.get_document(head).ok()?;
        if let Some(v) = cur.get(tail) {
            return Some(v);
        }
        rest = tail;
    }
}

/// Column layout inferred from a sample of documents.
#[derive(Clone, Debug)]
pub struct Layout {
    paths: Vec<String>,
    kinds: Vec<Kind>,
    columns: Arc<[ColumnMeta]>,
}

impl Layout {
    /// Infer columns from `sample` (the first batch).
    pub fn infer(sample: &[Document]) -> Self {
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut paths: Vec<String> = Vec::new();
        let mut kinds: Vec<Kind> = Vec::new();
        for d in sample {
            walk(d, "", 1, &mut |path, v| {
                let ix = match index.get(&path) {
                    Some(ix) => *ix,
                    None if paths.len() < MAX_COLUMNS => {
                        index.insert(path.clone(), paths.len());
                        paths.push(path);
                        kinds.push(Kind::Nothing);
                        paths.len() - 1
                    }
                    None => return,
                };
                if let Some(k) = kind_of(v) {
                    kinds[ix] = merge(kinds[ix], k);
                }
            });
        }
        // `_id` first, the rest in first-seen order.
        if let Some(pos) = paths.iter().position(|p| p == "_id") {
            let p = paths.remove(pos);
            let k = kinds.remove(pos);
            paths.insert(0, p);
            kinds.insert(0, k);
        }
        let mut columns: Vec<ColumnMeta> =
            paths.iter().zip(&kinds).map(|(p, k)| meta(p, *k)).collect();
        columns.push(ColumnMeta::new(
            DOCUMENT_COLUMN,
            crate::batch::DOCUMENT_TYPE,
            DataType::Json,
        ));
        Self {
            paths,
            kinds,
            columns: Arc::from(columns),
        }
    }

    /// The columns, ending with [`DOCUMENT_COLUMN`].
    pub fn columns(&self) -> Arc<[ColumnMeta]> {
        self.columns.clone()
    }

    /// Rows for `docs` in this layout.
    pub fn batch(&self, docs: &[Document]) -> RowBatch {
        let mut b = RowBatchBuilder::for_columns(&self.columns, docs.len());
        let mut buf = String::new();
        for d in docs {
            for (path, kind) in self.paths.iter().zip(&self.kinds) {
                push_cell(&mut b, *kind, lookup(d, path), &mut buf);
            }
            b.push_str(&json_text(&Bson::Document(d.clone())));
        }
        b.finish()
    }
}

/// A value as relaxed Extended JSON text.
pub fn json_text(v: &Bson) -> String {
    v.clone().into_relaxed_extjson().to_string()
}

/// A value as a grid shows it in a text column: strings as-is, ids as hex, dates as
/// RFC 3339, everything else as relaxed Extended JSON.
pub fn display(v: &Bson) -> String {
    match v {
        Bson::String(s) | Bson::Symbol(s) => s.clone(),
        Bson::ObjectId(o) => o.to_hex(),
        Bson::DateTime(d) => d.try_to_rfc3339_string().unwrap_or_else(|_| d.to_string()),
        Bson::Decimal128(d) => d.to_string(),
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => n.to_string(),
        Bson::Boolean(b) => b.to_string(),
        other => json_text(other),
    }
}

fn push_cell(b: &mut RowBatchBuilder, kind: Kind, v: Option<&Bson>, buf: &mut String) {
    let Some(v) = v.filter(|v| !matches!(v, Bson::Null | Bson::Undefined)) else {
        b.push_null();
        return;
    };
    match (kind, v) {
        (Kind::Bool, Bson::Boolean(x)) => b.push_bool(*x),
        (Kind::Int32 | Kind::Int64 | Kind::Double, Bson::Int32(n)) => b.push_i64(i64::from(*n)),
        (Kind::Int64, Bson::Int64(n)) => b.push_i64(*n),
        (Kind::Double, Bson::Int64(n)) => b.push_f64(*n as f64),
        (Kind::Double, Bson::Double(f)) => b.push_f64(*f),
        (Kind::Date, Bson::DateTime(d)) => b.push_i64(d.timestamp_millis().saturating_mul(1000)),
        (Kind::Uuid, Bson::Binary(x)) if x.bytes.len() == 16 => {
            let mut u = [0u8; 16];
            u.copy_from_slice(&x.bytes);
            b.push_uuid(u);
        }
        (Kind::Binary, Bson::Binary(x)) => b.push_bytes(&x.bytes),
        (Kind::Json, Bson::Array(_) | Bson::Document(_)) => b.push_str(&json_text(v)),
        (Kind::Decimal | Kind::String | Kind::ObjectId | Kind::Mixed | Kind::Nothing, v) => {
            buf.clear();
            buf.push_str(&display(v));
            b.push_str(buf);
        }
        // A value that does not fit the column's type (seen only after the first batch):
        // empty here, whole in the document column.
        _ => b.push_null(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::BatchList;
    use crate::value::Value;
    use mongodb::bson::oid::ObjectId;
    use mongodb::bson::{DateTime, doc};

    #[test]
    fn flattens_nested_fields_and_types_columns() {
        let id = ObjectId::parse_str("507f1f77bcf86cd799439011").expect("oid");
        let docs = vec![
            doc! { "name": "Ada", "_id": id, "age": 36, "address": { "city": "London", "geo": { "lat": 51.5 } }, "tags": ["a", "b"] },
            doc! { "_id": 2, "name": "Bob", "age": 41.5, "at": DateTime::from_millis(1_000), "extra": null },
        ];
        let layout = Layout::infer(&docs);
        let cols = layout.columns();
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "_id",
                "name",
                "age",
                "address.city",
                "address.geo.lat",
                "tags",
                "at",
                "extra",
                DOCUMENT_COLUMN
            ]
        );
        let types: Vec<&str> = cols.iter().map(|c| c.type_name.as_str()).collect();
        assert_eq!(
            types,
            [
                "mixed",
                "string",
                "double",
                "string",
                "double",
                "array/object",
                "date",
                "null",
                "document"
            ]
        );
        let mut list = BatchList::default();
        list.push(layout.batch(&docs));
        let v = |r: usize, c: usize| {
            list.cell(r, c)
                .map(|cell| cell.to_value(cols[c].data_type))
                .expect("cell")
        };
        assert_eq!(v(0, 0), Value::Text("507f1f77bcf86cd799439011".into()));
        assert_eq!(v(1, 0), Value::Text("2".into()));
        assert_eq!(v(0, 2), Value::Float(36.0));
        assert_eq!(v(0, 3), Value::Text("London".into()));
        assert_eq!(v(0, 4), Value::Float(51.5));
        assert_eq!(v(0, 5), Value::Json(r#"["a","b"]"#.into()));
        assert_eq!(v(1, 3), Value::Null);
        assert_eq!(v(1, 6), Value::TimestampTz(1_000_000));
        let Value::Json(whole) = v(0, 8) else {
            panic!("document column")
        };
        assert!(
            whole.contains(r#""$oid":"507f1f77bcf86cd799439011""#),
            "{whole}"
        );
    }

    #[test]
    fn later_values_of_another_type_stay_in_the_document() {
        let layout = Layout::infer(&[doc! { "n": 1 }]);
        let batch = layout.batch(&[doc! { "n": "seven", "new": true }]);
        let mut list = BatchList::default();
        list.push(batch);
        let cols = layout.columns();
        assert_eq!(cols.len(), 2);
        assert_eq!(list.cell(0, 0).map(|c| c.is_null()), Some(true));
        let doc = list.cell(0, 1).map(|c| c.to_display()).unwrap_or_default();
        assert!(doc.contains("seven") && doc.contains("new"), "{doc}");
    }
}
