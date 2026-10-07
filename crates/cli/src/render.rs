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
