//! Inline editing: which results are editable, and the UPDATE, INSERT and DELETE statements
//! that apply staged edits. Every statement changes exactly one row; updates and deletes
//! pick it through the primary key. Also the table data view's helpers (DBX-3a/3c): WHERE
//! validation, default page order and key filters.

use sqlparser::ast::Statement;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Token;

use crate::batch::ColumnMeta;
use crate::catalog::{ColumnInfo, ObjectDetail};
use crate::complete::table_refs;
use crate::dialect::{Dialect, SortKey};
use crate::value::Value;

/// The table behind an editable result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditTable {
    /// Schema, when the query qualified it.
    pub schema: Option<String>,
    /// Table name.
    pub table: String,
}

/// Whether a result can be edited in place: the statement reads a single table and every
/// column comes from that same table. Primary-key presence is checked separately once the
/// catalog detail is known.
pub fn editable_table(
    dialect: &dyn Dialect,
    sql: &str,
    columns: &[ColumnMeta],
) -> Option<EditTable> {
    let first = sql.split_whitespace().next()?.to_ascii_uppercase();
    if first != "SELECT" && first != "TABLE" {
        return None;
    }
    let refs = table_refs(dialect, sql);
    if refs.len() != 1 {
        return None;
    }
    let upper = sql.to_ascii_uppercase();
    if [" GROUP BY ", " DISTINCT ", " UNION ", " JOIN "]
        .iter()
        .any(|k| upper.contains(k))
    {
        return None;
    }
    let table_id = columns.first()?.table_id?;
    if columns
        .iter()
        .any(|c| c.table_id != Some(table_id) || c.table_column.is_none())
    {
        return None;
    }
    let r = &refs[0];
    Some(EditTable {
        schema: r.schema.clone(),
        table: r.name.clone(),
    })
}

/// Edits for one row: key column values and the new values.
#[derive(Clone, Debug, PartialEq)]
pub struct RowEdit {
    /// Primary key (column, current value).
    pub key: Vec<(String, Value)>,
    /// Changed (column, new value).
    pub set: Vec<(String, Value)>,
}

/// A new row: the values the user set. Columns not listed take their default.
#[derive(Clone, Debug, PartialEq)]
pub struct RowInsert {
    /// (column, value) the user entered or a duplicate copied.
    pub values: Vec<(String, Value)>,
}

/// A row to delete, picked by its primary key.
#[derive(Clone, Debug, PartialEq)]
pub struct RowDelete {
    /// Primary key (column, current value).
    pub key: Vec<(String, Value)>,
}

fn target(dialect: &dyn Dialect, table: &EditTable) -> String {
    match &table.schema {
        Some(s) => dialect.qualified(s, &table.table),
        None => dialect.quote_ident(&table.table),
    }
}

