//! MySQL / MariaDB migration DDL. A MySQL schema is a database, so the script starts with
//! `USE <target>` (routines from `SHOW CREATE` are not qualified).

use super::diff::Normalizer;
use super::model::{CheckDef, FkDef, IndexDef, KeyDef};
use super::script::{SchemaScripter, Stmt};
use super::text::{collapse_ws, replace_create, semi};
use crate::catalog::{ColumnInfo, ObjectKind};
use crate::dialect::mysql::MySqlDialect;
use crate::dialect::{Dialect, Flavor};

/// MySQL / MariaDB schema compare scripts.
#[derive(Clone, Copy, Debug, Default)]
pub struct MySqlScripter;

/// `DEFINER=`user`@`host`` dropped: the script creates objects as whoever runs it, and
/// two servers' definers would otherwise always differ.
fn strip_definer(ddl: &str) -> String {
    let Some(at) = find_ci(ddl, "DEFINER=") else {
        return ddl.to_owned();
    };
    // The account ends at the next whitespace outside backticks / quotes.
    let rest = &ddl[at + "DEFINER=".len()..];
    let mut quote: Option<char> = None;
    let mut end = rest.len();
    for (i, c) in rest.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '`' || c == '\'' || c == '"' => quote = Some(c),
            None if c.is_whitespace() => {
                end = i;
                break;
            }
            None => {}
        }
    }
    let tail = rest[end..].trim_start();
    format!("{}{tail}", &ddl[..at])
}

/// Byte offset of `needle` (ASCII, case-insensitive) in `hay`.
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    hay.to_ascii_uppercase().find(&needle.to_ascii_uppercase())
}

/// A default from `information_schema.COLUMNS` as SQL: MySQL 8 reports string defaults
/// unquoted (`abc`), MariaDB quoted (`'abc'`); expressions and keywords stay as they are.
fn default_sql(d: &str, data_type: &str) -> String {
    let t = d.trim();
    let upper = t.to_ascii_uppercase();
    let numeric_type = [
        "int",
        "tinyint",
        "smallint",
        "mediumint",
        "bigint",
        "decimal",
        "numeric",
        "float",
        "double",
        "bit",
        "year",
    ]
    .iter()
    .any(|n| data_type.to_ascii_lowercase().starts_with(n));
    // Already SQL: a string literal, an expression, NULL, a time keyword or a number.
    let verbatim = t.starts_with('\'')
        || t.starts_with('(')
        || upper == "NULL"
        || [
            "CURRENT_TIMESTAMP",
            "NOW(",
            "CURRENT_DATE",
            "LOCALTIMESTAMP",
        ]
        .iter()
        .any(|k| upper.starts_with(k))
        || (numeric_type && t.parse::<f64>().is_ok());
    if verbatim {
        t.to_owned()
    } else if t.contains('(') {
        // An expression default (MySQL 8.0.13+).
        format!("({t})")
    } else {
        format!("'{}'", t.replace('\\', "\\\\").replace('\'', "''"))
    }
}

impl MySqlScripter {
    /// Index key part: `col` or `col(10)` (prefix length) quoted.
    fn key_part(&self, c: &str) -> String {
        match c.strip_suffix(')').and_then(|s| s.rsplit_once('(')) {
            Some((name, len)) if len.chars().all(|ch| ch.is_ascii_digit()) => {
                format!("{}({len})", self.quote(name))
            }
            _ => self.quote(c),
        }
    }
}

