//! SQLite `EXPLAIN QUERY PLAN` → [`Plan`].
//!
//! `EXPLAIN QUERY PLAN` returns rows of `(id, parent, notused, detail)`: a tree by
//! `parent`, with one text line per step (`SCAN orders`, `SEARCH c USING INDEX ix (a=?)`,
//! `USE TEMP B-TREE FOR ORDER BY`). It has no row estimates, costs or timings, so plans
//! are estimated only. Siblings run in order: the tables of a join are nested loops with
//! the first sibling outermost.

use std::collections::HashMap;

use crate::model::{Plan, PlanKind, PlanNode, PlanSource, Predicate, warn};
use crate::{PlanError, Result};

/// One `EXPLAIN QUERY PLAN` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Step id.
    pub id: i64,
    /// Parent step id (0: top level).
    pub parent: i64,
    /// The step's text.
    pub detail: String,
}

/// Build the plan for `sql` from its `EXPLAIN QUERY PLAN` rows.
pub fn parse(rows: &[Row], sql: &str) -> Result<Plan> {
    if rows.is_empty() {
        return Err(PlanError::Parse(
            "EXPLAIN QUERY PLAN returned no rows".into(),
        ));
    }
    let mut kids: HashMap<i64, Vec<&Row>> = HashMap::new();
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    for r in rows {
        // A parent that is not listed (or a self-reference) means top level.
        let parent = if r.parent != r.id && ids.contains(&r.parent) {
            r.parent
        } else {
            0
        };
        kids.entry(parent).or_default().push(r);
    }
    fn build(r: &Row, kids: &HashMap<i64, Vec<&Row>>, depth: usize) -> PlanNode {
        let mut n = step(&r.detail);
        if depth < 64 {
            for c in kids.get(&r.id).into_iter().flatten() {
                n.children.push(build(c, kids, depth + 1));
            }
        }
        n
    }
    let top: Vec<PlanNode> = kids
        .get(&0)
        .into_iter()
        .flatten()
        .map(|r| build(r, &kids, 0))
        .collect();
    let root = match <[PlanNode; 1]>::try_from(top) {
        Ok([only]) => only,
        Err(many) => {
            let mut r = PlanNode::op("Query Plan");
            r.children = many;
            r
        }
    };
    Ok(Plan::new(
        PlanSource::Sqlite,
        PlanKind::Estimated,
        sql,
        root,
    ))
}

/// One step's text → a node.
fn step(detail: &str) -> PlanNode {
    let mut n = PlanNode::op(detail);
    n.details.insert("Detail".into(), detail.to_owned());
    let (verb, rest) = detail.split_once(' ').unwrap_or((detail, ""));
    match verb {
        "SCAN" | "SEARCH" => access(&mut n, verb == "SEARCH", rest),
        "USE" if rest.starts_with("TEMP B-TREE FOR ") => {
            let what = rest.trim_start_matches("TEMP B-TREE FOR ");
            n.operation = "Temp B-Tree".into();
            n.details.insert("For".into(), what.to_owned());
            n.warnings.push(if what.contains("ORDER BY") {
                format!("{} (temp B-tree for {what})", warn::SORT)
            } else {
                format!("{} (temp B-tree for {what})", warn::TEMP_TABLE)
            });
        }
        "CO-ROUTINE" => {
            n.operation = "Co-routine".into();
            n.object = Some(rest.to_owned());
        }
        "MATERIALIZE" => {
            n.operation = "Materialize".into();
            n.object = Some(rest.to_owned());
        }
        "BLOOM" => {
            n.operation = "Bloom Filter".into();
            if let Some(on) = rest.strip_prefix("FILTER ON ") {
                let (table, cond) = on.split_once(' ').unwrap_or((on, ""));
                n.object = Some(table.to_owned());
                if !cond.is_empty() {
                    n.predicates.push(Predicate {
                        kind: "Index Cond".into(),
                        text: cond.to_owned(),
                    });
                }
            }
        }
        _ => {}
    }
    n
}

