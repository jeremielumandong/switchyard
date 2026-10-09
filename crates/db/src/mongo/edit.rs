//! Grid edits on MongoDB results: staged cell changes, new rows and deletes become
//! `updateOne({ _id }, { $set })`, `insertOne` and `deleteOne({ _id })` statements in
//! the shell subset ([`super::shell`]), one document each.
//!
//! A row is matched by the `_id` read from the result's document column (relaxed
//! Extended JSON), so its BSON type (ObjectId, number, string, …) is kept. New values
//! take the type of their column (`int`, `long`, `objectId`, `date`, …) or, in a `mixed`
//! column, of the value they replace; numbers stay numbers and strings stay strings.

use std::str::FromStr;

use mongodb::bson::oid::ObjectId;
use mongodb::bson::spec::BinarySubtype;
use mongodb::bson::{Binary, Bson, DateTime, Decimal128, Document};

use super::flatten::lookup;
use super::shell::{self, Op, Shape};
use crate::batch::{ColumnMeta, DOCUMENT_COLUMN};
use crate::dialect::Dialect;
use crate::dialect::mongo::MongoDialect;
use crate::edit::EditTable;
use crate::value::Value;

/// The field every document is matched by.
pub const ID: &str = "_id";

/// The collection a `find` result can be edited in: the statement is a plain `find` /
/// `findOne` (projection of included or excluded fields only) and the result has both
/// `_id` and the document column. The error is a hint for the user.
pub fn edit_target(sql: &str, columns: &[ColumnMeta]) -> Result<EditTable, String> {
    const ONLY_FIND: &str = "Only find() results can be edited in place";
    let Ok(Op::Command {
        db,
        command,
        shape: Shape::Cursor,
        ..
    }) = shell::parse(sql)
    else {
        return Err(ONLY_FIND.into());
    };
    let Some(("find", Bson::String(collection))) =
        command.iter().next().map(|(k, v)| (k.as_str(), v))
    else {
        return Err(ONLY_FIND.into());
    };
    if let Ok(p) = command.get_document("projection")
        && p.values().any(|v| {
            !matches!(
                v,
                Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) | Bson::Boolean(_)
            )
        })
    {
        return Err(
            "Results with computed fields (a projection with expressions) are read-only".into(),
        );
    }
    let has = |name: &str| columns.iter().any(|c| c.name == name);
    if !has(ID) || !has(DOCUMENT_COLUMN) {
        return Err(
            "This result has no _id, so documents cannot be matched; include _id to edit them"
                .into(),
        );
    }
    Ok(EditTable {
        schema: db,
        table: collection.clone(),
    })
}

/// The `_id` of a document column cell (relaxed Extended JSON).
pub fn document_id(document: &str) -> Result<Bson, String> {
    let doc = parse_document_json(document)?;
    doc.get(ID)
        .cloned()
        .ok_or_else(|| "This document has no _id".to_owned())
}

fn parse_document_json(text: &str) -> Result<Document, String> {
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("Unreadable document: {e}"))?;
    match Bson::try_from(json) {
        Ok(Bson::Document(d)) => Ok(d),
        Ok(_) => Err("Unreadable document: not an object".into()),
        Err(e) => Err(format!("Unreadable document: {e}")),
    }
}

/// The type name a value would get as a column (as [`super::flatten`] names them).
fn type_of(v: &Bson) -> &'static str {
    match v {
        Bson::Boolean(_) => "bool",
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::Double(_) => "double",
        Bson::Decimal128(_) => "decimal",
        Bson::String(_) | Bson::Symbol(_) => "string",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Binary(b) if b.subtype == BinarySubtype::Uuid && b.bytes.len() == 16 => "uuid",
        Bson::Binary(_) => "binData",
        Bson::Array(_) | Bson::Document(_) => "array/object",
        _ => "mixed",
    }
}

fn int_value(n: i64, long: bool) -> Bson {
    match i32::try_from(n) {
        Ok(small) if !long => Bson::Int32(small),
        _ => Bson::Int64(n),
    }
}

/// Text typed into a cell as a number: integers stay integers (32-bit unless `long`),
/// anything else with a fraction or exponent becomes a double.
fn number(text: &str, long: bool) -> Result<Bson, String> {
    let t = text.trim();
    if let Ok(n) = t.parse::<i64>() {
        return Ok(int_value(n, long));
    }
    t.parse::<f64>()
        .map(Bson::Double)
        .map_err(|_| format!("`{text}` is not a number"))
}