/// `"a" = 1 AND "b" IS NULL`: the condition matching `key`, values as dialect literals.
/// Used for primary keys and for foreign-key navigation (DBX-3c).
pub fn key_condition(dialect: &dyn Dialect, key: &[(String, Value)]) -> String {
    key.iter()
        .map(|(c, v)| match v {
            Value::Null => format!("{} IS NULL", dialect.quote_ident(c)),
            v => format!("{} = {}", dialect.quote_ident(c), dialect.literal(v)),
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// UPDATE statements, one per row.
pub fn update_statements(
    dialect: &dyn Dialect,
    table: &EditTable,
    rows: &[RowEdit],
) -> Vec<String> {
    let target = target(dialect, table);
    rows.iter()
        .filter(|r| !r.set.is_empty() && !r.key.is_empty())
        .map(|r| {
            let set: Vec<String> = r
                .set
                .iter()
                .map(|(c, v)| format!("{} = {}", dialect.quote_ident(c), dialect.literal(v)))
                .collect();
            format!(
                "UPDATE {target} SET {} WHERE {};",
                set.join(", "),
                key_condition(dialect, &r.key)
            )
        })
        .collect()
}

/// INSERT statements, one per new row. `columns` are the result's columns in order:
/// a column the row sets gets its literal; the others are `DEFAULT` (omitted where the
/// dialect has no `DEFAULT` in `VALUES`), except `generated` ones (identity, serial),
/// which are always left out unless set.
pub fn insert_statements(
    dialect: &dyn Dialect,
    table: &EditTable,
    columns: &[String],
    generated: &[String],
    rows: &[RowInsert],
) -> Vec<String> {
    let target = target(dialect, table);
    rows.iter()
        .map(|r| {
            let mut values: Vec<(String, Option<String>)> = Vec::new();
            for c in columns {
                match r.values.iter().find(|(n, _)| n == c) {
                    Some((_, v)) => values.push((c.clone(), Some(dialect.literal(v)))),
                    None if generated.contains(c) => {}
                    None => values.push((c.clone(), None)),
                }
            }
            // Values for columns outside the result (should not happen) still go in.
            for (n, v) in &r.values {
                if !columns.contains(n) {
                    values.push((n.clone(), Some(dialect.literal(v))));
                }
            }
            dialect.insert_row(&target, &values)
        })
        .collect()
}

/// DELETE statements, one per row, each by primary key.
pub fn delete_statements(
    dialect: &dyn Dialect,
    table: &EditTable,
    rows: &[RowDelete],
) -> Vec<String> {
    let target = target(dialect, table);
    rows.iter()
        .filter(|r| !r.key.is_empty())
        .map(|r| {
            format!(
                "DELETE FROM {target} WHERE {};",
                key_condition(dialect, &r.key)
            )
        })
        .collect()
}

/// The values a duplicate of `row` starts with: every column except the primary key and
/// `generated` ones.
pub fn duplicate_values(
    row: &[(String, Value)],
    pk: &[String],
    generated: &[String],
) -> Vec<(String, Value)> {
    row.iter()
        .filter(|(c, _)| !pk.contains(c) && !generated.contains(c))
        .cloned()
        .collect()
}

/// Columns of `detail` the server fills in itself: identity, serial / sequence defaults,
/// auto-increment and generated columns, detected from the default expression, the
/// type name and the column's DDL line.
pub fn generated_columns(detail: &ObjectDetail) -> Vec<String> {
    detail
        .columns
        .iter()
        .filter(|c| is_generated(c, &detail.ddl))
        .map(|c| c.name.clone())
        .collect()
}

fn is_generated(c: &ColumnInfo, ddl: &str) -> bool {
    const MARKERS: [&str; 5] = [
        "nextval",
        "identity",
        "autoincrement",
        "auto_increment",
        "iseq$$",
    ];
    let has = |s: &str| {
        let s = s.to_ascii_lowercase();
        MARKERS.iter().any(|m| s.contains(m)) || s.contains("generated ")
    };
    if c.default.as_deref().is_some_and(has) {
        return true;
    }
    let ty = c.data_type.to_ascii_lowercase();
    if ty.contains("serial") || has(&ty) {
        return true;
    }
    // A DDL line starting with the column name (bare or quoted).
    ddl.lines().any(|l| {
        let l = l.trim_start().trim_start_matches(['"', '[', '`']);
        l.len() > c.name.len()
            && l[..c.name.len()].eq_ignore_ascii_case(&c.name)
            && l[c.name.len()..].starts_with(['"', ']', '`', ' ', '\t'])
            && has(&l[c.name.len()..])
    })
}

/// Check a data-view filter (DBX-3a): exactly one SQL expression, nothing after it.
pub fn validate_where(dialect: &dyn Dialect, cond: &str) -> Result<(), String> {
    if let Some(r) = dialect.check_filter(cond) {
        return r;
    }
    let pd = dialect.parser_dialect();
    let mut parser = Parser::new(pd.as_ref())
        .try_with_sql(cond)
        .map_err(|e| e.to_string())?;
    parser.parse_expr().map_err(|e| e.to_string())?;
    match parser.peek_token().token {
        Token::EOF => Ok(()),
        t => Err(format!("Unexpected `{t}` after the condition")),
    }
}

/// The ORDER BY of a page: what the user picked, else the primary key, so pages are
/// stable (and SQL Server, which needs an ORDER BY for OFFSET, gets one).
pub fn page_order(user: &[SortKey], pk: &[String]) -> Vec<SortKey> {
    if user.is_empty() {
        pk.iter().map(SortKey::asc).collect()
    } else {
        user.to_vec()
    }
}

/// Tables that `DELETE` statements among `statements` delete from, found with
/// `sqlparser` (Production confirms these before a commit).
pub fn delete_targets(dialect: &dyn Dialect, statements: &[String]) -> Vec<String> {
    let pd = dialect.parser_dialect();
    let mut out = Vec::new();
    for sql in statements {
        let Ok(stmts) = Parser::parse_sql(pd.as_ref(), sql) else {
            continue;
        };
        for st in stmts {
            if let Statement::Delete(d) = st {
                let from = match &d.from {
                    sqlparser::ast::FromTable::WithFromKeyword(t)
                    | sqlparser::ast::FromTable::WithoutKeyword(t) => t,
                };
                out.extend(from.iter().map(|t| t.relation.to_string()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::postgres::PostgresDialect;
    use crate::value::DataType;

    fn cols(same: bool) -> Vec<ColumnMeta> {
        let mut a = ColumnMeta::new("id", "int8", DataType::Int64);
        a.table_id = Some(10);
        a.table_column = Some(1);
        let mut b = ColumnMeta::new("email", "text", DataType::Text);
        b.table_id = Some(if same { 10 } else { 11 });
        b.table_column = Some(2);
        vec![a, b]
    }

    #[test]
    fn detects_editable_results() {
        let d = PostgresDialect;
        assert_eq!(
            editable_table(
                &d,
                "select id, email from public.customers where id < 10",
                &cols(true)
            ),
            Some(EditTable {
                schema: Some("public".into()),
                table: "customers".into()
            })
        );
        assert!(editable_table(&d, "select * from a join b on true", &cols(true)).is_none());
        assert!(editable_table(&d, "select id, email from customers", &cols(false)).is_none());
        assert!(
            editable_table(&d, "select count(*) from customers group by 1", &cols(true)).is_none()
        );
        let mut computed = cols(true);
        computed[1].table_id = None;
        assert!(editable_table(&d, "select id, lower(email) from customers", &computed).is_none());
    }

    #[test]
    fn builds_updates() {
        let d = PostgresDialect;
        let t = EditTable {
            schema: None,
            table: "customers".into(),
        };
        let stmts = update_statements(
            &d,
            &t,
            &[
                RowEdit {
                    key: vec![("id".into(), Value::Int(10037))],
                    set: vec![("segment".into(), Value::Text("vip".into()))],
                },
                RowEdit {
                    key: vec![("id".into(), Value::Int(10148))],
                    set: vec![
                        ("email".into(), Value::Text("priya.nair@example.com".into())),
                        ("note".into(), Value::Null),
                    ],
                },
            ],
        );
        assert_eq!(
            stmts,
            [
                "UPDATE customers SET segment = 'vip' WHERE id = 10037;",
                "UPDATE customers SET email = 'priya.nair@example.com', note = NULL WHERE id = 10148;",
            ]
        );
    }

    /// Inserts, deletes and a duplicate for every dialect (DBX-3b).
    #[test]
    fn row_statements_per_dialect() {
        use crate::dialect::dialect_for;
        use crate::value::Engine;
        for engine in [
            Engine::Postgres,
            Engine::SqlServer,
            Engine::Oracle,
            Engine::Snowflake,
            Engine::D1,
            Engine::MySql,
        ] {
            let d = dialect_for(engine);
            let t = EditTable {
                schema: Some("sales".into()),
                table: "order line".into(),
            };
            let columns: Vec<String> = ["id", "line", "sku", "Qty", "note"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect();
            let generated = vec!["id".to_owned()];
            let pk = vec!["id".to_owned(), "line".to_owned()];
            let source = vec![
                ("id".to_owned(), Value::Int(7)),
                ("line".to_owned(), Value::Int(2)),
                ("sku".to_owned(), Value::Text("A-1".into())),
                ("Qty".to_owned(), Value::Int(3)),
                ("note".to_owned(), Value::Null),
            ];
            let inserts = [
                RowInsert {
                    values: vec![("sku".into(), Value::Text("O'Neil".into()))],
                },
                RowInsert { values: vec![] },
                RowInsert {
                    values: duplicate_values(&source, &pk, &generated),
                },
            ];
            let deletes = [
                RowDelete {
                    key: vec![("id".into(), Value::Int(7)), ("line".into(), Value::Int(2))],
                },
                RowDelete {
                    key: vec![("id".into(), Value::Int(8)), ("line".into(), Value::Null)],
                },
            ];
            let mut all = insert_statements(d, &t, &columns, &generated, &inserts);
            all.extend(delete_statements(d, &t, &deletes));
            let name = format!("{engine:?}").to_lowercase();
            insta::assert_snapshot!(format!("{name}_row_statements"), all.join("\n"));
        }
    }

    #[test]
    fn duplicates_skip_key_and_generated_columns() {
        let row = vec![
            ("id".to_owned(), Value::Int(1)),
            ("code".to_owned(), Value::Text("x".into())),
            ("seq".to_owned(), Value::Int(9)),
        ];
        assert_eq!(
            duplicate_values(&row, &["id".into()], &["seq".into()]),
            vec![("code".to_owned(), Value::Text("x".into()))]
        );
    }

    #[test]
    fn generated_columns_from_defaults_types_and_ddl() {
        let col = |name: &str, ty: &str, default: Option<&str>| ColumnInfo {
            name: name.into(),
            data_type: ty.into(),
            default: default.map(Into::into),
            ..ColumnInfo::default()
        };
        let d = ObjectDetail {
            columns: vec![
                col("id", "integer", Some("nextval('t_id_seq'::regclass)")),
                col("big", "bigserial", None),
                col("ora", "NUMBER", Some("\"APP\".\"ISEQ$$_7\".nextval")),
                col("ms", "int", None),
                col("name", "text", Some("'x'")),
                col("msx", "int", None),
            ],
            ddl: "CREATE TABLE t (\n  [ms] int IDENTITY(1,1) NOT NULL,\n  [msx] int NOT NULL,\n  name text\n)".into(),
            ..ObjectDetail::default()
        };
        assert_eq!(generated_columns(&d), ["id", "big", "ora", "ms"]);
    }

    #[test]
    fn where_conditions_must_be_one_expression() {
        let d = PostgresDialect;
        assert!(validate_where(&d, "total > 10 AND status IN ('a', 'b')").is_ok());
        assert!(validate_where(&d, "exists (select 1 from x where x.id = id)").is_ok());
        assert!(validate_where(&d, "1 = 1; DROP TABLE customers").is_err());
        assert!(validate_where(&d, "1 = 1) UNION SELECT 1 --").is_err());
        assert!(validate_where(&d, "total >").is_err());
    }

    #[test]
    fn page_order_defaults_to_the_key() {
        assert_eq!(page_order(&[], &["id".into()]), vec![SortKey::asc("id")]);
        let user = [SortKey {
            column: "name".into(),
            descending: true,
        }];
        assert_eq!(page_order(&user, &["id".into()]), user.to_vec());
    }

    #[test]
    fn key_conditions_and_delete_targets() {
        let d = PostgresDialect;
        assert_eq!(
            key_condition(
                &d,
                &[
                    ("customer_id".into(), Value::Int(4)),
                    ("Store".into(), Value::Text("n'1".into())),
                    ("x".into(), Value::Null)
                ]
            ),
            "customer_id = 4 AND \"Store\" = 'n''1' AND x IS NULL"
        );
        let stmts = vec![
            "UPDATE t SET a = 1 WHERE id = 1;".to_owned(),
            "DELETE FROM public.t WHERE id = 2;".to_owned(),
        ];
        assert_eq!(delete_targets(&d, &stmts), ["public.t"]);
    }
}
