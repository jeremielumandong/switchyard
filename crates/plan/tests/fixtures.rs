//! Plans captured from real servers (`tests/fixtures/`), parsed and checked against insta
//! snapshots, and every findings rule against a positive and a negative fixture.
//!
//! PostgreSQL fixtures were captured from the docker seed (`docker/postgres`) with
//! `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)`; SQL Server fixtures by `tests/capture.rs`
//! with `SWITCHYARD_WRITE_FIXTURES=1`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;

use switchyard_plan::{Plan, PlanNode, Rule, Thresholds, analyze};

fn fixture(path: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{path}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn pg(name: &str) -> Plan {
    switchyard_plan::pg::parse(&fixture(&format!("pg/{name}.json")), name).unwrap()
}

fn mssql(name: &str) -> Option<Plan> {
    let path = format!(
        "{}/tests/fixtures/mssql/{name}.xml",
        env!("CARGO_MANIFEST_DIR")
    );
    let xml = std::fs::read_to_string(path).ok()?;
    Some(switchyard_plan::mssql::parse(&xml, name).unwrap())
}

/// A stable, readable rendering: one line per node, then the findings.
fn render(plan: &Plan) -> String {
    fn walk(plan: &Plan, n: &PlanNode, depth: usize, out: &mut String) {
        let num = |v: Option<f64>| v.map(|v| format!("{v:.0}")).unwrap_or_else(|| "-".into());
        let _ = write!(
            out,
            "{}#{} {}{} est={} act={} loops={}",
            "  ".repeat(depth),
            n.id,
            n.operation,
            n.object
                .as_deref()
                .map(|o| format!(" on {o}"))
                .unwrap_or_default(),
            num(n.estimated_rows),
            num(n.actual_rows),
            num(n.loops),
        );
        if let Some(t) = n.self_time_ms() {
            let _ = write!(out, " self={t:.1}ms ({:.0}%)", plan.weight(n) * 100.0);
        }
        for p in &n.predicates {
            let _ = write!(out, "\n{}  {}: {}", "  ".repeat(depth), p.kind, p.text);
        }
        for w in &n.warnings {
            let _ = write!(out, "\n{}  ! {w}", "  ".repeat(depth));
        }
        out.push('\n');
        for c in &n.children {
            walk(plan, c, depth + 1, out);
        }
    }
    let mut out = format!("{:?} {:?}\n", plan.source, plan.kind);
    walk(plan, &plan.root, 0, &mut out);
    for m in &plan.missing_indexes {
        let _ = writeln!(out, "missing: {}", m.create_statement());
    }
    out.push_str("findings:\n");
    for f in analyze(plan, &Thresholds::default()) {
        let _ = writeln!(
            out,
            "  {:?} {:?} node={:?}: {} | {} | {}",
            f.severity,
            f.rule,
            f.node_id,
            f.title,
            f.detail,
            f.suggestion.unwrap_or_default()
        );
    }
    out
}

macro_rules! pg_snapshot {
    ($($name:ident),*) => {$(
        #[test]
        fn $name() {
            insta::assert_snapshot!(concat!("pg_", stringify!($name)), render(&pg(stringify!($name))));
        }
    )*};
}

pg_snapshot!(
    seq_scan_filter,
    index_scan,
    hash_join,
    sort_spill,
    aggregate,
    cte,
    nested_loop,
    bad_estimate,
    filter_removes,
    estimated_join,
    implicit_cast
);

#[test]
fn mssql_snapshots() {
    let dir = format!("{}/tests/fixtures/mssql", env!("CARGO_MANIFEST_DIR"));
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".xml").map(str::to_owned)
        })
        .collect();
    names.sort();
    for name in names {
        let plan = mssql(&name).unwrap();
        insta::assert_snapshot!(format!("mssql_{name}"), render(&plan));
    }
}

/// Rules found for a plan, with the node they point at.
fn rules(plan: &Plan) -> Vec<(Rule, Option<u32>)> {
    analyze(plan, &Thresholds::default())
        .into_iter()
        .map(|f| (f.rule, f.node_id))
        .collect()
}

