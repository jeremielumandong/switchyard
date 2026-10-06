//! SQL Server showplan XML (`SHOWPLAN_XML`, `STATISTICS XML`) → [`Plan`].
//!
//! Operators are `RelOp` elements nested inside their physical operator element
//! (`<RelOp><NestedLoops><RelOp/><RelOp/></NestedLoops></RelOp>`). Everything found between
//! a `RelOp`'s start and its first nested `RelOp` (object, predicates, run-time counters,
//! warnings) belongs to it, so the parser keeps a stack of open operators and attaches each
//! piece to the innermost one.
//!
//! Row counts: `EstimateRows` is per execution; `ActualRows` is the sum over executions and
//! threads, so it is divided by executions to match PostgreSQL's per-loop numbers.
//! `ActualElapsedms` already includes children (row mode).

use quick_xml::events::{BytesStart, Event};

use crate::model::{Io, MissingIndex, Plan, PlanKind, PlanNode, PlanSource, Predicate};
use crate::{PlanError, Result};

/// Measured counters, summed over threads.
#[derive(Default)]
struct Runtime {
    rows: f64,
    executions: f64,
    elapsed_ms: Option<f64>,
    logical_reads: Option<f64>,
    physical_reads: Option<f64>,
    seen: bool,
}

struct Open {
    node: PlanNode,
    runtime: Runtime,
}

/// Elements whose `ScalarOperator/@ScalarString` is a predicate on the current operator.
const PREDICATE_PARENTS: &[(&str, &str)] = &[
    ("Predicate", "Predicate"),
    ("SeekPredicates", "Seek"),
    ("SeekPredicateNew", "Seek"),
    ("HashKeysProbe", "Hash Probe"),
    ("HashKeysBuild", "Hash Build"),
    ("ProbeResidual", "Probe Residual"),
    ("Residual", "Residual"),
    ("OuterReferences", "Outer References"),
];

fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    e.attributes()
        .with_checks(false)
        .flatten()
        .find(|a| a.key.0 == name)
        .map(|a| {
            quick_xml::escape::unescape(&a.value)
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| a.value.to_string())
        })
}

fn num(e: &BytesStart<'_>, name: &str) -> Option<f64> {
    attr(e, name)?.parse().ok()
}

/// Strip brackets for display: `[dbo].[orders]` → `dbo.orders`.
fn plain(s: &str) -> String {
    s.replace(['[', ']'], "")
}

fn warning(e: &BytesStart<'_>) -> Option<String> {
    let name = e.name().0;
    Some(match name {
        "SpillToTempDb" => format!(
            "Spilled to tempdb (spill level {})",
            attr(e, "SpillLevel").unwrap_or_else(|| "?".into())
        ),
        "SortSpillDetails" | "HashSpillDetails" | "ExchangeSpillDetails" => format!(
            "{} wrote {} pages to tempdb",
            name.trim_end_matches("SpillDetails"),
            attr(e, "WritesToTempDb").unwrap_or_else(|| "?".into())
        ),
        "PlanAffectingConvert" => format!(
            "Type conversion may affect {}: {}",
            attr(e, "ConvertIssue").unwrap_or_default(),
            attr(e, "Expression").unwrap_or_default()
        ),
        "NoJoinPredicate" => "No join predicate".into(),
        "ColumnsWithNoStatistics" => "Columns with no statistics".into(),
        "UnmatchedIndexes" => "Unmatched filtered indexes".into(),
        "MemoryGrantWarning" => format!(
            "Memory grant: {}",
            attr(e, "GrantWarningKind").unwrap_or_default()
        ),
        "Wait" => format!(
            "Waited {} ms on {}",
            attr(e, "WaitTime").unwrap_or_default(),
            attr(e, "WaitType").unwrap_or_default()
        ),
        _ => return None,
    })
}

/// One parsed statement.
struct Statement {
    root: Option<PlanNode>,
    text: String,
    compile_ms: Option<f64>,
    elapsed_ms: Option<f64>,
    warnings: Vec<String>,
    missing: Vec<MissingIndex>,
}

