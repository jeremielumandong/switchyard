//! Hypothetical indexes (PostgreSQL + HypoPG): the estimated plan of a statement as if some
//! indexes existed, without creating them.
//!
//! HypoPG keeps hypothetical indexes in the backend's memory only; the planner sees them in
//! plain `EXPLAIN` (never `EXPLAIN ANALYZE`). Everything here happens in the caller's session
//! and the indexes are always removed with `hypopg_reset()` afterwards, also on error.

use serde::{Deserialize, Serialize};
use sqlparser::ast::Statement;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use switchyard_db::driver::DbSession;
use switchyard_db::value::{Engine, Value};

use crate::access::{MARKER, probe};
use crate::capture::{Mode, capture};
use crate::compare::{Comparison, compare};
use crate::model::Plan;
use crate::{PlanError, Result};

/// One hypothetical index as HypoPG created it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HypoIndex {
    /// The `CREATE INDEX` statement it was made from.
    pub definition: String,
    /// HypoPG's generated name (`<13543>btree_orders_status`).
    pub name: String,
    /// Estimated size, bytes.
    pub bytes: Option<f64>,
}

/// The statement planned without and with the hypothetical indexes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WhatIf {
    /// Estimated plan as things are.
    pub before: Plan,
    /// Estimated plan with the hypothetical indexes.
    pub after: Plan,
    /// The indexes that were simulated.
    pub indexes: Vec<HypoIndex>,
    /// Node-by-node difference.
    pub comparison: Comparison,
}

impl WhatIf {
    /// Whether the planner chose a plan that uses any hypothetical index.
    pub fn uses_hypothetical(&self) -> bool {
        let names: Vec<&str> = self.indexes.iter().map(|i| i.name.as_str()).collect();
        let mut stack = vec![&self.after.root];
        while let Some(n) = stack.pop() {
            if n.object
                .as_deref()
                .is_some_and(|ix| names.iter().any(|h| ix.contains(h)))
            {
                return true;
            }
            stack.extend(n.children.iter());
        }
        false
    }
}

/// Check that `definition` is exactly one `CREATE INDEX` statement.
pub fn validate(definition: &str) -> Result<String> {
    let text = definition.trim().trim_end_matches(';').trim();
    let parsed = Parser::parse_sql(&PostgreSqlDialect {}, text)
        .map_err(|e| PlanError::Unsupported(format!("not a valid CREATE INDEX: {e}")))?;
    match parsed.as_slice() {
        [Statement::CreateIndex(_)] => Ok(text.to_owned()),
        _ => Err(PlanError::Unsupported(
            "hypothetical indexes take exactly one CREATE INDEX statement each".into(),
        )),
    }
}

/// Plan `sql` without and with the hypothetical `indexes` (each a `CREATE INDEX`).
pub async fn what_if(
    session: &mut dyn DbSession,
    engine: Engine,
    sql: &str,
    indexes: &[String],
) -> Result<WhatIf> {
    if engine != Engine::Postgres {
        return Err(PlanError::Unsupported(
            "hypothetical indexes need PostgreSQL with HypoPG".into(),
        ));
    }
    if indexes.is_empty() {
        return Err(PlanError::Unsupported(
            "add at least one index to try".into(),
        ));
    }
    let defs: Vec<String> = indexes.iter().map(|d| validate(d)).collect::<Result<_>>()?;
    let installed = probe(
        session,
        engine,
        &format!("SELECT {MARKER} extversion FROM pg_extension WHERE extname = 'hypopg'"),
        &[],
    )
    .await?;
    if installed.len() == 0 {
        return Err(PlanError::Unsupported(
            "HypoPG is not installed in this database (CREATE EXTENSION hypopg;)".into(),
        ));
    }

    let before = capture(session, engine, sql, Mode::Estimated).await?;
    // Start clean, so earlier experiments in this session do not leak in.
    reset(session, engine).await?;
    let result = simulate(session, engine, sql, &defs).await;
    let cleaned = reset(session, engine).await;
    let (after, created) = result?;
    cleaned?;
    let comparison = compare(&before, &after);
    Ok(WhatIf {
        before,
        after,
        indexes: created,
        comparison,
    })
}

async fn simulate(
    session: &mut dyn DbSession,
    engine: Engine,
    sql: &str,
    defs: &[String],
) -> Result<(Plan, Vec<HypoIndex>)> {
    let mut created = Vec::with_capacity(defs.len());
    for def in defs {
        let r = probe(
            session,
            engine,
            &format!(
                "SELECT {MARKER} indexname, hypopg_relation_size(indexrelid) AS bytes
                   FROM hypopg_create_index($1)"
            ),
            &[Value::Text(def.clone())],
        )
        .await?;
        created.push(HypoIndex {
            definition: def.clone(),
            name: r.text(0, "indexname").unwrap_or("").to_owned(),
            bytes: r.num(0, "bytes"),
        });
    }
    let after = capture(session, engine, sql, Mode::Estimated).await?;
    Ok((after, created))
}

async fn reset(session: &mut dyn DbSession, engine: Engine) -> Result<()> {
    probe(
        session,
        engine,
        &format!("SELECT {MARKER} hypopg_reset()"),
        &[],
    )
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_create_index_is_accepted() {
        assert_eq!(
            validate(" CREATE INDEX ON orders (status); ")
                .ok()
                .as_deref(),
            Some("CREATE INDEX ON orders (status)")
        );
        assert!(validate("CREATE INDEX ON t (a); DROP TABLE t").is_err());
        assert!(validate("DROP TABLE orders").is_err());
        assert!(validate("select 1").is_err());
    }
}