fn has(plan: &Plan, rule: Rule) -> bool {
    rules(plan).iter().any(|(r, _)| *r == rule)
}

#[test]
fn every_rule_has_a_positive_and_a_negative_fixture() {
    let index = pg("index_scan");
    // Full scan: 1M-row seq scan vs. a primary-key lookup.
    assert!(has(&pg("seq_scan_filter"), Rule::FullScan));
    assert!(!has(&index, Rule::FullScan));
    // Bad estimate: 5,000 estimated vs 1,000,000 actual; a filter the planner estimates well.
    assert!(has(&pg("bad_estimate"), Rule::BadEstimate));
    assert!(!has(&pg("filter_removes"), Rule::BadEstimate));
    // Rows removed: 987,500 of 1,000,000 discarded vs. an index lookup.
    assert!(has(&pg("filter_removes"), Rule::RowsRemovedByFilter));
    assert!(!has(&index, Rule::RowsRemovedByFilter));
    // Spill: external merge sort with work_mem = 64kB vs. an in-memory hash aggregate.
    assert!(has(&pg("sort_spill"), Rule::Spill));
    assert!(!has(&pg("aggregate"), Rule::Spill));
    // Implicit conversion: `id::text = '42'` vs. a typed comparison.
    assert!(has(&pg("implicit_cast"), Rule::ImplicitConversion));
    assert!(!has(&index, Rule::ImplicitConversion));
    // Missing index (PostgreSQL: a suggestion on the full scan's filter columns).
    let seq = analyze(&pg("seq_scan_filter"), &Thresholds::default());
    let full = seq.iter().find(|f| f.rule == Rule::FullScan).unwrap();
    assert_eq!(
        full.suggestion.as_deref(),
        Some("CREATE INDEX ON orders (status, total);")
    );
    // Expensive nested loop: the captured loop probes an index (cheap); a hand-made one
    // that scans its inner side 5,000 times is flagged.
    assert!(!has(&pg("nested_loop"), Rule::ExpensiveNestedLoop));
    assert!(has(
        &nested_loop_scanning_inner(),
        Rule::ExpensiveNestedLoop
    ));
    // Key lookup and SQL Server's missing index: positives from showplan XML, and the
    // PostgreSQL plans (which never have them) as negatives.
    let lookup = key_lookup_plan();
    assert!(has(&lookup, Rule::KeyLookup));
    assert!(has(&lookup, Rule::MissingIndex));
    assert!(!has(&pg("hash_join"), Rule::KeyLookup));
    assert!(!has(&pg("hash_join"), Rule::MissingIndex));
}

fn nested_loop_scanning_inner() -> Plan {
    let json = r#"[{"Plan":{"Node Type":"Nested Loop","Join Type":"Inner","Plan Rows":10,"Actual Rows":10,"Actual Loops":1,"Actual Total Time":900,
      "Plans":[
        {"Node Type":"Seq Scan","Relation Name":"customers","Plan Rows":5000,"Actual Rows":5000,"Actual Loops":1,"Actual Total Time":5},
        {"Node Type":"Seq Scan","Relation Name":"products","Plan Rows":1,"Actual Rows":0,"Actual Loops":5000,"Actual Total Time":0.17}
      ]},"Execution Time":901}]"#;
    switchyard_plan::pg::parse(json, "nl").unwrap()
}