/// `SCAN t`, `SCAN TABLE t AS x`, `SCAN t USING COVERING INDEX ix`,
/// `SEARCH t USING INDEX ix (a=? AND b>?)`, `SEARCH t USING INTEGER PRIMARY KEY (rowid=?)`.
fn access(n: &mut PlanNode, search: bool, rest: &str) {
    let rest = rest.strip_prefix("TABLE ").unwrap_or(rest);
    if rest == "CONSTANT ROW" {
        n.operation = "Constant Row".into();
        return;
    }
    let (table, mut tail) = rest.split_once(' ').unwrap_or((rest, ""));
    // `AS alias` (older SQLite).
    if let Some(after) = tail.strip_prefix("AS ") {
        tail = after.split_once(' ').map_or("", |(_, t)| t);
    }
    let (using, cond) = match tail.find(" (") {
        Some(i) => (&tail[..i], Some(tail[i + 1..].trim())),
        None if tail.starts_with('(') => ("", Some(tail)),
        None => (tail, None),
    };
    let using = using.strip_prefix("USING ").unwrap_or(using).trim();
    let covering = using.contains("COVERING INDEX");
    let automatic = using.contains("AUTOMATIC");
    let index = using
        .rsplit_once("INDEX ")
        .map(|(_, ix)| ix.trim())
        .filter(|ix| !ix.is_empty() && !automatic);
    n.operation = match (search, using) {
        (_, u) if u.contains("PRIMARY KEY") => "Primary Key Search",
        (_, "") if !search => "Table Scan",
        (false, u) if u.starts_with("VIRTUAL TABLE") => "Virtual Table Scan",
        (false, _) if covering => "Covering Index Scan",
        (false, _) => "Index Scan",
        (true, _) if automatic => "Automatic Index Search",
        (true, _) if covering => "Covering Index Search",
        (true, _) => "Index Search",
    }
    .into();
    n.object = Some(match index {
        Some(ix) => format!("{table}.{ix}"),
        None => table.to_owned(),
    });
    if automatic {
        n.warnings
            .push("SQLite builds an automatic index on every run".into());
    }
    if let Some(c) = cond {
        n.predicates.push(Predicate {
            kind: "Index Cond".into(),
            text: c.to_owned(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops(details: &[&str]) -> Vec<String> {
        details
            .iter()
            .map(|d| {
                let n = step(d);
                format!(
                    "{} {} {}",
                    n.operation,
                    n.object.unwrap_or_default(),
                    n.predicates
                        .first()
                        .map(|p| p.text.clone())
                        .unwrap_or_default()
                )
                .trim()
                .to_owned()
            })
            .collect()
    }

    #[test]
    fn steps_read_like_the_engine_says() {
        assert_eq!(
            ops(&[
                "SCAN orders",
                "SCAN TABLE orders AS o",
                "SCAN o USING COVERING INDEX ix_total",
                "SEARCH c USING INTEGER PRIMARY KEY (rowid=?)",
                "SEARCH o USING INDEX ix_orders (customer_id=? AND total>?)",
                "SEARCH t USING AUTOMATIC COVERING INDEX (k=?)",
                "SCAN CONSTANT ROW",
                "MATERIALIZE recent",
            ]),
            [
                "Table Scan orders",
                "Table Scan orders",
                "Covering Index Scan o.ix_total",
                "Primary Key Search c (rowid=?)",
                "Index Search o.ix_orders (customer_id=? AND total>?)",
                "Automatic Index Search t (k=?)",
                "Constant Row",
                "Materialize recent",
            ]
        );
        let sort = step("USE TEMP B-TREE FOR ORDER BY");
        assert!(sort.warnings[0].starts_with(warn::SORT));
        let group = step("USE TEMP B-TREE FOR GROUP BY");
        assert!(group.warnings[0].starts_with(warn::TEMP_TABLE));
    }

    #[test]
    fn rows_become_a_tree() {
        let row = |id, parent, detail: &str| Row {
            id,
            parent,
            detail: detail.into(),
        };
        let plan = parse(
            &[
                row(2, 0, "SCAN o"),
                row(5, 0, "SEARCH c USING INTEGER PRIMARY KEY (rowid=?)"),
                row(9, 0, "CORRELATED SCALAR SUBQUERY 1"),
                row(12, 9, "SCAN x"),
            ],
            "select",
        )
        .unwrap();
        assert_eq!(plan.root.operation, "Query Plan");
        assert_eq!(plan.root.children.len(), 3);
        assert_eq!(plan.root.children[2].children[0].operation, "Table Scan");
        assert!(parse(&[], "select").is_err());
        let single = parse(&[row(3, 0, "SCAN t")], "select").unwrap();
        assert_eq!(single.root.operation, "Table Scan");
    }
}