/// Text typed into a cell, as a value of type `kind` (a column type name).
fn from_text(kind: &str, text: &str) -> Result<Bson, String> {
    let t = text.trim();
    Ok(match kind {
        "string" => Bson::String(text.to_owned()),
        "objectId" => Bson::ObjectId(
            ObjectId::parse_str(t)
                .map_err(|_| format!("`{text}` is not an ObjectId (24 hex digits)"))?,
        ),
        "int" => number(t, false)?,
        "long" => number(t, true)?,
        "double" => Bson::Double(
            t.parse::<f64>()
                .map_err(|_| format!("`{text}` is not a number"))?,
        ),
        "decimal" => Bson::Decimal128(
            Decimal128::from_str(t).map_err(|_| format!("`{text}` is not a decimal"))?,
        ),
        "bool" => match t.to_ascii_lowercase().as_str() {
            "true" | "1" => Bson::Boolean(true),
            "false" | "0" => Bson::Boolean(false),
            _ => return Err(format!("`{text}` is not true or false")),
        },
        "date" => Bson::DateTime(
            shell::parse_date(t).ok_or_else(|| format!("`{text}` is not an ISO-8601 date"))?,
        ),
        "uuid" => {
            shell::parse_value(&format!("UUID({})", json_string(t))).map_err(|e| e.message)?
        }
        "binData" => return Err("Binary fields are not edited in place".into()),
        "array/object" => match shell::parse_value(t) {
            Ok(v @ (Bson::Array(_) | Bson::Document(_))) => v,
            Ok(_) => return Err("Expected an array `[…]` or a document `{…}`".into()),
            Err(e) => return Err(e.message),
        },
        // Mixed or unknown: read it as a shell value (`42`, `"42"`, `ObjectId("…")`,
        // `true`), else keep the text as a string.
        _ => shell::parse_value(t).unwrap_or_else(|_| Bson::String(text.to_owned())),
    })
}

/// The BSON value for `v` entered in a column typed `type_name` (from [`ColumnMeta`]),
/// replacing `original` (the field's current value, if any).
pub fn field_value(type_name: &str, original: Option<&Bson>, v: &Value) -> Result<Bson, String> {
    let original = original.filter(|b| !matches!(b, Bson::Null | Bson::Undefined));
    // A column whose values disagree on a type follows the value being replaced.
    let kind = match type_name {
        "mixed" | "null" | "" => original.map(type_of).unwrap_or("mixed"),
        t => t,
    };
    Ok(match v {
        Value::Null => Bson::Null,
        Value::Text(s) | Value::Other(s) => from_text(kind, s)?,
        Value::Bool(b) => Bson::Boolean(*b),
        Value::Int(n) => match kind {
            "double" => Bson::Double(*n as f64),
            k => int_value(*n, k == "long"),
        },
        Value::Float(f) => Bson::Double(*f),
        Value::Numeric(s) => from_text("decimal", s)?,
        Value::Json(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(j) => Bson::try_from(j).map_err(|e| e.to_string())?,
            Err(_) => from_text("array/object", s)?,
        },
        Value::Uuid(u) => Bson::Binary(Binary {
            subtype: BinarySubtype::Uuid,
            bytes: u.to_vec(),
        }),
        Value::Bytes(b) => Bson::Binary(Binary {
            subtype: BinarySubtype::Generic,
            bytes: b.clone(),
        }),
        Value::Timestamp(us) | Value::TimestampTz(us) => {
            Bson::DateTime(DateTime::from_millis(us.div_euclid(1000)))
        }
        Value::Date(days) => Bson::DateTime(DateTime::from_millis(i64::from(*days) * 86_400_000)),
        Value::Time(_) => Bson::String(v.to_display()),
    })
}

fn json_string(s: &str) -> String {
    serde_json::Value::String(s.to_owned()).to_string()
}

/// A value as shell text that [`shell::parse_value`] reads back as the same BSON value.
pub fn shell_text(v: &Bson) -> Result<String, String> {
    let mut out = String::new();
    write_shell(&mut out, v)?;
    Ok(out)
}