fn key_lookup_plan() -> Plan {
    let xml = r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan"><BatchSequence><Batch><Statements>
<StmtSimple StatementText="select * from orders where customer_id = 7"><QueryPlan>
<MissingIndexes><MissingIndexGroup Impact="72.1"><MissingIndex Schema="[dbo]" Table="[orders]">
<ColumnGroup Usage="EQUALITY"><Column Name="[customer_id]"/></ColumnGroup>
<ColumnGroup Usage="INCLUDE"><Column Name="[status]"/><Column Name="[total]"/></ColumnGroup>
</MissingIndex></MissingIndexGroup></MissingIndexes>
<RelOp NodeId="0" PhysicalOp="Nested Loops" LogicalOp="Inner Join" EstimateRows="12" EstimatedTotalSubtreeCost="0.5">
<RunTimeInformation><RunTimeCountersPerThread ActualRows="900" ActualExecutions="1" ActualElapsedms="40"/></RunTimeInformation>
<NestedLoops>
<RelOp NodeId="1" PhysicalOp="Index Seek" LogicalOp="Index Seek" EstimateRows="12" EstimatedTotalSubtreeCost="0.01">
<RunTimeInformation><RunTimeCountersPerThread ActualRows="900" ActualExecutions="1" ActualElapsedms="2"/></RunTimeInformation>
<IndexScan><Object Schema="[dbo]" Table="[orders]" Index="[ix_orders_customer]"/></IndexScan></RelOp>
<RelOp NodeId="2" PhysicalOp="Key Lookup" LogicalOp="Key Lookup" EstimateRows="1" EstimatedTotalSubtreeCost="0.4">
<RunTimeInformation><RunTimeCountersPerThread ActualRows="900" ActualExecutions="900" ActualElapsedms="35"/></RunTimeInformation>
<IndexScan Lookup="1"><Object Schema="[dbo]" Table="[orders]" Index="[PK_orders]"/></IndexScan></RelOp>
</NestedLoops></RelOp></QueryPlan></StmtSimple></Statements></Batch></BatchSequence></ShowPlanXML>"#;
    switchyard_plan::mssql::parse(xml, "lookup").unwrap()
}

#[test]
fn thresholds_are_configurable() {
    let plan = pg("seq_scan_filter");
    let strict = Thresholds {
        full_scan_rows: 10_000_000.0,
        ..Thresholds::default()
    };
    assert!(has(&plan, Rule::FullScan));
    assert!(
        !analyze(&plan, &strict)
            .iter()
            .any(|f| f.rule == Rule::FullScan)
    );
}

#[test]
fn findings_are_ranked_and_point_at_real_nodes() {
    for name in ["seq_scan_filter", "sort_spill", "hash_join", "cte"] {
        let plan = pg(name);
        let found = analyze(&plan, &Thresholds::default());
        assert!(found.windows(2).all(|w| w[0].score >= w[1].score), "{name}");
        for f in &found {
            if let Some(id) = f.node_id {
                assert!(plan.node(id).is_some(), "{name}: node {id}");
            }
        }
    }
}

/// Every fixture in `tests/fixtures/<dir>` with extension `ext`, sorted by name.
fn fixture_names(dir: &str, ext: &str) -> Vec<String> {
    let path = format!("{}/tests/fixtures/{dir}", env!("CARGO_MANIFEST_DIR"));
    let mut names: Vec<String> = std::fs::read_dir(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(ext).map(str::to_owned)
        })
        .collect();
    names.sort();
    names
}

/// MySQL 8.4 (docker `mysql`): `<name>.json` from `EXPLAIN FORMAT=JSON`, `<name>.txt` from
/// `EXPLAIN ANALYZE`.
fn mysql_json(name: &str) -> Plan {
    switchyard_plan::mysql::parse_json(&fixture(&format!("mysql/{name}.json")), name).unwrap()
}

fn mysql_tree(name: &str) -> Plan {
    switchyard_plan::mysql::parse_tree(&fixture(&format!("mysql/{name}.txt")), name).unwrap()
}

/// SQLite 3.45 `EXPLAIN QUERY PLAN` rows as `id|parent|detail` (first line: the query).
fn sqlite(name: &str) -> Plan {
    let text = fixture(&format!("sqlite/{name}.txt"));
    let rows: Vec<switchyard_plan::sqlite::Row> = text
        .lines()
        .filter(|l| !l.starts_with("--") && !l.is_empty())
        .map(|l| {
            let mut parts = l.splitn(3, '|');
            switchyard_plan::sqlite::Row {
                id: parts.next().unwrap().parse().unwrap(),
                parent: parts.next().unwrap().parse().unwrap(),
                detail: parts.next().unwrap().to_owned(),
            }
        })
        .collect();
    switchyard_plan::sqlite::parse(&rows, name).unwrap()
}

