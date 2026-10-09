//! MongoDB `explain` → [`Plan`].
//!
//! Handles the reply shapes MongoDB returns for `find` and `aggregate`:
//!
//! * `{ queryPlanner: { winningPlan }, executionStats: { executionStages } }`: a find, or a
//!   pipeline pushed down entirely into the query layer. The winning plan is a tree of
//!   stages (`COLLSCAN`, `IXSCAN`, `FETCH`, `SORT`, …) linked by `inputStage(s)`.
//! * With the slot-based engine the winning plan is `{ queryPlan, slotBasedPlan }` and the
//!   execution stages are SBE's own; then `queryPlan` is the tree and the totals go on
//!   its root.
//! * `{ stages: [ { $cursor: { … } }, { $group: … }, … ] }`: a pipeline; each later stage
//!   sits on top of the previous one.
//! * Sharded plans list `shards`; each shard's plan becomes a child.
//!
//! MongoDB gives no row estimates or costs, only measured counts (`nReturned`,
//! `docsExamined`, `keysExamined`) and times (`executionTimeMillisEstimate`, which
//! include a stage's inputs).

use serde_json::Value;

use crate::model::{Plan, PlanKind, PlanNode, PlanSource, Predicate, warn};
use crate::{PlanError, Result};

/// Parse the explain reply (as relaxed Extended JSON) for statement `sql`.
pub fn parse(json: &str, sql: &str) -> Result<Plan> {
    let doc: Value =
        serde_json::from_str(json.trim()).map_err(|e| PlanError::Parse(e.to_string()))?;
    let (root, actual, execution_ms) = if let Some(stages) = doc["stages"].as_array() {
        pipeline(stages)?
    } else if doc.get("queryPlanner").is_some() {
        let (n, ms) = query(&doc)?;
        (n, doc.get("executionStats").is_some(), ms)
    } else {
        return Err(PlanError::Parse(
            "no queryPlanner or stages in the explain output".into(),
        ));
    };
    let kind = if actual {
        PlanKind::Actual
    } else {
        PlanKind::Estimated
    };
    let mut plan = Plan::new(PlanSource::MongoDb, kind, sql, root);
    plan.execution_ms = execution_ms;
    plan.planning_ms = num(&doc["queryPlanner"]["optimizationTimeMillis"]);
    if let Some(n) = doc["queryPlanner"]["rejectedPlans"]
        .as_array()
        .map(Vec::len)
        .filter(|n| *n > 0)
    {
        plan.warnings
            .push(format!("{n} other plan(s) considered and rejected"));
    }
    Ok(plan)
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        // Relaxed Extended JSON keeps large longs as `{ "$numberLong": "…" }`.
        Value::Object(o) => o
            .get("$numberLong")
            .or(o.get("$numberDouble"))
            .or(o.get("$numberInt"))
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok()),
        _ => None,
    }
}

/// Compact JSON, cut to `max` characters.
fn compact(v: &Value, max: usize) -> String {
    let s = v.to_string();
    if s.chars().count() <= max {
        s
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// The collection of a `db.collection` namespace.
fn collection(ns: Option<&str>) -> Option<String> {
    let ns = ns?;
    Some(ns.split_once('.').map_or(ns, |(_, c)| c).to_owned())
}

/// A `queryPlanner` (+ `executionStats`) document → its tree and execution time.
fn query(doc: &Value) -> Result<(PlanNode, Option<f64>)> {
    let planner = &doc["queryPlanner"];
    let coll = collection(planner["namespace"].as_str());
    let winning = &planner["winningPlan"];
    let stats = &doc["executionStats"];
    let stages = &stats["executionStages"];
    // Classic engine: the execution stages are the plan with counts. SBE: they are not.
    let classic_stats = stages["stage"]
        .as_str()
        .is_some_and(|s| s.chars().all(|c| c.is_ascii_uppercase() || c == '_'));
    let mut root = if classic_stats {
        stage(stages, coll.as_deref())
    } else if let Some(q) = winning.get("queryPlan") {
        stage(q, coll.as_deref())
    } else if winning.get("stage").is_some() {
        stage(winning, coll.as_deref())
    } else {
        return Err(PlanError::Parse(
            "no winningPlan in the explain output".into(),
        ));
    };
    if stats.is_object() && !classic_stats {
        // Totals only: put them on the root, and the documents read on a lone COLLSCAN.
        root.actual_rows = num(&stats["nReturned"]);
        root.total_time_ms = num(&stats["executionTimeMillis"]);
        if let Some(docs) = num(&stats["totalDocsExamined"]) {
            let mut scans: Vec<&mut PlanNode> = Vec::new();
            collect_mut(&mut root, "COLLSCAN", &mut scans);
            if let [only] = scans.as_mut_slice() {
                only.details.insert("Actual Rows Read".into(), fmt(docs));
            }
        }
    }
    for (k, label) in [
        ("totalDocsExamined", "Total Docs Examined"),
        ("totalKeysExamined", "Total Keys Examined"),
    ] {
        if let Some(v) = num(&stats[k]) {
            root.details.insert(label.into(), fmt(v));
        }
    }
    Ok((root, num(&stats["executionTimeMillis"])))
}

fn collect_mut<'a>(n: &'a mut PlanNode, op: &str, out: &mut Vec<&'a mut PlanNode>) {
    if n.operation == op {
        out.push(n);
        return;
    }
    for c in &mut n.children {
        collect_mut(c, op, out);
    }
}

fn fmt(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v:.2}")
    }
}