fn write_shell(out: &mut String, v: &Bson) -> Result<(), String> {
    use std::fmt::Write as _;
    match v {
        Bson::Null | Bson::Undefined => out.push_str("null"),
        Bson::Boolean(b) => {
            let _ = write!(out, "{b}");
        }
        Bson::Int32(n) => {
            let _ = write!(out, "{n}");
        }
        Bson::Int64(n) => {
            let _ = write!(out, "NumberLong({})", json_string(&n.to_string()));
        }
        Bson::Double(f) if f.is_nan() => out.push_str("NaN"),
        Bson::Double(f) if f.is_infinite() => {
            out.push_str(if *f > 0.0 { "Infinity" } else { "-Infinity" });
        }
        // `{:?}` keeps a fraction (`36.0`), so it reads back as a double.
        Bson::Double(f) => {
            let _ = write!(out, "{f:?}");
        }
        Bson::Decimal128(d) => {
            let _ = write!(out, "NumberDecimal({})", json_string(&d.to_string()));
        }
        Bson::String(s) | Bson::Symbol(s) => out.push_str(&json_string(s)),
        Bson::ObjectId(o) => {
            let _ = write!(out, "ObjectId(\"{}\")", o.to_hex());
        }
        Bson::DateTime(d) => match d.try_to_rfc3339_string() {
            Ok(s) => {
                let _ = write!(out, "ISODate({})", json_string(&s));
            }
            Err(_) => {
                let _ = write!(out, "ISODate({})", d.timestamp_millis());
            }
        },
        Bson::Binary(b) if b.subtype == BinarySubtype::Uuid && b.bytes.len() == 16 => {
            let mut u = [0u8; 16];
            u.copy_from_slice(&b.bytes);
            let _ = write!(
                out,
                "UUID(\"{}\")",
                Value::Uuid(u).to_display().to_ascii_lowercase()
            );
        }
        Bson::Timestamp(t) => {
            let _ = write!(out, "Timestamp({}, {})", t.time, t.increment);
        }
        Bson::RegularExpression(r) => {
            let _ = write!(
                out,
                "RegExp({}, {})",
                json_string(&r.pattern),
                json_string(&r.options)
            );
        }
        Bson::MinKey => out.push_str("MinKey"),
        Bson::MaxKey => out.push_str("MaxKey"),
        Bson::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_shell(out, item)?;
            }
            out.push(']');
        }
        Bson::Document(d) => write_document(out, d)?,
        other => {
            return Err(format!(
                "{} values cannot be written from the grid",
                match other {
                    Bson::Binary(_) => "Binary",
                    Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => "JavaScript",
                    _ => "These",
                }
            ));
        }
    }
    Ok(())
}

fn write_document(out: &mut String, d: &Document) -> Result<(), String> {
    if d.is_empty() {
        out.push_str("{}");
        return Ok(());
    }
    out.push_str("{ ");
    for (i, (k, v)) in d.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&json_string(k));
        out.push_str(": ");
        write_shell(out, v)?;
    }
    out.push_str(" }");
    Ok(())
}

fn target(table: &EditTable) -> String {
    MongoDialect.qualified(table.schema.as_deref().unwrap_or(""), &table.table)
}

fn id_filter(id: &Bson) -> Result<String, String> {
    Ok(format!("{{ _id: {} }}", shell_text(id)?))
}

/// `db.getCollection("c").updateOne({ _id: … }, { $set: { "a.b": … } })`.
pub fn update_statement(
    table: &EditTable,
    id: &Bson,
    set: &[(String, Bson)],
) -> Result<String, String> {
    let mut fields = Document::new();
    for (path, v) in set {
        fields.insert(path.clone(), v.clone());
    }
    let mut body = String::new();
    write_document(&mut body, &fields)?;
    Ok(format!(
        "{}.updateOne({}, {{ $set: {body} }})",
        target(table),
        id_filter(id)?
    ))
}

/// `db.getCollection("c").deleteOne({ _id: … })`.
pub fn delete_statement(table: &EditTable, id: &Bson) -> Result<String, String> {
    Ok(format!("{}.deleteOne({})", target(table), id_filter(id)?))
}

