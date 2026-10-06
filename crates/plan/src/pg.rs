//! PostgreSQL `EXPLAIN (FORMAT JSON)` → [`Plan`].
//!
//! PostgreSQL reports rows and times per loop: "Actual Total Time" is the average per
//! loop, so a node's subtree time is that times "Actual Loops". Buffers are already
//! totals across loops.

use serde_json::Value;

use crate::model::{Io, Plan, PlanKind, PlanNode, PlanSource, Predicate};
use crate::{PlanError, Result};

/// Keys copied into [`PlanNode::predicates`], in display order.
const PREDICATES: &[&str] = &[
    "Index Cond",
    "Recheck Cond",
    "Hash Cond",
    "Merge Cond",
    "Join Filter",
    "Filter",
    "One-Time Filter",
];

/// Keys copied into [`PlanNode::details`] when present.
const DETAILS: &[&str] = &[
    "Join Type",
    "Strategy",
    "Partial Mode",
    "Parent Relationship",
    "Subplan Name",
    "CTE Name",
    "Scan Direction",
    "Sort Method",
    "Sort Space Type",
    "Sort Space Used",
    "Hash Buckets",
    "Hash Batches",
    "Peak Memory Usage",
    "Rows Removed by Filter",
    "Rows Removed by Join Filter",
    "Rows Removed by Index Recheck",
    "Heap Fetches",
    "Workers Planned",
    "Workers Launched",
    "Startup Cost",
];

/// Parse the JSON text `EXPLAIN (FORMAT JSON)` returns for `sql`.
pub fn parse(json: &str, sql: &str) -> Result<Plan> {
    let doc: Value =
        serde_json::from_str(json.trim()).map_err(|e| PlanError::Parse(e.to_string()))?;
    // `[ { "Plan": …, "Planning Time": …, "Execution Time": … } ]`
    let top = doc
        .as_array()
        .and_then(|a| a.first())
        .or(Some(&doc))
        .filter(|t| t.get("Plan").is_some())
        .ok_or_else(|| PlanError::Parse("no \"Plan\" in EXPLAIN output".into()))?;
    let mut root = node(&top["Plan"]);
    nest_ctes(&mut root);
    let kind = if root.actual_rows.is_some() {
        PlanKind::Actual
    } else {
        PlanKind::Estimated
    };
    let mut plan = Plan::new(PlanSource::Postgres, kind, sql, root);
    plan.planning_ms = top["Planning Time"].as_f64();
    plan.execution_ms = top["Execution Time"].as_f64();
    if let Some(triggers) = top["Triggers"].as_array() {
        for t in triggers {
            plan.warnings.push(format!(
                "Trigger {} ran {} times ({:.1} ms)",
                t["Trigger Name"].as_str().unwrap_or("?"),
                t["Calls"].as_f64().unwrap_or(0.0),
                t["Time"].as_f64().unwrap_or(0.0)
            ));
        }
    }
    Ok(plan)
}

/// A materialized CTE is listed as an InitPlan of some ancestor, but it runs inside the
/// first `CTE Scan` that reads it, whose time already includes it. Move each CTE body under
/// that scan so self times are not counted twice.
fn nest_ctes(root: &mut PlanNode) {
    fn take(n: &mut PlanNode, out: &mut Vec<(String, PlanNode)>) {
        let mut i = 0;
        while i < n.children.len() {
            let cte = n.children[i]
                .details
                .get("Subplan Name")
                .and_then(|s| s.strip_prefix("CTE "))
                .map(str::to_owned);
            match cte {
                Some(name) => out.push((name, n.children.remove(i))),
                None => {
                    take(&mut n.children[i], out);
                    i += 1;
                }
            }
        }
    }
    fn place(n: &mut PlanNode, name: &str, body: &mut Option<PlanNode>) {
        if body.is_none() {
            return;
        }
        if n.operation == "CTE Scan" && n.details.get("CTE Name").map(String::as_str) == Some(name)
        {
            if let Some(b) = body.take() {
                n.children.push(b);
            }
            return;
        }
        for c in &mut n.children {
            place(c, name, body);
        }
    }
    let mut ctes = Vec::new();
    take(root, &mut ctes);
    // Inner CTEs may be read by outer ones: place in reverse so nested bodies land first.
    for (name, body) in ctes.into_iter().rev() {
        let mut body = Some(body);
        place(root, &name, &mut body);
        if let Some(unused) = body {
            // Never scanned (or scanned only by another CTE's body placed later): keep it.
            root.children.push(unused);
        }
    }
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Array(a) => Some(a.iter().filter_map(text).collect::<Vec<_>>().join(", ")),
        _ => None,
    }
}

