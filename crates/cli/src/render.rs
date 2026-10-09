//! Plain-text rendering of plans and workloads for the terminal and for agents.

use std::fmt::Write as _;

use switchyard_core::db::ObjectDetail;
use switchyard_core::plan::access::Workload;
use switchyard_core::plan::{Finding, Plan, PlanKind, PlanNode, Severity};

fn num(v: f64) -> String {
    if v >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if v >= 1e4 {
        format!("{:.0}k", v / 1e3)
    } else if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

fn ms(v: f64) -> String {
    if v >= 1000.0 {
        format!("{:.2} s", v / 1000.0)
    } else {
        format!("{v:.2} ms")
    }
}

fn node_line(n: &PlanNode, plan: &Plan) -> String {
    let mut s = n.operation.clone();
    if let Some(o) = &n.object {
        let _ = write!(s, " on {o}");
    }
    let mut facts = Vec::new();
    match (n.estimated_rows, n.actual_rows) {
        (Some(e), Some(a)) => facts.push(format!("rows {} (est {})", num(a), num(e))),
        (Some(e), None) => facts.push(format!("rows ~{}", num(e))),
        (None, Some(a)) => facts.push(format!("rows {}", num(a))),
        (None, None) => {}
    }
    if let Some(l) = n.loops.filter(|l| *l > 1.0) {
        facts.push(format!("loops {}", num(l)));
    }
    if let Some(t) = n.self_time_ms() {
        facts.push(format!("self {}", ms(t)));
    } else if let Some(share) = plan.self_cost_share(n) {
        facts.push(format!("{:.0}% of cost", share * 100.0));
    }
    for p in &n.predicates {
        facts.push(format!("{}: {}", p.kind, p.text));
    }
    if !facts.is_empty() {
        let _ = write!(s, "  [{}]", facts.join("; "));
    }
    s
}

fn tree(n: &PlanNode, plan: &Plan, depth: usize, out: &mut String) {
    let _ = writeln!(out, "{}{}", "  ".repeat(depth), node_line(n, plan));
    for c in &n.children {
        tree(c, plan, depth + 1, out);
    }
}

/// A plan as an indented tree, followed by its findings.
pub fn plan_text(plan: &Plan, findings: &[Finding]) -> String {
    let mut out = String::new();
    let kind = match plan.kind {
        PlanKind::Actual => "Actual plan",
        PlanKind::Estimated => "Estimated plan",
    };
    let mut head = vec![kind.to_owned()];
    if let Some(t) = plan.total_time_ms() {
        head.push(ms(t));
    }
    if let Some(c) = plan.root.cost {
        head.push(format!("cost {}", num(c)));
    }
    let _ = writeln!(out, "{}", head.join(" · "));
    tree(&plan.root, plan, 0, &mut out);
    for w in &plan.warnings {
        let _ = writeln!(out, "warning: {w}");
    }
    if findings.is_empty() {
        let _ = writeln!(out, "\nNo hotspots.");
    } else {
        let _ = writeln!(out, "\nHotspots:");
        for f in findings {
            let sev = match f.severity {
                Severity::High => "HIGH",
                Severity::Medium => "MEDIUM",
                Severity::Low => "LOW",
            };
            let _ = writeln!(out, "- [{sev}] {}: {}", f.title, f.detail);
            if let Some(s) = &f.suggestion {
                let _ = writeln!(out, "  suggestion: {s}");
            }
        }
    }
    for m in &plan.missing_indexes {
        let _ = writeln!(out, "missing index: {}", m.create_statement());
    }
    out
}

/// A workload summary: hints, top statements, tables, indexes, missing indexes.
pub fn workload_text(w: &Workload) -> String {
    let mut out = String::new();
    for h in &w.hints {
        let _ = writeln!(out, "note: {}", h.message);
        if let Some(fix) = &h.fix {
            for l in fix.lines() {
                let _ = writeln!(out, "  {l}");
            }
        }
    }
    let _ = writeln!(out, "\nTop statements by total time:");
    if w.statements.is_empty() {
        let _ = writeln!(out, "  (none)");
    }
    for q in w.statements.iter().take(20) {
        let text = q.query.split_whitespace().collect::<Vec<_>>().join(" ");
        let text: String = text.chars().take(160).collect();
        let _ = writeln!(
            out,
            "  {} total · {} calls · {} mean · {text}",
            ms(q.total_ms),
            num(q.calls),
            ms(q.mean_ms)
        );
    }
    let _ = writeln!(out, "\nTables (most rows read by full scans first):");
    for t in w.tables.iter().take(20) {
        let flag = if t.mostly_sequential() {
            "  ← mostly full scans"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "  {}.{}: full scans {}, rows scanned {}, index scans {}, rows {}{flag}",
            t.schema,
            t.name,
            t.seq_scans.map_or("–".into(), num),
            t.seq_rows_read.map_or("–".into(), num),
            t.index_scans.map_or("–".into(), num),
            t.rows.map_or("–".into(), num),
        );
    }
    let unused: Vec<_> = w.indexes.iter().filter(|i| i.unused()).collect();
    if !unused.is_empty() {
        let _ = writeln!(out, "\nUnused indexes:");
        for i in unused {
            let _ = writeln!(out, "  {}.{} on {}", i.schema, i.name, i.table);
        }
    }
    if !w.missing_indexes.is_empty() {
        let _ = writeln!(out, "\nMissing indexes reported by the engine:");
        for m in &w.missing_indexes {
            let impact = m
                .impact
                .map_or(String::new(), |i| format!(" (impact {i:.0}%)"));
            let _ = writeln!(out, "  {}{impact}", m.create_statement());
        }
    }
    out
}

/// A table or view: columns, indexes, constraints, foreign keys.
pub fn detail_text(d: &ObjectDetail) -> String {
    let mut out = String::new();
    let o = &d.object;
    let rows = o
        .estimated_rows
        .map_or(String::new(), |r| format!(" (~{r} rows)"));
    let _ = writeln!(out, "{}.{}{rows}", o.schema, o.name);
    let _ = writeln!(out, "\nColumns:");
    for c in &d.columns {
        let mut s = format!("  {} {}", c.name, c.data_type);
        if !c.nullable {
            s.push_str(" NOT NULL");
        }
        if let Some(def) = &c.default {
            let _ = write!(s, " DEFAULT {def}");
        }
        if c.is_primary_key {
            s.push_str(" (primary key)");
        }
        let _ = writeln!(out, "{s}");
    }
    if !d.indexes.is_empty() {
        let _ = writeln!(out, "\nIndexes:");
        for i in &d.indexes {
            let _ = writeln!(out, "  {}: {}", i.name, i.definition);
        }
    }
    if !d.constraints.is_empty() {
        let _ = writeln!(out, "\nConstraints:");
        for c in &d.constraints {
            let _ = writeln!(out, "  {} {}: {}", c.kind, c.name, c.definition);
        }
    }
    if !d.foreign_keys.is_empty() {
        let _ = writeln!(out, "\nForeign keys:");
        for f in &d.foreign_keys {
            let _ = writeln!(
                out,
                "  {} ({}) → {} ({})",
                f.name,
                f.columns.join(", "),
                f.references,
                f.referenced_columns.join(", ")
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::db::Engine;
    use switchyard_core::db::catalog::{
        ColumnInfo, ConstraintInfo, ForeignKeyInfo, IndexInfo, ObjectInfo,
    };
    use switchyard_core::plan::access::{Hint, IndexUsage, Source, StatementStat, TableUsage};
    use switchyard_core::plan::{MissingIndex, PlanSource, Predicate, Rule};

    fn node(op: &str, object: Option<&str>, children: Vec<PlanNode>) -> PlanNode {
        PlanNode {
            object: object.map(str::to_owned),
            children,
            ..PlanNode::op(op)
        }
    }

    fn workload() -> Workload {
        Workload {
            engine: Engine::Postgres,
            since_ms: None,
            tables: Vec::new(),
            indexes: Vec::new(),
            statements: Vec::new(),
            missing_indexes: Vec::new(),
            hints: Vec::new(),
            hypopg: None,
        }
    }

    #[test]
    fn numbers_and_times_are_compact() {
        assert_eq!(num(0.5), "0.50");
        assert_eq!(num(1000.0), "1000");
        assert_eq!(num(25_000.0), "25k");
        assert_eq!(num(2_500_000.0), "2.5M");
        assert_eq!(ms(0.25), "0.25 ms");
        assert_eq!(ms(999.994), "999.99 ms");
        assert_eq!(ms(1500.0), "1.50 s");
    }

    #[test]
    fn estimated_plan_shows_cost_shares_predicates_and_findings() {
        let mut scan = node("Seq Scan", Some("orders"), vec![]);
        scan.cost = Some(60.0);
        scan.estimated_rows = Some(25_000.0);
        scan.predicates.push(Predicate {
            kind: "Filter".into(),
            text: "(status = 'new'::text)".into(),
        });
        let mut seek = node("Index Scan", Some("customers.customers_pkey"), vec![]);
        seek.cost = Some(10.0);
        seek.estimated_rows = Some(1.0);
        let mut root = node("Hash Join", None, vec![scan, seek]);
        root.cost = Some(100.0);
        let plan = Plan::new(PlanSource::Postgres, PlanKind::Estimated, "select", root);
        let findings = [Finding {
            rule: Rule::FullScan,
            node_id: Some(1),
            severity: Severity::High,
            score: 1.0,
            title: "Seq Scan on orders".into(),
            detail: "Reads 25,000 rows.".into(),
            suggestion: Some("CREATE INDEX ON orders (status);".into()),
        }];
        assert_eq!(
            plan_text(&plan, &findings),
            "Estimated plan · cost 100\n\
             Hash Join  [30% of cost]\n\
             \x20 Seq Scan on orders  [rows ~25k; 60% of cost; Filter: (status = 'new'::text)]\n\
             \x20 Index Scan on customers.customers_pkey  [rows ~1; 10% of cost]\n\
             \n\
             Hotspots:\n\
             - [HIGH] Seq Scan on orders: Reads 25,000 rows.\n\
             \x20 suggestion: CREATE INDEX ON orders (status);\n"
        );
    }

    #[test]
    fn actual_plan_shows_rows_loops_self_time_and_warnings() {
        let mut scan = node("Seq Scan", Some("events"), vec![]);
        scan.total_time_ms = Some(1200.0);
        scan.actual_rows = Some(2_500_000.0);
        scan.estimated_rows = Some(1000.0);
        scan.loops = Some(3.0);
        let mut root = node("Limit", None, vec![scan]);
        root.total_time_ms = Some(1500.0);
        root.actual_rows = Some(10.0);
        root.estimated_rows = Some(10.0);
        // A single loop is not worth printing.
        root.loops = Some(1.0);
        let mut plan = Plan::new(PlanSource::Postgres, PlanKind::Actual, "select", root);
        plan.warnings.push("statement timed out once".into());
        assert_eq!(
            plan_text(&plan, &[]),
            "Actual plan · 1.50 s\n\
             Limit  [rows 10 (est 10); self 300.00 ms]\n\
             \x20 Seq Scan on events  [rows 2.5M (est 1000); loops 3; self 1.20 s]\n\
             warning: statement timed out once\n\
             \n\
             No hotspots.\n"
        );
    }

    #[test]
    fn plan_lists_engine_missing_indexes_after_findings() {
        let mut plan = Plan::new(
            PlanSource::SqlServer,
            PlanKind::Estimated,
            "select",
            PlanNode::op("Clustered Index Scan"),
        );
        let m = MissingIndex {
            impact: Some(80.0),
            table: "[dbo].[orders]".into(),
            equality: vec!["[status]".into()],
            inequality: Vec::new(),
            include: Vec::new(),
        };
        plan.missing_indexes.push(m.clone());
        let text = plan_text(&plan, &[]);
        assert!(
            text.ends_with(&format!(
                "No hotspots.\nmissing index: {}\n",
                m.create_statement()
            )),
            "{text}"
        );
        // A node without rows, loops, time or cost has no bracket.
        assert!(text.contains("\nClustered Index Scan\n"), "{text}");
    }

    #[test]
    fn workload_lists_hints_tables_unused_and_missing_indexes() {
        let mut w = workload();
        w.hints.push(Hint {
            source: Source::Statements,
            message: "pg_stat_statements is not installed".into(),
            fix: Some("CREATE EXTENSION pg_stat_statements;\n-- then reconnect".into()),
        });
        w.tables.push(TableUsage {
            schema: "public".into(),
            name: "orders".into(),
            seq_scans: Some(12.0),
            seq_rows_read: Some(1_200_000.0),
            index_scans: Some(2.0),
            rows: Some(50_000.0),
            ..TableUsage::default()
        });
        w.tables.push(TableUsage {
            schema: "public".into(),
            name: "tags".into(),
            ..TableUsage::default()
        });
        w.indexes.push(IndexUsage {
            schema: "public".into(),
            table: "orders".into(),
            name: "orders_note_idx".into(),
            scans: Some(0.0),
            ..IndexUsage::default()
        });
        // Unscanned but backing a key: not reported as unused.
        w.indexes.push(IndexUsage {
            schema: "public".into(),
            table: "orders".into(),
            name: "orders_pkey".into(),
            scans: Some(0.0),
            primary: true,
            ..IndexUsage::default()
        });
        let m = MissingIndex {
            impact: Some(87.4),
            table: "dbo.orders".into(),
            equality: vec!["status".into()],
            inequality: Vec::new(),
            include: vec!["total".into()],
        };
        w.missing_indexes.push(m.clone());
        assert_eq!(
            workload_text(&w),
            format!(
                "note: pg_stat_statements is not installed\n\
                 \x20 CREATE EXTENSION pg_stat_statements;\n\
                 \x20 -- then reconnect\n\
                 \n\
                 Top statements by total time:\n\
                 \x20 (none)\n\
                 \n\
                 Tables (most rows read by full scans first):\n\
                 \x20 public.orders: full scans 12, rows scanned 1.2M, index scans 2, rows 50k  ← mostly full scans\n\
                 \x20 public.tags: full scans –, rows scanned –, index scans –, rows –\n\
                 \n\
                 Unused indexes:\n\
                 \x20 public.orders_note_idx on orders\n\
                 \n\
                 Missing indexes reported by the engine:\n\
                 \x20 {} (impact 87%)\n",
                m.create_statement()
            )
        );
    }

    #[test]
    fn workload_statements_are_one_line_truncated_and_capped() {
        let mut w = workload();
        w.statements.push(StatementStat {
            id: "1".into(),
            query: "select *\n   from orders\n\twhere id = $1".into(),
            calls: 3.0,
            total_ms: 2500.0,
            mean_ms: 833.333,
            rows: None,
            pages: None,
        });
        for i in 0..30 {
            w.statements.push(StatementStat {
                id: format!("q{i}"),
                query: format!("select {} from t", "x, ".repeat(100)),
                calls: 1.0,
                total_ms: 1.0,
                mean_ms: 1.0,
                rows: None,
                pages: None,
            });
        }
        let text = workload_text(&w);
        assert!(
            text.contains(
                "\n  2.50 s total · 3 calls · 833.33 ms mean · select * from orders where id = $1\n"
            ),
            "{text}"
        );
        let lines: Vec<&str> = text.lines().filter(|l| l.contains(" calls · ")).collect();
        assert_eq!(lines.len(), 20);
        let long = lines[1].rsplit(" mean · ").next().unwrap_or_default();
        assert_eq!(long.chars().count(), 160);
        // Nothing flagged: no unused or missing index sections.
        assert!(!text.contains("Unused indexes"));
        assert!(!text.contains("Missing indexes"));
    }

    #[test]
    fn detail_lists_columns_keys_and_skips_empty_sections() {
        let d = ObjectDetail {
            object: ObjectInfo {
                schema: "public".into(),
                name: "orders".into(),
                estimated_rows: Some(42),
                ..ObjectInfo::default()
            },
            columns: vec![
                ColumnInfo {
                    name: "id".into(),
                    data_type: "bigint".into(),
                    nullable: false,
                    default: Some("nextval('orders_id_seq')".into()),
                    is_primary_key: true,
                    ..ColumnInfo::default()
                },
                ColumnInfo {
                    name: "note".into(),
                    data_type: "text".into(),
                    nullable: true,
                    ..ColumnInfo::default()
                },
            ],
            indexes: vec![IndexInfo {
                name: "orders_pkey".into(),
                definition: "CREATE UNIQUE INDEX orders_pkey ON orders (id)".into(),
                ..IndexInfo::default()
            }],
            constraints: vec![ConstraintInfo {
                name: "orders_total_check".into(),
                kind: "CHECK".into(),
                definition: "CHECK (total >= 0)".into(),
            }],
            foreign_keys: vec![ForeignKeyInfo {
                name: "orders_customer_fk".into(),
                columns: vec!["tenant".into(), "customer_id".into()],
                references: "public.customers".into(),
                referenced_columns: vec!["tenant".into(), "id".into()],
                ..ForeignKeyInfo::default()
            }],
            ..ObjectDetail::default()
        };
        assert_eq!(
            detail_text(&d),
            "public.orders (~42 rows)\n\
             \n\
             Columns:\n\
             \x20 id bigint NOT NULL DEFAULT nextval('orders_id_seq') (primary key)\n\
             \x20 note text\n\
             \n\
             Indexes:\n\
             \x20 orders_pkey: CREATE UNIQUE INDEX orders_pkey ON orders (id)\n\
             \n\
             Constraints:\n\
             \x20 CHECK orders_total_check: CHECK (total >= 0)\n\
             \n\
             Foreign keys:\n\
             \x20 orders_customer_fk (tenant, customer_id) → public.customers (tenant, id)\n"
        );

        let view = ObjectDetail {
            object: ObjectInfo {
                schema: "public".into(),
                name: "v".into(),
                ..ObjectInfo::default()
            },
            ..ObjectDetail::default()
        };
        assert_eq!(detail_text(&view), "public.v\n\nColumns:\n");
    }
}