/// Put `v` at the dotted `path` of `doc`, creating sub-documents on the way.
fn insert_path(doc: &mut Document, path: &str, v: Bson) {
    match path.split_once('.') {
        None => {
            doc.insert(path, v);
        }
        Some((head, rest)) => {
            if !matches!(doc.get(head), Some(Bson::Document(_))) {
                doc.insert(head, Document::new());
            }
            if let Some(Bson::Document(sub)) = doc.get_mut(head) {
                insert_path(sub, rest, v);
            }
        }
    }
}

/// `db.getCollection("c").insertOne({ … })` with dotted paths made into sub-documents.
pub fn insert_statement(table: &EditTable, fields: &[(String, Bson)]) -> Result<String, String> {
    let mut doc = Document::new();
    for (path, v) in fields {
        insert_path(&mut doc, path, v.clone());
    }
    let mut body = String::new();
    write_document(&mut body, &doc)?;
    Ok(format!("{}.insertOne({body})", target(table)))
}

fn column<'a>(columns: &'a [ColumnMeta], name: &str) -> Result<&'a ColumnMeta, String> {
    columns
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| format!("No column `{name}`"))
}

/// The update for one edited row: `document` is its document column text, `set` the
/// changed (column, value) pairs.
pub fn row_update(
    table: &EditTable,
    columns: &[ColumnMeta],
    document: &str,
    set: &[(String, Value)],
) -> Result<String, String> {
    let doc = parse_document_json(document)?;
    let id = doc
        .get(ID)
        .ok_or_else(|| "This document has no _id".to_owned())?;
    let mut fields = Vec::new();
    for (name, v) in set {
        if name == ID || name == DOCUMENT_COLUMN {
            return Err(format!("{name} is not edited in place"));
        }
        let meta = column(columns, name)?;
        let value = field_value(&meta.type_name, lookup(&doc, name), v)
            .map_err(|e| format!("{name}: {e}"))?;
        fields.push((name.clone(), value));
    }
    update_statement(table, id, &fields)
}

/// The delete for one row, by the `_id` in its document column text.
pub fn row_delete(table: &EditTable, document: &str) -> Result<String, String> {
    delete_statement(table, &document_id(document)?)
}

/// The insert for one new row: NULL values and the document column are left out, so
/// the server adds `_id` unless the row sets one.
pub fn row_insert(
    table: &EditTable,
    columns: &[ColumnMeta],
    values: &[(String, Value)],
) -> Result<String, String> {
    let mut fields = Vec::new();
    for (name, v) in values {
        if name == DOCUMENT_COLUMN || v.is_null() {
            continue;
        }
        let meta = column(columns, name)?;
        let value = field_value(&meta.type_name, None, v).map_err(|e| format!("{name}: {e}"))?;
        fields.push((name.clone(), value));
    }
    insert_statement(table, &fields)
}

