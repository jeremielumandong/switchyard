//! SQL snippets (DBX-4b): the user's snippets stored in the `snippets` table, built-ins
//! defined in code per engine, prefix matching and placeholder expansion.
//!
//! Bodies use `${n:default}` tab stops (also `${n}`). A bare `$n` is left alone so
//! PostgreSQL bind parameters (`$1`) survive; `\$` escapes a literal `$`.

use std::ops::Range;

use serde::{Deserialize, Serialize};
use switchyard_db::Engine;

use crate::model::ValidationError;

/// Id prefix of built-in snippets (never stored).
pub const BUILTIN_ID_PREFIX: &str = "builtin:";

/// A SQL snippet offered by editor completion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snippet {
    /// Row id (`builtin:<engine>:<prefix>` for built-ins; empty for a new snippet).
    pub id: String,
    /// Display name.
    pub name: String,
    /// The word that triggers it in completion.
    pub prefix: String,
    /// Text inserted, with `${n:placeholder}` tab stops.
    pub body: String,
    /// Engine it applies to; `None` for every engine.
    pub engine: Option<Engine>,
    /// Creation time, ms since epoch (0 for built-ins).
    pub created_at: i64,
    /// Last change, ms since epoch (0 for built-ins).
    pub updated_at: i64,
}

impl Snippet {
    /// A new, unsaved user snippet.
    pub fn new(name: &str, prefix: &str, body: &str, engine: Option<Engine>) -> Self {
        Self {
            id: String::new(),
            name: name.to_owned(),
            prefix: prefix.to_owned(),
            body: body.to_owned(),
            engine,
            created_at: 0,
            updated_at: 0,
        }
    }

    /// Defined in code, not stored.
    pub fn is_builtin(&self) -> bool {
        self.id.starts_with(BUILTIN_ID_PREFIX)
    }

    /// Whether it is offered for `engine`.
    pub fn applies_to(&self, engine: Engine) -> bool {
        self.engine.is_none_or(|e| e == engine)
    }

    /// Check the fields a user can edit.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let err = |field, message: &str| ValidationError {
            field,
            message: message.to_owned(),
        };
        if self.name.trim().is_empty() {
            return Err(err("name", "Name is required"));
        }
        if self.prefix.trim().is_empty() {
            return Err(err("prefix", "Prefix is required"));
        }
        if !self
            .prefix
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            return Err(err("prefix", "Prefix: letters, digits, '_' or '-' only"));
        }
        if self.body.trim().is_empty() {
            return Err(err("body", "Body is required"));
        }
        Ok(())
    }
}