impl SchemaScripter for MySqlScripter {
    fn dialect(&self) -> &'static dyn Dialect {
        &MySqlDialect
    }

    fn preamble(&self, target_schema: &str) -> Vec<String> {
        vec![format!("USE {};", self.quote(target_schema))]
    }

    fn normalize_definition(&self, ddl: &str) -> String {
        collapse_ws(&strip_definer(ddl))
    }

    fn normalize_default(&self, default: &str) -> String {
        let t = collapse_ws(default);
        // MariaDB quotes string defaults, MySQL does not.
        t.strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
            .map_or(t.clone(), str::to_owned)
    }

    fn column_def(&self, c: &ColumnInfo) -> String {
        let mut s = format!("{} {}", self.quote(&c.name), c.data_type);
        s.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
        if let Some(d) = &c.default {
            s.push_str(" DEFAULT ");
            s.push_str(&default_sql(d, &c.data_type));
        }
        if let Some(i) = &c.identity {
            s.push(' ');
            s.push_str(i);
        }
        s
    }

    fn alter_column(&self, table: &str, _from: &ColumnInfo, to: &ColumnInfo) -> Vec<Stmt> {
        vec![Stmt::new(format!(
            "ALTER TABLE {table} MODIFY COLUMN {};",
            self.column_def(to)
        ))]
    }

    fn add_primary_key(&self, table: &str, pk: &KeyDef) -> Stmt {
        // MySQL names every primary key PRIMARY.
        Stmt::new(format!(
            "ALTER TABLE {table} ADD PRIMARY KEY ({});",
            self.column_list(&pk.columns)
        ))
    }

    fn drop_primary_key(&self, table: &str, _pk: &KeyDef) -> Stmt {
        Stmt::new(format!("ALTER TABLE {table} DROP PRIMARY KEY;"))
    }

    fn constraint_prefix(&self, name: &str) -> String {
        if name.is_empty() || name == "PRIMARY" {
            String::new()
        } else {
            format!("CONSTRAINT {} ", self.quote(name))
        }
    }

    fn drop_unique(&self, table: &str, u: &KeyDef) -> Stmt {
        Stmt::new(format!(
            "ALTER TABLE {table} DROP INDEX {};",
            self.quote(&u.name)
        ))
    }

    fn drop_check(&self, table: &str, c: &CheckDef) -> Stmt {
        // `DROP CONSTRAINT` works for checks on MySQL 8.0.19+ and MariaDB.
        Stmt::new(format!(
            "ALTER TABLE {table} DROP CONSTRAINT {};",
            self.quote(&c.name)
        ))
    }

    fn drop_foreign_key(&self, table: &str, fk: &FkDef) -> Stmt {
        Stmt::new(format!(
            "ALTER TABLE {table} DROP FOREIGN KEY {};",
            self.quote(&fk.name)
        ))
    }

    fn create_index(&self, schema: &str, table: &str, ix: &IndexDef, _n: &Normalizer<'_>) -> Stmt {
        let method = ix.method.as_deref().unwrap_or("").to_ascii_uppercase();
        let (prefix, using) = match method.as_str() {
            "FULLTEXT" => ("FULLTEXT ", String::new()),
            "SPATIAL" => ("SPATIAL ", String::new()),
            "HASH" => ("", " USING HASH".to_owned()),
            _ => ("", String::new()),
        };
        let unique = if ix.unique { "UNIQUE " } else { "" };
        let cols = ix
            .columns
            .iter()
            .map(|c| self.key_part(c))
            .collect::<Vec<_>>()
            .join(", ");
        Stmt::new(format!(
            "CREATE {unique}{prefix}INDEX {} ON {} ({cols}){using};",
            self.quote(&ix.name),
            self.qualified(schema, table)
        ))
    }

    fn drop_index(&self, schema: &str, table: &str, ix: &IndexDef) -> Stmt {
        Stmt::new(format!(
            "DROP INDEX {} ON {};",
            self.quote(&ix.name),
            self.qualified(schema, table)
        ))
    }

    fn create_view(
        &self,
        _kind: ObjectKind,
        _schema: &str,
        _name: &str,
        ddl: &str,
        _exists: bool,
    ) -> Vec<Stmt> {
        let ddl = strip_definer(ddl.trim());
        vec![Stmt::new(semi(&replace_create(
            &ddl,
            Flavor::MySql,
            "CREATE OR REPLACE",
        )))]
    }

    fn create_routine(
        &self,
        kind: ObjectKind,
        schema: &str,
        name: &str,
        signature: Option<&str>,
        ddl: &str,
        exists: bool,
    ) -> Vec<Stmt> {
        let mut out = Vec::new();
        if exists {
            out.push(self.drop_routine(kind, schema, name, signature));
        }
        // The body holds `;`: the mysql client convention changes the delimiter.
        let body = strip_definer(ddl.trim());
        let body = body.trim_end().trim_end_matches(';');
        out.push(Stmt::new(format!("DELIMITER $$\n{body}\n$$\nDELIMITER ;")));
        out
    }

    fn drop_routine(
        &self,
        kind: ObjectKind,
        schema: &str,
        name: &str,
        _signature: Option<&str>,
    ) -> Stmt {
        let kw = if kind == ObjectKind::Procedure {
            "PROCEDURE"
        } else {
            "FUNCTION"
        };
        Stmt::new(format!(
            "DROP {kw} IF EXISTS {};",
            self.qualified(schema, name)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definers_are_dropped() {
        assert_eq!(
            strip_definer(
                "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v` AS select 1"
            ),
            "CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `v` AS select 1"
        );
        assert_eq!(
            strip_definer("CREATE DEFINER=`a b`@`localhost` PROCEDURE `p`() BEGIN END"),
            "CREATE PROCEDURE `p`() BEGIN END"
        );
        assert_eq!(
            strip_definer("CREATE VIEW v AS SELECT 1"),
            "CREATE VIEW v AS SELECT 1"
        );
    }

    #[test]
    fn defaults_are_quoted_when_they_are_strings() {
        assert_eq!(default_sql("abc", "varchar(10)"), "'abc'");
        assert_eq!(default_sql("'abc'", "varchar(10)"), "'abc'");
        assert_eq!(default_sql("0", "int"), "0");
        assert_eq!(default_sql("1.5", "decimal(4,1)"), "1.5");
        assert_eq!(default_sql("12", "varchar(4)"), "'12'");
        assert_eq!(
            default_sql("CURRENT_TIMESTAMP", "timestamp"),
            "CURRENT_TIMESTAMP"
        );
        assert_eq!(default_sql("uuid()", "char(36)"), "(uuid())");
        assert_eq!(default_sql("it's", "text"), "'it''s'");
    }
}