/// Parse every statement in one showplan document.
fn statements(xml: &str) -> Result<Vec<Statement>> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out: Vec<Statement> = Vec::new();
    let mut stack: Vec<Open> = Vec::new();
    // Element names from the document root to the current element.
    let mut path: Vec<String> = Vec::new();
    let mut predicate_kind: Option<&'static str> = None;
    let mut predicate_depth = 0usize;
    // Seek keys: the column and comparison come before the value (`Prefix ScanType="EQ"`,
    // `RangeColumns/ColumnReference`, then `RangeExpressions/ScalarOperator`).
    let mut seek_op = "=";
    let mut seek_col: Option<String> = None;
    let mut in_warnings = 0usize;
    let mut missing: Option<MissingIndex> = None;
    let mut usage = String::new();
    loop {
        let ev = reader
            .read_event()
            .map_err(|e| PlanError::Parse(format!("showplan XML: {e}")))?;
        let (start, empty) = match &ev {
            Event::Start(e) => (Some(e.clone()), false),
            Event::Empty(e) => (Some(e.clone()), true),
            _ => (None, false),
        };
        if let Some(e) = start {
            let name = e.name().0.to_owned();
            match name.as_str() {
                n if n.starts_with("Stmt") => {
                    out.push(Statement {
                        root: None,
                        text: attr(&e, "StatementText").unwrap_or_default(),
                        compile_ms: None,
                        elapsed_ms: None,
                        warnings: Vec::new(),
                        missing: Vec::new(),
                    });
                }
                "QueryPlan" => {
                    if let Some(s) = out.last_mut() {
                        s.compile_ms = num(&e, "CompileTime");
                    }
                }
                "QueryTimeStats" => {
                    if let Some(s) = out.last_mut() {
                        s.elapsed_ms = num(&e, "ElapsedTime");
                    }
                }
                "RelOp" => {
                    let physical = attr(&e, "PhysicalOp").unwrap_or_else(|| "?".into());
                    let logical = attr(&e, "LogicalOp").unwrap_or_default();
                    let mut node = PlanNode {
                        // "Hash Match" says little; its logical op says which kind.
                        operation: if physical == "Hash Match" && !logical.is_empty() {
                            format!("Hash Match ({logical})")
                        } else {
                            physical.clone()
                        },
                        estimated_rows: num(&e, "EstimateRows"),
                        cost: num(&e, "EstimatedTotalSubtreeCost"),
                        ..Default::default()
                    };
                    for (key, label) in [
                        ("LogicalOp", "Logical Op"),
                        ("EstimatedExecutionMode", "Execution Mode"),
                        ("Parallel", "Parallel"),
                        ("EstimateIO", "Estimated I/O"),
                        ("EstimateCPU", "Estimated CPU"),
                        ("EstimatedRowsRead", "Estimated Rows Read"),
                    ] {
                        if let Some(v) = attr(&e, key) {
                            node.details.insert(label.into(), v);
                        }
                    }
                    if empty {
                        attach(&mut stack, &mut out, node);
                    } else {
                        stack.push(Open {
                            node,
                            runtime: Runtime::default(),
                        });
                    }
                }
                "RunTimeCountersPerThread" => {
                    if let Some(top) = stack.last_mut() {
                        let r = &mut top.runtime;
                        r.seen = true;
                        r.rows += num(&e, "ActualRows").unwrap_or(0.0);
                        r.executions += num(&e, "ActualExecutions").unwrap_or(0.0);
                        if let Some(ms) = num(&e, "ActualElapsedms") {
                            r.elapsed_ms = Some(r.elapsed_ms.unwrap_or(0.0).max(ms));
                        }
                        if let Some(n) = num(&e, "ActualLogicalReads") {
                            r.logical_reads = Some(r.logical_reads.unwrap_or(0.0) + n);
                        }
                        if let Some(n) = num(&e, "ActualPhysicalReads") {
                            r.physical_reads = Some(r.physical_reads.unwrap_or(0.0) + n);
                        }
                        if let Some(n) = num(&e, "ActualRowsRead") {
                            let prev: f64 = top
                                .node
                                .details
                                .get("Actual Rows Read")
                                .and_then(|v| v.parse().ok())
                                .unwrap_or(0.0);
                            top.node
                                .details
                                .insert("Actual Rows Read".into(), (prev + n).to_string());
                        }
                    }
                }
                // The object an operator reads; not the ones inside MissingIndexes.
                "Object" if missing.is_none() && !path.iter().any(|p| p == "MissingIndexes") => {
                    if let Some(top) = stack.last_mut()
                        && top.node.object.is_none()
                    {
                        let table = [attr(&e, "Schema"), attr(&e, "Table")]
                            .into_iter()
                            .flatten()
                            .map(|s| plain(&s))
                            .collect::<Vec<_>>()
                            .join(".");
                        let index = attr(&e, "Index").map(|s| plain(&s));
                        top.node.object = match index {
                            Some(ix) if !table.is_empty() => Some(format!("{table}.{ix}")),
                            Some(ix) => Some(ix),
                            None if !table.is_empty() => Some(table),
                            None => None,
                        };
                    }
                }
                // A clustered seek per outer row to fetch the rest of the row: SSMS shows it
                // as "Key Lookup".
                "IndexScan" if matches!(attr(&e, "Lookup").as_deref(), Some("1" | "true")) => {
                    if let Some(top) = stack.last_mut()
                        && top.node.operation == "Clustered Index Seek"
                    {
                        top.node.operation = "Key Lookup".into();
                    }
                }
                "Warnings" => in_warnings += 1,
                w if in_warnings > 0 => {
                    if let Some(text) = warning(&e) {
                        match stack.last_mut() {
                            Some(top) => top.node.warnings.push(text),
                            None => {
                                if let Some(s) = out.last_mut() {
                                    s.warnings.push(text);
                                }
                            }
                        }
                    } else if w == "SpillOccurred"
                        && attr(&e, "Detail").as_deref() == Some("1")
                        && let Some(top) = stack.last_mut()
                    {
                        top.node.warnings.push("Spill occurred".into());
                    }
                }
                "MissingIndexGroup" => {
                    usage.clear();
                    missing = Some(MissingIndex {
                        impact: num(&e, "Impact"),
                        table: String::new(),
                        equality: Vec::new(),
                        inequality: Vec::new(),
                        include: Vec::new(),
                    });
                }
                "MissingIndex" => {
                    if let Some(m) = missing.as_mut() {
                        m.table = [attr(&e, "Schema"), attr(&e, "Table")]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join(".");
                    }
                }
                "ColumnGroup" => usage = attr(&e, "Usage").unwrap_or_default(),
                "Column" if missing.is_some() && path.iter().any(|p| p == "ColumnGroup") => {
                    if let (Some(m), Some(col)) = (missing.as_mut(), attr(&e, "Name")) {
                        match usage.as_str() {
                            "EQUALITY" => m.equality.push(col),
                            "INEQUALITY" => m.inequality.push(col),
                            _ => m.include.push(col),
                        }
                    }
                }
                "Prefix" | "StartRange" | "EndRange" if predicate_kind == Some("Seek") => {
                    seek_op = match attr(&e, "ScanType").as_deref() {
                        Some("GT") => ">",
                        Some("GE") => ">=",
                        Some("LT") => "<",
                        Some("LE") => "<=",
                        Some("NE") => "<>",
                        Some("IS") => "IS",
                        _ => "=",
                    };
                    seek_col = None;
                }
                "ColumnReference"
                    if predicate_kind == Some("Seek")
                        && seek_col.is_none()
                        && path.last().is_some_and(|p| p == "RangeColumns") =>
                {
                    seek_col = attr(&e, "Column");
                }
                n => {
                    if let Some((_, label)) = PREDICATE_PARENTS.iter().find(|(p, _)| *p == n)
                        && predicate_kind.is_none()
                        && !stack.is_empty()
                    {
                        predicate_kind = Some(label);
                        predicate_depth = path.len();
                    } else if n == "ScalarOperator"
                        && let Some(kind) = predicate_kind
                        && let Some(text) = attr(&e, "ScalarString")
                        && let Some(top) = stack.last_mut()
                    {
                        // Only the outermost scalar under the predicate element.
                        predicate_kind = None;
                        let text = match seek_col.take() {
                            Some(col) if kind == "Seek" => format!("{col} {seek_op} {text}"),
                            _ => text,
                        };
                        top.node.predicates.push(Predicate {
                            kind: kind.into(),
                            text,
                        });
                    }
                }
            }
            if !empty {
                path.push(name);
            }
            continue;
        }
        match ev {
            Event::End(e) => {
                let name = e.name().0.to_owned();
                path.pop();
                if predicate_kind.is_some() && path.len() <= predicate_depth {
                    predicate_kind = None;
                }
                match name.as_str() {
                    "RelOp" => {
                        if let Some(open) = stack.pop() {
                            let node = finish(open);
                            attach(&mut stack, &mut out, node);
                        }
                    }
                    "Warnings" => in_warnings = in_warnings.saturating_sub(1),
                    "MissingIndexGroup" => {
                        if let (Some(m), Some(s)) = (missing.take(), out.last_mut()) {
                            s.missing.push(m);
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// Fold measured counters into the node.
fn finish(open: Open) -> PlanNode {
    let Open { mut node, runtime } = open;
    if runtime.seen {
        let execs = runtime.executions;
        node.loops = Some(execs);
        node.actual_rows = Some(if execs > 0.0 {
            runtime.rows / execs
        } else {
            0.0
        });
        node.total_time_ms = runtime.elapsed_ms;
        node.io = Io {
            cache_hits: runtime.logical_reads,
            disk_reads: runtime.physical_reads,
            temp_written: None,
        };
        if execs == 0.0 {
            node.warnings.push("Never executed".into());
        }
    }
    node
}

/// SQL Server omits run-time counters on some operators (Compute Scalar, …). One with a
/// single input passes its rows through, so it takes that input's counters.
fn fill_pass_through(node: &mut PlanNode) {
    for c in &mut node.children {
        fill_pass_through(c);
    }
    if node.actual_rows.is_none()
        && let [child] = node.children.as_slice()
    {
        node.actual_rows = child.actual_rows;
        node.loops = child.loops;
        node.total_time_ms = child.total_time_ms;
    }
}

/// Hang a finished node under the operator still open, or make it the statement's root.
fn attach(stack: &mut [Open], out: &mut [Statement], node: PlanNode) {
    match stack.last_mut() {
        Some(parent) => parent.node.children.push(node),
        None => {
            if let Some(s) = out.last_mut() {
                s.root = Some(node);
            }
        }
    }
}

/// Parse showplan documents (one per statement or batch) captured for `sql`. Several
/// statements become children of a synthetic `Batch` root.
pub fn parse_many(docs: &[String], sql: &str) -> Result<Plan> {
    let mut stmts: Vec<Statement> = Vec::new();
    for d in docs {
        stmts.extend(statements(d)?.into_iter().filter(|s| s.root.is_some()));
    }
    let actual = stmts
        .iter()
        .filter_map(|s| s.root.as_ref())
        .any(|r| r.walk().iter().any(|n| n.actual_rows.is_some()));
    if actual {
        for s in &mut stmts {
            if let Some(root) = s.root.as_mut() {
                fill_pass_through(root);
            }
        }
    }
    let kind = if actual {
        PlanKind::Actual
    } else {
        PlanKind::Estimated
    };
    let (root, mut planning, mut execution, mut warnings, mut missing) =
        match <[Statement; 1]>::try_from(stmts) {
            Ok([s]) => (
                s.root.unwrap_or_default(),
                s.compile_ms,
                s.elapsed_ms,
                s.warnings,
                s.missing,
            ),
            Err(stmts) if stmts.is_empty() => {
                return Err(PlanError::Parse("showplan has no operators".into()));
            }
            Err(stmts) => {
                let mut root = PlanNode::op("Batch");
                let (mut p, mut x, mut w, mut m) = (None, None, Vec::new(), Vec::new());
                for s in stmts {
                    let mut child = s.root.unwrap_or_default();
                    child
                        .details
                        .insert("Statement".into(), s.text.trim().to_owned());
                    p = sum(p, s.compile_ms);
                    x = sum(x, s.elapsed_ms);
                    w.extend(s.warnings);
                    m.extend(s.missing);
                    root.cost = sum(root.cost, child.cost);
                    root.total_time_ms = sum(root.total_time_ms, child.total_time_ms);
                    root.children.push(child);
                }
                (root, p, x, w, m)
            }
        };
    let mut plan = Plan::new(PlanSource::SqlServer, kind, sql, root);
    plan.planning_ms = planning.take();
    plan.execution_ms = execution.take();
    plan.warnings = std::mem::take(&mut warnings);
    plan.missing_indexes = std::mem::take(&mut missing);
    Ok(plan)
}

fn sum(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    }
}

/// Parse one showplan document.
pub fn parse(xml: &str, sql: &str) -> Result<Plan> {
    parse_many(&[xml.to_owned()], sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACTUAL: &str = r#"<?xml version="1.0" encoding="utf-16"?>
<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan" Version="1.6">
 <BatchSequence><Batch><Statements>
  <StmtSimple StatementText="select * from orders o join customers c on c.id = o.customer_id where o.total &gt; 990" StatementType="SELECT">
   <QueryPlan CompileTime="3">
    <MissingIndexes>
     <MissingIndexGroup Impact="87.5">
      <MissingIndex Database="[shop]" Schema="[dbo]" Table="[orders]">
       <ColumnGroup Usage="INEQUALITY"><Column Name="[total]" ColumnId="4"/></ColumnGroup>
       <ColumnGroup Usage="INCLUDE"><Column Name="[customer_id]" ColumnId="2"/></ColumnGroup>
      </MissingIndex>
     </MissingIndexGroup>
    </MissingIndexes>
    <QueryTimeStats CpuTime="40" ElapsedTime="52"/>
    <RelOp NodeId="0" PhysicalOp="Nested Loops" LogicalOp="Inner Join" EstimateRows="10" EstimatedTotalSubtreeCost="5.2">
     <RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="1000" ActualExecutions="1" ActualElapsedms="50"/></RunTimeInformation>
     <NestedLoops Optimized="false">
      <OuterReferences><ColumnReference Column="customer_id"/></OuterReferences>
      <RelOp NodeId="1" PhysicalOp="Clustered Index Scan" LogicalOp="Clustered Index Scan" EstimateRows="10" EstimatedTotalSubtreeCost="4.9">
       <Warnings><PlanAffectingConvert ConvertIssue="Seek Plan" Expression="CONVERT_IMPLICIT(numeric(12,2),[o].[total],0)"/></Warnings>
       <RunTimeInformation>
        <RunTimeCountersPerThread Thread="1" ActualRows="600" ActualExecutions="1" ActualElapsedms="30" ActualLogicalReads="5000"/>
        <RunTimeCountersPerThread Thread="2" ActualRows="400" ActualExecutions="1" ActualElapsedms="28" ActualLogicalReads="3000"/>
       </RunTimeInformation>
       <IndexScan Ordered="false">
        <Object Database="[shop]" Schema="[dbo]" Table="[orders]" Index="[PK_orders]" Alias="[o]"/>
        <Predicate><ScalarOperator ScalarString="[shop].[dbo].[orders].[total] as [o].[total]&gt;(990.00)"><Compare CompareOp="GT"><ScalarOperator ScalarString="inner"/></Compare></ScalarOperator></Predicate>
       </IndexScan>
      </RelOp>
      <RelOp NodeId="2" PhysicalOp="Clustered Index Seek" LogicalOp="Clustered Index Seek" EstimateRows="1" EstimatedTotalSubtreeCost="0.3">
       <RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="1000" ActualExecutions="1000" ActualElapsedms="12"/></RunTimeInformation>
       <IndexScan><Object Schema="[dbo]" Table="[customers]" Index="[PK_customers]"/>
        <SeekPredicates><SeekPredicateNew><SeekKeys><Prefix ScanType="EQ"><RangeColumns/><RangeExpressions><ScalarOperator ScalarString="[o].[customer_id]"/></RangeExpressions></Prefix></SeekKeys></SeekPredicateNew></SeekPredicates>
       </IndexScan>
      </RelOp>
     </NestedLoops>
    </RelOp>
   </QueryPlan>
  </StmtSimple>
 </Statements></Batch></BatchSequence>
</ShowPlanXML>"#;

    #[test]
    fn actual_plan_tree_counters_and_missing_index() {
        let p = parse(ACTUAL, "q").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(p.kind, PlanKind::Actual);
        assert_eq!((p.planning_ms, p.execution_ms), (Some(3.0), Some(52.0)));
        let ops: Vec<&str> = p.nodes().iter().map(|n| n.operation.as_str()).collect();
        assert_eq!(
            ops,
            [
                "Nested Loops",
                "Clustered Index Scan",
                "Clustered Index Seek"
            ]
        );
        let scan = &p.root.children[0];
        assert_eq!(scan.object.as_deref(), Some("dbo.orders.PK_orders"));
        // Two threads, one execution each (as SSMS counts them): 1,000 rows over two
        // executions; elapsed is the slowest thread.
        assert_eq!(
            (scan.actual_rows, scan.loops, scan.total_time_ms),
            (Some(500.0), Some(2.0), Some(30.0))
        );
        assert_eq!(scan.actual_rows_total(), Some(1000.0));
        assert_eq!(scan.io.cache_hits, Some(8000.0));
        assert_eq!(scan.predicates[0].kind, "Predicate");
        assert!(scan.predicates[0].text.ends_with(">(990.00)"));
        assert!(scan.warnings[0].starts_with("Type conversion may affect Seek Plan"));
        let seek = &p.root.children[1];
        assert_eq!((seek.actual_rows, seek.loops), (Some(1.0), Some(1000.0)));
        assert_eq!(seek.predicates[0].kind, "Seek");
        assert_eq!(
            p.missing_indexes[0].create_statement(),
            "CREATE INDEX ix_orders_total ON [dbo].[orders] ([total]) INCLUDE ([customer_id])"
        );
        assert_eq!(p.root.self_time_ms(), Some(8.0));
    }

    #[test]
    fn estimated_plans_have_no_runtime_and_errors_are_reported() {
        let est = ACTUAL
            .lines()
            .filter(|l| {
                !l.contains("RunTimeCountersPerThread") && !l.contains("RunTimeInformation")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let p = parse(&est, "q").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(p.kind, PlanKind::Estimated);
        assert!(p.nodes().iter().all(|n| n.actual_rows.is_none()));
        assert!(parse("<ShowPlanXML/>", "q").is_err());
        assert!(parse("<a><b></a>", "q").is_err());
    }
}