fn node(v: &Value) -> PlanNode {
    let f = |k: &str| v[k].as_f64();
    let operation = match (v["Node Type"].as_str(), v["Join Type"].as_str()) {
        // DML: "ModifyTable" with "Operation": "Delete" reads as "Delete", like EXPLAIN's text.
        (Some("ModifyTable"), _) => v["Operation"].as_str().unwrap_or("ModifyTable").to_owned(),
        // "Hash Join" with "Join Type": "Left" reads as "Hash Left Join", like EXPLAIN's text.
        (Some(op), Some(join)) if op.ends_with(" Join") && join != "Inner" => {
            format!("{} {join} Join", op.trim_end_matches(" Join"))
        }
        (Some("Nested Loop"), Some(join)) if join != "Inner" => format!("Nested Loop {join} Join"),
        (Some(op), _) => op.to_owned(),
        (None, _) => "?".to_owned(),
    };
    let object = match (v["Relation Name"].as_str(), v["Index Name"].as_str()) {
        (Some(rel), Some(ix)) => Some(format!("{rel}.{ix}")),
        (Some(rel), None) => Some(rel.to_owned()),
        (None, Some(ix)) => Some(ix.to_owned()),
        (None, None) => v["CTE Name"]
            .as_str()
            .or(v["Function Name"].as_str())
            .map(str::to_owned),
    };
    let loops = f("Actual Loops");
    // Never-executed nodes report loops 0 and no meaningful time.
    let total_time_ms = match (f("Actual Total Time"), loops) {
        (Some(t), Some(l)) => Some(t * l),
        _ => None,
    };
    let mut warnings = Vec::new();
    if v["Sort Space Type"] == "Disk" {
        warnings.push(format!(
            "Sort spilled to disk ({} kB)",
            f("Sort Space Used").unwrap_or(0.0)
        ));
    }
    if f("Hash Batches").is_some_and(|b| b > 1.0) {
        warnings.push(format!(
            "Hash used {} batches (spilled to disk)",
            f("Hash Batches").unwrap_or(0.0)
        ));
    }
    if loops == Some(0.0) {
        warnings.push("Never executed".into());
    }
    let details = DETAILS
        .iter()
        .filter_map(|k| Some(((*k).to_owned(), text(&v[*k])?)))
        .chain(v["Sort Key"].as_array().map(|_| {
            (
                "Sort Key".to_owned(),
                text(&v["Sort Key"]).unwrap_or_default(),
            )
        }))
        .chain(v["Group Key"].as_array().map(|_| {
            (
                "Group Key".to_owned(),
                text(&v["Group Key"]).unwrap_or_default(),
            )
        }))
        .collect();
    PlanNode {
        id: 0,
        operation,
        object,
        estimated_rows: f("Plan Rows"),
        actual_rows: f("Actual Rows"),
        loops,
        cost: f("Total Cost"),
        total_time_ms,
        io: Io {
            cache_hits: f("Shared Hit Blocks"),
            disk_reads: f("Shared Read Blocks"),
            temp_written: f("Temp Written Blocks"),
        },
        predicates: PREDICATES
            .iter()
            .filter_map(|k| {
                Some(Predicate {
                    kind: (*k).to_owned(),
                    text: v[*k].as_str()?.to_owned(),
                })
            })
            .collect(),
        warnings,
        details,
        children: v["Plans"]
            .as_array()
            .map(|a| a.iter().map(node).collect())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_estimated_and_errors() {
        let p = parse(
            r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"t","Plan Rows":10,"Total Cost":1.5}}]"#,
            "select * from t",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(p.kind, PlanKind::Estimated);
        assert_eq!(p.root.object.as_deref(), Some("t"));
        assert_eq!(p.root.total_time_ms, None);
        assert!(parse("[]", "x").is_err());
        assert!(parse("not json", "x").is_err());
    }

    #[test]
    fn times_are_multiplied_by_loops_and_joins_are_named() {
        let p = parse(
            r#"[{"Plan":{"Node Type":"Nested Loop","Join Type":"Left","Actual Total Time":10,"Actual Loops":1,"Actual Rows":5,"Plan Rows":5,
                "Plans":[{"Node Type":"Index Scan","Index Name":"ix","Relation Name":"t","Actual Total Time":0.002,"Actual Loops":2000,"Actual Rows":1,"Plan Rows":1}]}}]"#,
            "x",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(p.root.operation, "Nested Loop Left Join");
        let inner = &p.root.children[0];
        assert_eq!(inner.object.as_deref(), Some("t.ix"));
        assert!((inner.total_time_ms.unwrap_or(0.0) - 4.0).abs() < 1e-9);
        assert!((p.root.self_time_ms().unwrap_or(0.0) - 6.0).abs() < 1e-9);
    }
}