/// Collections that `deleteOne` / `deleteMany` statements among `statements` delete
/// from (Production confirms these before a commit).
pub fn delete_targets(statements: &[String]) -> Vec<String> {
    statements
        .iter()
        .filter_map(|s| match shell::parse(s).ok()? {
            Op::Command { db, command, .. } => {
                let (verb, target) = command.iter().next()?;
                let name = target.as_str()?;
                (verb == "delete").then(|| match db {
                    Some(db) => format!("{db}.{name}"),
                    None => name.to_owned(),
                })
            }
            Op::Use(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::DataType;
    use mongodb::bson::doc;

    fn table() -> EditTable {
        EditTable {
            schema: None,
            table: "people".into(),
        }
    }

    fn cols() -> Vec<ColumnMeta> {
        vec![
            ColumnMeta::new("_id", "objectId", DataType::Text),
            ColumnMeta::new("name", "string", DataType::Text),
            ColumnMeta::new("age", "int", DataType::Int32),
            ColumnMeta::new("score", "double", DataType::Float64),
            ColumnMeta::new("visits", "long", DataType::Int64),
            ColumnMeta::new("zip", "mixed", DataType::Text),
            ColumnMeta::new("address.city", "string", DataType::Text),
            ColumnMeta::new("at", "date", DataType::TimestampTz),
            ColumnMeta::new("tags", "array/object", DataType::Json),
            ColumnMeta::new(DOCUMENT_COLUMN, "document", DataType::Json),
        ]
    }

    const DOC: &str = r#"{"_id":{"$oid":"507f1f77bcf86cd799439011"},"name":"Ada","age":36,"zip":"02139","address":{"city":"London"}}"#;

    /// Parse a generated statement back and return its command document.
    fn command(sql: &str) -> Document {
        match shell::parse(sql) {
            Ok(Op::Command { command, .. }) => command,
            other => panic!("{sql}: {other:?}"),
        }
    }

    #[test]
    fn update_matches_the_object_id_and_sets_nested_paths() {
        let text = |s: &str| Value::Text(s.into());
        let sql = row_update(
            &table(),
            &cols(),
            DOC,
            &[
                ("address.city".into(), text("Paris")),
                ("age".into(), text("37")),
                ("zip".into(), text("10001")),
            ],
        )
        .unwrap();
        assert_eq!(
            sql,
            r#"db.getCollection("people").updateOne({ _id: ObjectId("507f1f77bcf86cd799439011") }, { $set: { "address.city": "Paris", "age": 37, "zip": "10001" } })"#
        );
        let c = command(&sql);
        let u = c.get_array("updates").unwrap()[0].as_document().unwrap();
        let oid = ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap();
        assert_eq!(u.get_document("q").unwrap(), &doc! { "_id": oid });
        // Numbers stay numbers, and a string column of digits stays a string.
        assert_eq!(
            u.get_document("u").unwrap(),
            &doc! { "$set": { "address.city": "Paris", "age": 37, "zip": "10001" } }
        );
        assert!(!u.get_bool("multi").unwrap_or(false));
    }

    #[test]
    fn values_keep_their_column_types() {
        let t = |s: &str| Value::Text(s.into());
        assert_eq!(field_value("int", None, &t("5")), Ok(Bson::Int32(5)));
        assert_eq!(
            field_value("int", None, &t("5000000000")),
            Ok(Bson::Int64(5_000_000_000))
        );
        assert_eq!(field_value("long", None, &t("5")), Ok(Bson::Int64(5)));
        assert_eq!(field_value("double", None, &t("5")), Ok(Bson::Double(5.0)));
        assert_eq!(field_value("int", None, &t("2.5")), Ok(Bson::Double(2.5)));
        assert!(field_value("int", None, &t("five")).is_err());
        assert_eq!(
            field_value("string", None, &t("42")),
            Ok(Bson::String("42".into()))
        );
        assert_eq!(
            field_value("bool", None, &t("TRUE")),
            Ok(Bson::Boolean(true))
        );
        assert_eq!(
            field_value("objectId", None, &t(" 507f1f77bcf86cd799439011 ")),
            Ok(Bson::ObjectId(
                ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap()
            ))
        );
        assert!(field_value("objectId", None, &t("nope")).is_err());
        assert_eq!(
            field_value("date", None, &t("2024-05-01T10:00:00Z")),
            Ok(Bson::DateTime(DateTime::from_millis(1_714_557_600_000)))
        );
        assert_eq!(
            field_value("array/object", None, &t("[1, 'a']")),
            Ok(Bson::Array(vec![Bson::Int32(1), Bson::String("a".into())]))
        );
        assert!(field_value("array/object", None, &t("7")).is_err());
        assert_eq!(field_value("string", None, &Value::Null), Ok(Bson::Null));
        // Mixed columns follow the value they replace, else read a shell value.
        let s = Bson::String("x".into());
        assert_eq!(
            field_value("mixed", Some(&s), &t("7")),
            Ok(Bson::String("7".into()))
        );
        assert_eq!(
            field_value("mixed", Some(&Bson::Int64(1)), &t("7")),
            Ok(Bson::Int64(7))
        );
        assert_eq!(field_value("mixed", None, &t("7")), Ok(Bson::Int32(7)));
        assert_eq!(
            field_value("mixed", None, &t("hello")),
            Ok(Bson::String("hello".into()))
        );
        // Typed values (a duplicated row) map directly.
        assert_eq!(
            field_value("long", None, &Value::Int(3)),
            Ok(Bson::Int64(3))
        );
        assert_eq!(
            field_value("date", None, &Value::TimestampTz(1_000_000)),
            Ok(Bson::DateTime(DateTime::from_millis(1_000)))
        );
    }

    #[test]
    fn shell_text_round_trips() {
        let values = [
            Bson::Int32(-3),
            Bson::Int64(1 << 40),
            Bson::Int64(7),
            Bson::Double(36.0),
            Bson::Double(0.1),
            Bson::Double(1e300),
            Bson::String("quote \" and \\ and\nnewline ü".into()),
            Bson::ObjectId(ObjectId::parse_str("507f1f77bcf86cd799439011").unwrap()),
            Bson::DateTime(DateTime::from_millis(1_714_557_600_123)),
            Bson::Decimal128(Decimal128::from_str("12.50").unwrap()),
            Bson::Binary(Binary {
                subtype: BinarySubtype::Uuid,
                bytes: (0u8..16).collect(),
            }),
            Bson::Boolean(false),
            Bson::Null,
            Bson::Array(vec![Bson::Int32(1), Bson::Document(doc! { "a b": "c" })]),
            Bson::Document(doc! { "nested": { "x": 1_i64 } }),
        ];
        for v in values {
            let text = shell_text(&v).unwrap();
            assert_eq!(shell::parse_value(&text).as_ref(), Ok(&v), "{text}");
        }
        assert!(
            shell_text(&Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes: vec![1]
            }))
            .is_err()
        );
    }

    #[test]
    fn ids_of_other_types_are_kept() {
        let t = EditTable {
            schema: Some("shop".into()),
            table: "orders".into(),
        };
        assert_eq!(
            row_delete(&t, r#"{"_id":42,"x":1}"#).unwrap(),
            r#"db.getSiblingDB("shop").getCollection("orders").deleteOne({ _id: 42 })"#
        );
        assert_eq!(
            row_delete(&t, r#"{"_id":"abc"}"#).unwrap(),
            r#"db.getSiblingDB("shop").getCollection("orders").deleteOne({ _id: "abc" })"#
        );
        assert!(row_delete(&t, r#"{"x":1}"#).is_err());
        let sql = row_delete(&t, r#"{"_id":{"$oid":"507f1f77bcf86cd799439011"}}"#).unwrap();
        assert_eq!(delete_targets(&[sql]), ["shop.orders"]);
        let upd = row_update(
            &t,
            &cols(),
            r#"{"_id":1}"#,
            &[("name".into(), Value::Text("x".into()))],
        )
        .unwrap();
        assert!(delete_targets(&[upd]).is_empty());
    }

    #[test]
    fn inserts_nest_dotted_paths_and_skip_nulls() {
        let sql = row_insert(
            &table(),
            &cols(),
            &[
                ("name".into(), Value::Text("Eve".into())),
                ("address.city".into(), Value::Text("Rome".into())),
                ("age".into(), Value::Text("29".into())),
                ("score".into(), Value::Null),
                (DOCUMENT_COLUMN.into(), Value::Json("{}".into())),
            ],
        )
        .unwrap();
        assert_eq!(
            sql,
            r#"db.getCollection("people").insertOne({ "name": "Eve", "address": { "city": "Rome" }, "age": 29 })"#
        );
        let c = command(&sql);
        let d = c.get_array("documents").unwrap()[0].as_document().unwrap();
        assert_eq!(d.get_document("address").unwrap(), &doc! { "city": "Rome" });
        assert_eq!(d.get("age"), Some(&Bson::Int32(29)));
    }

    #[test]
    fn only_find_results_with_an_id_are_editable() {
        let c = cols();
        assert_eq!(
            edit_target(r#"db.people.find({ age: { $gt: 3 } }).limit(50)"#, &c),
            Ok(table())
        );
        assert_eq!(
            edit_target(
                r#"db.getSiblingDB("shop").getCollection("o").findOne()"#,
                &c
            ),
            Ok(EditTable {
                schema: Some("shop".into()),
                table: "o".into()
            })
        );
        assert!(edit_target("db.people.aggregate([])", &c).is_err());
        assert!(edit_target("db.people.find().count()", &c).is_err());
        assert!(edit_target("db.people.find({}, { n: '$name' })", &c).is_err());
        assert!(edit_target("db.people.find({}, { name: 1 })", &c).is_ok());
        let no_id: Vec<ColumnMeta> = c.into_iter().skip(1).collect();
        let err = edit_target("db.people.find()", &no_id).unwrap_err();
        assert!(err.contains("_id"), "{err}");
    }
}
