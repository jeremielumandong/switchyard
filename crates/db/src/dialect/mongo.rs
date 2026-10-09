//! MongoDB "dialect": mongosh statements instead of SQL. Templates and data-view pages
//! are `find` / `insertOne` / `updateOne` calls; statement safety comes from the
//! shell parser rather than `sqlparser`.

use super::lexer::Flavor;
use super::{Dialect, ParamRef, SortKey, StatementSpan, line_of_byte};
use crate::catalog::ObjectKind;
use crate::guard::{Classification, Destructive as GuardDestructive, DestructiveKind};
use crate::mongo::shell::{self, Destructive, Effect};
use crate::value::{self, Engine, Value};

/// MongoDB shell dialect.
#[derive(Clone, Copy, Debug, Default)]
pub struct MongoDialect;

/// Shell methods offered by completion.
const MONGO_METHODS: &[&str] = &[
    "aggregate",
    "countDocuments",
    "createIndex",
    "deleteMany",
    "deleteOne",
    "distinct",
    "drop",
    "dropIndex",
    "estimatedDocumentCount",
    "explain",
    "find",
    "findOne",
    "getCollection",
    "getIndexes",
    "getSiblingDB",
    "insertMany",
    "insertOne",
    "limit",
    "projection",
    "replaceOne",
    "runCommand",
    "skip",
    "sort",
    "stats",
    "updateMany",
    "updateOne",
    "$match",
    "$group",
    "$project",
    "$sort",
    "$limit",
    "$lookup",
    "$unwind",
    "$count",
    "$set",
    "$in",
    "$gt",
    "$gte",
    "$lt",
    "$lte",
    "$ne",
    "$exists",
    "$regex",
    "$and",
    "$or",
];

/// A JSON string literal (`"name"`).
fn json_string(s: &str) -> String {
    serde_json::Value::String(s.to_owned()).to_string()
}

/// A field name as an object key: bare when it is a plain identifier, else quoted.
fn key(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if plain {
        name.to_owned()
    } else {
        json_string(name)
    }
}

fn projection(cols: &[String]) -> Option<String> {
    (!cols.is_empty()).then(|| {
        let mut fields: Vec<String> = cols.iter().map(|c| format!("{}: 1", key(c))).collect();
        if !cols.iter().any(|c| c == "_id") {
            fields.push("_id: 0".into());
        }
        format!("{{ {} }}", fields.join(", "))
    })
}

impl Dialect for MongoDialect {
    /// `explain` of a find or aggregate: `queryPlanner`, or `executionStats` (runs it).
    fn plans(&self) -> super::PlanSupport {
        super::PlanSupport::BOTH
    }

    fn engine(&self) -> Engine {
        Engine::MongoDb
    }

    fn flavor(&self) -> Flavor {
        Flavor::JavaScript
    }

    fn quote_ident(&self, ident: &str) -> String {
        json_string(ident)
    }

    fn qualified(&self, schema: &str, name: &str) -> String {
        if schema.is_empty() {
            format!("db.getCollection({})", json_string(name))
        } else {
            format!(
                "db.getSiblingDB({}).getCollection({})",
                json_string(schema),
                json_string(name)
            )
        }
    }

    fn split_script(&self, sql: &str) -> Vec<StatementSpan> {
        shell::split(sql)
            .into_iter()
            .map(|(start, end)| StatementSpan {
                start,
                end,
                line: line_of_byte(sql, start),
                repeat: 1,
            })
            .collect()
    }

    fn select_rows(&self, qualified: &str, limit: u64) -> String {
        format!("{qualified}.find({{}}).limit({limit})")
    }

    fn select_template(&self, qualified: &str, cols: &[String], limit: u64) -> String {
        match projection(cols) {
            Some(p) => format!("{qualified}.find({{}}, {p}).limit({limit})"),
            None => self.select_rows(qualified, limit),
        }
    }

    fn insert_template(&self, qualified: &str, cols: &[String]) -> String {
        let fields: Vec<String> = cols
            .iter()
            .filter(|c| *c != "_id")
            .map(|c| format!("  {}: null", key(c)))
            .collect();
        let body = if fields.is_empty() {
            "  field: null".to_owned()
        } else {
            fields.join(",\n")
        };
        format!("{qualified}.insertOne({{\n{body}\n}})")
    }