/// MongoDB 7 (docker `mongo`, 20,000 documents): `<case>_est` with `queryPlanner`,
/// `<case>_act` with `executionStats`.
fn mongo(name: &str) -> Plan {
    switchyard_plan::mongo::parse(&fixture(&format!("mongo/{name}.json")), name).unwrap()
}

#[test]
fn mysql_snapshots() {
    for name in fixture_names("mysql", ".json") {
        insta::assert_snapshot!(format!("mysql_{name}_est"), render(&mysql_json(&name)));
    }
    for name in fixture_names("mysql", ".txt") {
        insta::assert_snapshot!(format!("mysql_{name}_act"), render(&mysql_tree(&name)));
    }
}

#[test]
fn sqlite_snapshots() {
    for name in fixture_names("sqlite", ".txt") {
        insta::assert_snapshot!(format!("sqlite_{name}"), render(&sqlite(&name)));
    }
}

#[test]
fn mongo_snapshots() {
    for name in fixture_names("mongo", ".json") {
        insta::assert_snapshot!(format!("mongo_{name}"), render(&mongo(&name)));
    }
}

#[test]
fn rules_apply_to_mysql_sqlite_and_mongodb_plans() {
    use switchyard_plan::{PlanKind, PlanSource, Severity};
    // MySQL: a 20,000-row table scan, estimated and measured; the measured filter keeps 1.
    let est = mysql_json("full_scan");
    assert_eq!(
        (est.source, est.kind),
        (PlanSource::MySql, PlanKind::Estimated)
    );
    assert!(has(&est, Rule::FullScan));
    let act = mysql_tree("full_scan");
    assert_eq!(act.kind, PlanKind::Actual);
    assert!(has(&act, Rule::FullScan));
    assert!(has(&act, Rule::RowsRemovedByFilter));
    assert!(!has(&mysql_tree("index_lookup"), Rule::FullScan));
    // Filesort and temporary tables over enough rows; none on a lookup.
    assert!(has(&mysql_json("derived"), Rule::TempStructure));
    assert!(has(&mysql_tree("derived"), Rule::TempStructure));
    assert!(!has(&mysql_json("index_lookup"), Rule::TempStructure));
    // The scan of an internal temporary table is not a full-scan finding.
    let derived = mysql_tree("derived");
    let found = rules(&derived);
    for n in derived.nodes() {
        if n.object.as_deref() == Some("<temporary>") {
            assert!(!found.contains(&(Rule::FullScan, Some(n.id))));
        }
    }
    // The DELETE's estimated plan: the write over its scan.
    assert_eq!(mysql_json("dml_delete").root.operation, "Delete");

    // SQLite has no row counts: every full scan and temp B-tree is flagged, at Low.
    let scan = sqlite("full_scan");
    let found = analyze(&scan, &Thresholds::default());
    assert!(found.iter().any(|f| f.rule == Rule::FullScan));
    assert!(found.iter().all(|f| f.severity == Severity::Low));
    assert!(!has(&sqlite("index_search"), Rule::FullScan));
    assert!(has(&sqlite("join_sort"), Rule::TempStructure));
    assert!(!has(&sqlite("index_search"), Rule::TempStructure));

    // MongoDB: COLLSCAN over 20,000 documents with an index suggestion; an IXSCAN is fine.
    let coll = mongo("collscan_act");
    assert_eq!(
        (coll.source, coll.kind),
        (PlanSource::MongoDb, PlanKind::Actual)
    );
    let f = analyze(&coll, &Thresholds::default());
    let full = f.iter().find(|f| f.rule == Rule::FullScan).unwrap();
    assert_eq!(
        full.suggestion.as_deref(),
        Some("db.orders.createIndex({ total: 1 })")
    );
    assert!(has(&mongo("collscan_est"), Rule::FullScan));
    assert!(!has(&mongo("ixscan_fetch_act"), Rule::FullScan));
    assert!(has(&mongo("sort_limit_act"), Rule::TempStructure));
    assert!(!has(&mongo("covered_sort_act"), Rule::TempStructure));
}