/// Field names a filter compares, in order: `{ a: 1, $and: [{ b: { $gt: 2 } }] }` → a, b.
fn filter_fields(v: &Value, out: &mut Vec<String>) {
    let Some(o) = v.as_object() else { return };
    for (k, val) in o {
        if k == "$and" || k == "$or" {
            for item in val.as_array().into_iter().flatten() {
                filter_fields(item, out);
            }
        } else if !k.starts_with('$') && !out.contains(k) {
            out.push(k.clone());
        }
    }
}

/// One classic (or `queryPlan`) stage and its inputs.
fn stage(v: &Value, coll: Option<&str>) -> PlanNode {
    let op = v["stage"].as_str().unwrap_or("?");
    let mut n = PlanNode::op(op);
    let index = v["indexName"].as_str();
    n.object = match (op, coll, index) {
        (_, Some(c), Some(ix)) => Some(format!("{c}.{ix}")),
        ("COLLSCAN" | "FETCH" | "IDHACK" | "EXPRESS_IXSCAN" | "CLUSTERED_IXSCAN", Some(c), _) => {
            Some(c.to_owned())
        }
        _ => None,
    };
    n.actual_rows = num(&v["nReturned"]);
    n.total_time_ms = num(&v["executionTimeMillisEstimate"]);
    let examined = num(&v["docsExamined"]).or(num(&v["keysExamined"]));
    if let Some(e) = examined {
        n.details.insert("Actual Rows Read".into(), fmt(e));
    }
    if let Some(f) = v.get("filter").filter(|f| f.is_object()) {
        n.predicates.push(Predicate {
            kind: "Filter".into(),
            text: compact(f, 500),
        });
        let mut fields = Vec::new();
        filter_fields(f, &mut fields);
        if !fields.is_empty() {
            n.details.insert("Filter Fields".into(), fields.join(", "));
        }
        if let (Some(e), Some(r)) = (examined, n.actual_rows) {
            n.details
                .insert("Rows Removed by Filter".into(), fmt((e - r).max(0.0)));
        }
    }
    if let Some(b) = v.get("indexBounds").filter(|b| b.is_object()) {
        n.predicates.push(Predicate {
            kind: "Index Bounds".into(),
            text: compact(b, 500),
        });
    }
    for (k, label) in [
        ("keyPattern", "Key Pattern"),
        ("direction", "Direction"),
        ("sortPattern", "Sort Pattern"),
        ("limitAmount", "Limit"),
        ("skipAmount", "Skip"),
        ("transformBy", "Projection"),
        ("isMultiKey", "Multi-key"),
        ("works", "Works"),
        ("docsExamined", "Docs Examined"),
        ("keysExamined", "Keys Examined"),
        ("totalDataSizeSorted", "Bytes Sorted"),
        ("memUsage", "Memory Used"),
    ] {
        match &v[k] {
            Value::Null => {}
            Value::String(s) => {
                n.details.insert(label.into(), s.clone());
            }
            other => {
                n.details.insert(label.into(), compact(other, 200));
            }
        }
    }
    if op == "SORT" {
        n.warnings
            .push(format!("{} (blocking sort in memory)", warn::SORT));
    }
    if v["usedDisk"] == true {
        n.warnings.push("Sort spilled to disk".into());
    }
    if op == "LIMIT" {
        n.operation = "Limit".into();
        n.details.insert("Stage".into(), "LIMIT".into());
    }
    let mut inputs: Vec<&Value> = Vec::new();
    for k in [
        "inputStage",
        "outerStage",
        "innerStage",
        "thenStage",
        "elseStage",
    ] {
        if let Some(i) = v.get(k).filter(|i| i.is_object()) {
            inputs.push(i);
        }
    }
    inputs.extend(v["inputStages"].as_array().into_iter().flatten());
    for i in inputs {
        n.children.push(stage(i, coll));
    }
    for shard in v["shards"].as_array().into_iter().flatten() {
        let plan = shard
            .get("executionStages")
            .or(shard.get("winningPlan"))
            .map(|p| p.get("queryPlan").unwrap_or(p));
        if let Some(p) = plan {
            let mut s = stage(p, coll);
            if let Some(name) = shard["shardName"].as_str() {
                s.details.insert("Shard".into(), name.to_owned());
            }
            n.children.push(s);
        }
    }
    n
}