/// The engine's key as stored in the `engine` column (`postgres`, `sqlserver`, …).
pub(crate) fn engine_key(engine: Engine) -> String {
    serde_json::to_value(engine)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Parse an `engine` column value; unknown keys read as "every engine".
pub(crate) fn engine_from_key(key: &str) -> Option<Engine> {
    serde_json::from_value(serde_json::Value::String(key.to_owned())).ok()
}

fn builtin(engine: Engine, prefix: &str, name: &str, body: &str) -> Snippet {
    Snippet {
        id: format!("{BUILTIN_ID_PREFIX}{}:{prefix}", engine_key(engine)),
        name: name.to_owned(),
        prefix: prefix.to_owned(),
        body: body.to_owned(),
        engine: Some(engine),
        created_at: 0,
        updated_at: 0,
    }
}

/// Snippets shipped with the app for `engine`, in display order.
pub fn builtin_snippets(engine: Engine) -> Vec<Snippet> {
    let select = match engine {
        Engine::SqlServer => "SELECT TOP (${3:100}) ${2:*}\nFROM ${1:table_name};",
        Engine::Oracle => "SELECT ${2:*}\nFROM ${1:table_name}\nFETCH FIRST ${3:100} ROWS ONLY;",
        Engine::Postgres | Engine::D1 | Engine::Snowflake => {
            "SELECT ${2:*}\nFROM ${1:table_name}\nLIMIT ${3:100};"
        }
    };
    let mut out = vec![
        builtin(engine, "sel", "Select rows", select),
        builtin(
            engine,
            "ins",
            "Insert row",
            "INSERT INTO ${1:table_name} (${2:column1}, ${3:column2})\nVALUES (${4:value1}, ${5:value2});",
        ),
        builtin(
            engine,
            "upd",
            "Update with WHERE",
            "UPDATE ${1:table_name}\nSET ${2:column1} = ${3:value1}\nWHERE ${4:condition};",
        ),
        builtin(
            engine,
            "cte",
            "Common table expression",
            "WITH ${1:cte_name} AS (\n    SELECT ${2:*}\n    FROM ${3:table_name}\n)\nSELECT *\nFROM ${1:cte_name};",
        ),
    ];
    // Snowflake has no secondary indexes on standard tables.
    if engine != Engine::Snowflake {
        out.push(builtin(
            engine,
            "idx",
            "Create index",
            "CREATE INDEX ${1:index_name}\n    ON ${2:table_name} (${3:column1});",
        ));
    }
    let tran = match engine {
        Engine::SqlServer => {
            Some("BEGIN TRANSACTION;\n\n${1:-- statements}\n\nCOMMIT TRANSACTION;")
        }
        Engine::Postgres => Some("BEGIN;\n\n${1:-- statements}\n\nCOMMIT;"),
        // Oracle starts a transaction implicitly with the first DML.
        Engine::Oracle => Some("SET TRANSACTION READ WRITE;\n\n${1:-- statements}\n\nCOMMIT;"),
        // Not supported over these engines' HTTP APIs (`Engine::supports_transactions`).
        Engine::D1 | Engine::Snowflake => None,
    };
    if let Some(body) = tran {
        out.push(builtin(engine, "tran", "Transaction", body));
    }
    out
}

/// Built-ins plus user snippets that apply to `engine`. A user snippet with the same
/// prefix (case-insensitive) as a built-in replaces it. Sorted by prefix.
pub fn snippets_for(engine: Engine, user: &[Snippet]) -> Vec<Snippet> {
    let mine: Vec<&Snippet> = user.iter().filter(|s| s.applies_to(engine)).collect();
    let mut out: Vec<Snippet> = builtin_snippets(engine)
        .into_iter()
        .filter(|b| {
            !mine
                .iter()
                .any(|u| u.prefix.eq_ignore_ascii_case(&b.prefix))
        })
        .collect();
    out.extend(mine.into_iter().cloned());
    out.sort_by(|a, b| {
        a.prefix
            .to_lowercase()
            .cmp(&b.prefix.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// Snippets whose prefix starts with `typed` (case-insensitive). Nothing for an empty word.
pub fn matching<'a>(snippets: &'a [Snippet], typed: &str) -> Vec<&'a Snippet> {
    let typed = typed.trim().to_lowercase();
    if typed.is_empty() {
        return Vec::new();
    }
    snippets
        .iter()
        .filter(|s| s.prefix.to_lowercase().starts_with(&typed))
        .collect()
}

/// A snippet body with its tab stops resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expansion {
    /// Text to insert: each placeholder replaced by its default text.
    pub text: String,
    /// Byte range in `text` of the first tab stop's default (lowest `n` ≥ 1, else `$0`);
    /// select it after inserting. `None`: put the cursor at the end.
    pub first: Option<Range<usize>>,
}

/// Replace `${n:default}` / `${n}` tab stops with their default text and find the first.
/// Malformed placeholders are inserted as written.
pub fn expand(body: &str) -> Expansion {
    let mut text = String::with_capacity(body.len());
    // (tab stop number, range of its default text in `text`)
    let mut stops: Vec<(u32, Range<usize>)> = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find(['$', '\\']) {
        text.push_str(&rest[..i]);
        let tail = &rest[i..];
        if let Some(after) = tail.strip_prefix("\\$") {
            text.push('$');
            rest = after;
            continue;
        }
        if let Some(after) = tail.strip_prefix('\\') {
            text.push('\\');
            rest = after;
            continue;
        }
        match parse_stop(tail) {
            Some((n, default, used)) => {
                let start = text.len();
                text.push_str(default);
                stops.push((n, start..text.len()));
                rest = &tail[used..];
            }
            None => {
                text.push('$');
                rest = &tail[1..];
            }
        }
    }
    text.push_str(rest);
    let first = stops
        .iter()
        .filter(|(n, _)| *n >= 1)
        .min_by_key(|(n, r)| (*n, r.start))
        .or_else(|| stops.iter().find(|(n, _)| *n == 0))
        .map(|(_, r)| r.clone());
    Expansion { text, first }
}

/// Parse `${n}` or `${n:default}` at the start of `s`: (n, default, bytes consumed).
fn parse_stop(s: &str) -> Option<(u32, &str, usize)> {
    let inner = s.strip_prefix("${")?;
    let digits = inner.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let n: u32 = inner[..digits].parse().ok()?;
    let after = &inner[digits..];
    if after.starts_with('}') {
        return Some((n, "", 2 + digits + 1));
    }
    let default_and_rest = after.strip_prefix(':')?;
    let end = default_and_rest.find('}')?;
    Some((n, &default_and_rest[..end], 2 + digits + 1 + end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(prefix: &str, engine: Option<Engine>) -> Snippet {
        let mut s = Snippet::new(&format!("mine {prefix}"), prefix, "SELECT 1;", engine);
        s.id = format!("u-{prefix}");
        s
    }

    #[test]
    fn builtins_per_engine() {
        for engine in [
            Engine::Postgres,
            Engine::SqlServer,
            Engine::Oracle,
            Engine::Snowflake,
            Engine::D1,
        ] {
            let rendered: String = builtin_snippets(engine)
                .iter()
                .map(|s| format!("-- {} ({}) [{}]\n{}\n\n", s.name, s.prefix, s.id, s.body))
                .collect();
            insta::assert_snapshot!(format!("builtins_{}", engine_key(engine)), rendered);
            assert!(builtin_snippets(engine).iter().all(|s| s.is_builtin()));
            assert!(
                builtin_snippets(engine)
                    .iter()
                    .all(|s| s.validate().is_ok())
            );
        }
    }

    #[test]
    fn prefix_matching() {
        let all = snippets_for(Engine::Postgres, &[]);
        let names = |typed: &str| -> Vec<String> {
            matching(&all, typed)
                .iter()
                .map(|s| s.prefix.clone())
                .collect()
        };
        assert_eq!(names("se"), ["sel"]);
        assert_eq!(names("SEL"), ["sel"]);
        assert_eq!(names("t"), ["tran"]);
        assert!(names("").is_empty());
        assert!(names("select").is_empty());
        assert!(names("x").is_empty());
    }

    #[test]
    fn engine_filtering() {
        let user_snips = vec![
            user("pgstat", Some(Engine::Postgres)),
            user("who", Some(Engine::SqlServer)),
            user("cnt", None),
        ];
        let pg: Vec<_> = snippets_for(Engine::Postgres, &user_snips)
            .into_iter()
            .map(|s| s.prefix)
            .collect();
        assert!(pg.contains(&"pgstat".to_owned()));
        assert!(pg.contains(&"cnt".to_owned()));
        assert!(!pg.contains(&"who".to_owned()));
        let ms: Vec<_> = snippets_for(Engine::SqlServer, &user_snips)
            .into_iter()
            .map(|s| s.prefix)
            .collect();
        assert!(ms.contains(&"who".to_owned()));
        assert!(!ms.contains(&"pgstat".to_owned()));
        // No index snippet on Snowflake, no transaction snippet on D1 / Snowflake.
        let sf: Vec<_> = snippets_for(Engine::Snowflake, &[])
            .into_iter()
            .map(|s| s.prefix)
            .collect();
        assert!(!sf.contains(&"idx".to_owned()) && !sf.contains(&"tran".to_owned()));
    }

    #[test]
    fn user_snippet_overrides_builtin() {
        let mut mine = user("SEL", None);
        mine.body = "SELECT * FROM ${1:t};".into();
        let pg = snippets_for(Engine::Postgres, std::slice::from_ref(&mine));
        let sel: Vec<_> = pg
            .iter()
            .filter(|s| s.prefix.eq_ignore_ascii_case("sel"))
            .collect();
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].id, "u-SEL");
        // Other built-ins remain.
        assert!(pg.iter().any(|s| s.prefix == "upd" && s.is_builtin()));
        // An override for another engine does not hide this engine's built-in.
        let ms_only = user("sel", Some(Engine::SqlServer));
        let pg = snippets_for(Engine::Postgres, &[ms_only]);
        assert!(pg.iter().any(|s| s.prefix == "sel" && s.is_builtin()));
    }

    #[test]
    fn placeholder_expansion() {
        let e = expand("SELECT ${2:*}\nFROM ${1:table_name}\nLIMIT ${3:100};");
        assert_eq!(e.text, "SELECT *\nFROM table_name\nLIMIT 100;");
        assert_eq!(&e.text[e.first.clone().unwrap()], "table_name");

        // Repeated stop: first occurrence wins; empty stop; $0 fallback.
        let e = expand("WITH ${1:c} AS (${2}) SELECT * FROM ${1:c};");
        assert_eq!(e.text, "WITH c AS () SELECT * FROM c;");
        assert_eq!(e.first, Some(5..6));
        let e = expand("BEGIN; ${0} COMMIT;");
        assert_eq!(e.text, "BEGIN;  COMMIT;");
        assert_eq!(e.first, Some(7..7));

        // No tab stops; PG bind parameters and dollar quotes survive; escapes.
        let e = expand("SELECT $1, $$x$$, \\${1:y}, ${a}, ${2:unterminated");
        assert_eq!(e.text, "SELECT $1, $$x$$, ${1:y}, ${a}, ${2:unterminated");
        assert_eq!(e.first, None);

        // Multi-byte defaults.
        let e = expand("SELECT '${1:é}'");
        assert_eq!(&e.text[e.first.unwrap()], "é");
    }

    #[test]
    fn validation() {
        assert!(Snippet::new("n", "p", "b", None).validate().is_ok());
        assert!(Snippet::new("", "p", "b", None).validate().is_err());
        assert!(Snippet::new("n", "", "b", None).validate().is_err());
        assert!(
            Snippet::new("n", "has space", "b", None)
                .validate()
                .is_err()
        );
        assert!(Snippet::new("n", "p", "  ", None).validate().is_err());
    }

    #[test]
    fn engine_keys_round_trip() {
        assert_eq!(engine_key(Engine::SqlServer), "sqlserver");
        assert_eq!(engine_from_key("sqlserver"), Some(Engine::SqlServer));
        assert_eq!(engine_from_key("nope"), None);
    }
}