    fn update_template(&self, qualified: &str, cols: &[String], _pk: &[String]) -> String {
        let fields: Vec<String> = cols
            .iter()
            .filter(|c| *c != "_id")
            .map(|c| format!("    {}: null", key(c)))
            .collect();
        let body = if fields.is_empty() {
            "    field: null".to_owned()
        } else {
            fields.join(",\n")
        };
        format!("{qualified}.updateOne(\n  {{ _id: null }},\n  {{ $set: {{\n{body}\n  }} }}\n)")
    }

    fn delete_template(&self, qualified: &str, _pk: &[String]) -> String {
        format!("{qualified}.deleteOne({{ _id: null }})")
    }

    fn script_drop(&self, _kind: ObjectKind, qualified: &str) -> String {
        format!("{qualified}.drop()")
    }

    fn script_create(&self, _kind: ObjectKind, ddl: &str) -> String {
        ddl.trim_end().to_owned()
    }

    fn script_exec(&self, _kind: ObjectKind, qualified: &str, _params: &[String]) -> String {
        format!("// MongoDB has no stored routines\n{qualified}.find({{}})")
    }

    fn find_params(&self, _sql: &str) -> Vec<ParamRef> {
        Vec::new()
    }

    fn bind_params(&self, sql: &str) -> (String, Vec<String>) {
        (sql.to_owned(), Vec::new())
    }

    fn parser_dialect(&self) -> Box<dyn sqlparser::dialect::Dialect> {
        // Unused for statements ([`Self::classify`] and [`Self::check_filter`] answer
        // instead); the generic dialect makes any SQL-only caller fail cleanly.
        Box::new(sqlparser::dialect::GenericDialect {})
    }

    fn literal(&self, v: &Value) -> String {
        match v {
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => {
                let mut s = String::new();
                value::write_float(&mut s, *f);
                s
            }
            Value::Numeric(s) => format!("NumberDecimal({})", json_string(s)),
            Value::Json(s) => s.clone(),
            Value::Uuid(_) => format!("UUID({})", json_string(&v.to_display())),
            Value::Date(_) | Value::Timestamp(_) | Value::TimestampTz(_) => {
                format!("ISODate({})", json_string(&v.to_display()))
            }
            Value::Bytes(_) | Value::Time(_) | Value::Text(_) | Value::Other(_) => {
                json_string(&v.to_display())
            }
        }
    }