/// `{ stages: [ { $cursor }, { $group }, … ] }`: each stage on top of the previous.
fn pipeline(stages: &[Value]) -> Result<(PlanNode, bool, Option<f64>)> {
    let mut acc: Option<PlanNode> = None;
    let mut actual = false;
    let mut execution_ms = None;
    for s in stages {
        let Some((name, spec)) = s
            .as_object()
            .and_then(|o| o.iter().find(|(k, _)| k.starts_with('$')))
        else {
            continue;
        };
        let mut n = if name == "$cursor" {
            let (mut n, ms) = query(spec)?;
            actual |= spec.get("executionStats").is_some();
            execution_ms = ms;
            // The cursor stage's own count and time cover the query layer.
            if let Some(r) = num(&s["nReturned"]) {
                n.actual_rows = Some(r);
            }
            if let Some(t) = num(&s["executionTimeMillisEstimate"]) {
                n.total_time_ms = Some(t);
            }
            n
        } else {
            let mut n = PlanNode::op(name.as_str());
            n.actual_rows = num(&s["nReturned"]);
            n.total_time_ms = num(&s["executionTimeMillisEstimate"]);
            actual |= n.actual_rows.is_some();
            match name.as_str() {
                "$match" => n.predicates.push(Predicate {
                    kind: "Filter".into(),
                    text: compact(spec, 500),
                }),
                "$sort" => {
                    n.warnings
                        .push(format!("{} (blocking sort in memory)", warn::SORT));
                    if let Some(k) = spec.get("sortKey") {
                        n.details.insert("Sort Pattern".into(), compact(k, 200));
                    }
                    if let Some(l) = num(&spec["limit"]) {
                        n.details.insert("Limit".into(), fmt(l));
                    }
                }
                "$lookup" | "$graphLookup" | "$unionWith" => {
                    n.object = spec["from"]
                        .as_str()
                        .or(spec["coll"].as_str())
                        .map(str::to_owned);
                    n.details.insert("Stage".into(), compact(spec, 300));
                }
                "$limit" => {
                    n.operation = "Limit".into();
                    n.details.insert("Stage".into(), "$limit".into());
                    n.details.insert("Limit".into(), compact(spec, 50));
                }
                _ => {
                    n.details.insert("Stage".into(), compact(spec, 300));
                }
            }
            if s["usedDisk"] == true {
                n.warnings.push("Stage spilled to disk".into());
            }
            n
        };
        if let Some(prev) = acc.take() {
            n.children.insert(0, prev);
        }
        acc = Some(n);
    }
    let root = acc.ok_or_else(|| PlanError::Parse("the pipeline explain has no stages".into()))?;
    if execution_ms.is_none() && actual {
        execution_ms = root.total_time_ms;
    }
    Ok((root, actual, execution_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_based_plans_use_the_query_plan_with_totals_on_top() {
        let json = r#"{
          "explainVersion": "2",
          "queryPlanner": { "namespace": "shop.orders", "winningPlan": {
            "queryPlan": { "stage": "GROUP", "planNodeId": 2,
              "inputStage": { "stage": "COLLSCAN", "planNodeId": 1,
                "filter": { "status": { "$eq": "A" } }, "direction": "forward" } },
            "slotBasedPlan": { "slots": "…", "stages": "…" } }, "rejectedPlans": [] },
          "executionStats": { "nReturned": 3, "executionTimeMillis": 12,
            "totalKeysExamined": 0, "totalDocsExamined": 20000,
            "executionStages": { "stage": "project", "nReturned": 3 } } }"#;
        let plan = parse(json, "db.orders.aggregate([…])").unwrap();
        assert_eq!(plan.kind, PlanKind::Actual);
        assert_eq!(plan.execution_ms, Some(12.0));
        assert_eq!(plan.root.operation, "GROUP");
        assert_eq!(plan.root.actual_rows, Some(3.0));
        let scan = &plan.root.children[0];
        assert_eq!(scan.object.as_deref(), Some("orders"));
        assert_eq!(
            scan.details.get("Actual Rows Read").map(String::as_str),
            Some("20000")
        );
        assert_eq!(
            scan.details.get("Filter Fields").map(String::as_str),
            Some("status")
        );
    }

    #[test]
    fn sharded_plans_list_each_shard() {
        let json = r#"{ "queryPlanner": { "namespace": "shop.orders", "winningPlan": {
            "stage": "SHARD_MERGE", "shards": [
              { "shardName": "s0", "winningPlan": { "stage": "COLLSCAN" } },
              { "shardName": "s1", "winningPlan": { "queryPlan": { "stage": "IXSCAN",
                "indexName": "status_1" } } } ] } } }"#;
        let plan = parse(json, "db.orders.find()").unwrap();
        assert_eq!(plan.kind, PlanKind::Estimated);
        let ops: Vec<String> = plan
            .nodes()
            .iter()
            .map(|n| format!("{} {}", n.operation, n.object.as_deref().unwrap_or("-")))
            .collect();
        assert_eq!(
            ops,
            ["SHARD_MERGE -", "COLLSCAN orders", "IXSCAN orders.status_1"]
        );
        assert!(parse("{}", "x").is_err());
        assert_eq!(num(&serde_json::json!({ "$numberLong": "42" })), Some(42.0));
    }
}
