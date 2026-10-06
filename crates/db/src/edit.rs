//! Inline editing: which results are editable, and the UPDATE statements that apply staged
//! cell edits. Every statement targets exactly one row through its primary key.

use crate::batch::ColumnMeta;
use crate::complete::table_refs;
use crate::dialect::Dialect;
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
    let first = sql.split_whitespace()
        .next()?
        .to_ascii_uppercase();
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

/// UPDATE statements, one per row.
pub fn update_statements(
    dialect: &dyn Dialect,
    table: &EditTable,
    rows: &[RowEdit],
) -> Vec<String> {
    let target = match &table.schema {
        Some(s) => dialect.qualified(s, &table.table),
        None => dialect.quote_ident(&table.table),
    };
    rows.iter()
        .filter(|r| !r.set.is_empty() && !r.key.is_empty())
        .map(|r| {
            let set: Vec<String> = r
                .set
                .iter()
                .map(|(c, v)| format!("{} = {}", dialect.quote_ident(c), dialect.literal(v)))
                .collect();
            let cond: Vec<String> = r
                .key
                .iter()
                .map(|(c, v)| match v {
                    Value::Null => format!("{} IS NULL", dialect.quote_ident(c)),
                    v => format!("{} = {}", dialect.quote_ident(c), dialect.literal(v)),
                })
                .collect();
            format!(
                "UPDATE {target} SET {} WHERE {};",
                set.join(", "),
                cond.join(" AND ")
            )
        })
        .collect()
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
}