    fn keywords(&self) -> &'static [&'static str] {
        &[]
    }

    fn functions(&self) -> &'static [&'static str] {
        MONGO_METHODS
    }

    fn default_schema(&self) -> &'static str {
        ""
    }

    fn object_folders(&self) -> &'static [ObjectKind] {
        &[ObjectKind::Table, ObjectKind::View]
    }

    fn folder_label(&self, kind: ObjectKind) -> &'static str {
        match kind {
            ObjectKind::Table => "Collections",
            other => other.folder_label(),
        }
    }

    fn supports_dependencies(&self) -> bool {
        false
    }

    fn use_database(&self, database: &str) -> Option<String> {
        Some(format!("use {database}"))
    }

    fn select_page(
        &self,
        qualified: &str,
        cols: &[String],
        where_: Option<&str>,
        order: &[SortKey],
        limit: u64,
        offset: u64,
    ) -> String {
        let filter = where_
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .unwrap_or("{}");
        let mut out = match projection(cols) {
            Some(p) => format!("{qualified}.find({filter}, {p})"),
            None => format!("{qualified}.find({filter})"),
        };
        if !order.is_empty() {
            let keys: Vec<String> = order
                .iter()
                .map(|k| format!("{}: {}", key(&k.column), if k.descending { -1 } else { 1 }))
                .collect();
            out.push_str(&format!(".sort({{ {} }})", keys.join(", ")));
        }
        if offset > 0 {
            out.push_str(&format!(".skip({offset})"));
        }
        out.push_str(&format!(".limit({limit})"));
        out
    }

    fn insert_row(&self, qualified: &str, values: &[(String, Option<String>)]) -> String {
        let fields: Vec<String> = values
            .iter()
            .filter_map(|(c, v)| v.as_ref().map(|v| format!("{}: {v}", key(c))))
            .collect();
        format!("{qualified}.insertOne({{ {} }})", fields.join(", "))
    }

    fn classify(&self, sql: &str) -> Option<Classification> {
        let mut found = Vec::new();
        let mut write = false;
        for (s, e) in shell::split(sql) {
            match shell::parse(&sql[s..e]) {
                Err(err) => return Some(Classification::Unparsed(err.message)),
                Ok(op) => match op.effect() {
                    Effect::Read => {}
                    Effect::Write { destructive } => {
                        write = true;
                        if let Some((kind, object)) = destructive {
                            found.push(GuardDestructive {
                                kind: match kind {
                                    Destructive::DropCollection => DestructiveKind::Drop {
                                        object_type: "COLLECTION".into(),
                                    },
                                    Destructive::DropDatabase => DestructiveKind::Drop {
                                        object_type: "DATABASE".into(),
                                    },
                                    Destructive::DeleteAll => DestructiveKind::DeleteWithoutWhere,
                                    Destructive::UpdateAll => DestructiveKind::UpdateWithoutWhere,
                                },
                                objects: vec![if object.is_empty() {
                                    "current database".into()
                                } else {
                                    object
                                }],
                            });
                        }
                    }
                },
            }
        }
        Some(if write {
            Classification::Write(found)
        } else {
            Classification::ReadOnly
        })
    }

    fn check_filter(&self, cond: &str) -> Option<Result<(), String>> {
        Some(shell::parse_document(cond).map(|_| ()).map_err(|e| {
            format!(
                "{} (a filter document such as {{ status: \"A\" }})",
                e.message
            )
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates() {
        let d = MongoDialect;
        let q = d.qualified("shop", "orders");
        assert_eq!(q, r#"db.getSiblingDB("shop").getCollection("orders")"#);
        assert_eq!(
            d.select_template(&q, &["total".into(), "a b".into()], 100),
            r#"db.getSiblingDB("shop").getCollection("orders").find({}, { total: 1, "a b": 1, _id: 0 }).limit(100)"#
        );
        assert_eq!(
            d.select_page(
                &q,
                &[],
                Some("{ status: 'A' }"),
                &[SortKey {
                    column: "total".into(),
                    descending: true
                }],
                50,
                100
            ),
            r#"db.getSiblingDB("shop").getCollection("orders").find({ status: 'A' }).sort({ total: -1 }).skip(100).limit(50)"#
        );
        for sql in [
            d.select_rows(&q, 10),
            d.insert_template(&q, &["_id".into(), "name".into()]),
            d.update_template(&q, &["name".into()], &[]),
            d.delete_template(&q, &[]),
            d.script_drop(ObjectKind::Table, &q),
            d.insert_row(&q, &[("n".into(), Some("1".into())), ("m".into(), None)]),
        ] {
            assert!(shell::parse(&sql).is_ok(), "{sql}");
        }
        assert_eq!(d.literal(&Value::Text("a\"b".into())), r#""a\"b""#);
    }

    #[test]
    fn classifies_with_the_shell_parser() {
        let d = MongoDialect;
        assert_eq!(
            d.classify("db.a.find({})\ndb.b.countDocuments()"),
            Some(Classification::ReadOnly)
        );
        let c = d
            .classify("db.a.find({})\ndb.b.deleteMany({})")
            .expect("classified");
        assert_eq!(c.destructive().len(), 1);
        assert_eq!(c.destructive()[0].headline(), "Delete every row in b?");
        assert!(matches!(
            d.classify("db.a.nope()"),
            Some(Classification::Unparsed(_))
        ));
        assert_eq!(d.check_filter("{ a: 1 }"), Some(Ok(())));
        assert!(matches!(d.check_filter("a = 1"), Some(Err(_))));
    }

    #[test]
    fn splits_into_spans_with_lines() {
        let spans = MongoDialect.split_script("show dbs\n\ndb.a.find()");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].line, 3);
    }
}
