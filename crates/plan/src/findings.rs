//! Findings: rules that point at the expensive or suspicious parts of a [`Plan`].
//!
//! Rules only see [`PlanNode`]s (never engine output). Each finding links to a node id
//! (or to none, for plan-wide findings) and carries a score used for ranking: the rule's
//! weight scaled by the node's share of the plan's time (actual plans) or cost (estimated).
//! Suggestions are text only; nothing here runs them.

use serde::{Deserialize, Serialize};

use crate::model::{Plan, PlanNode, PlanSource};

/// The rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    /// A whole table or clustered index is read for many rows.
    FullScan,
    /// Actual rows differ from the estimate by a large factor.
    BadEstimate,
    /// A filter throws away most of the rows a scan read.
    RowsRemovedByFilter,
    /// A sort or hash spilled to disk / tempdb.
    Spill,
    /// A nested loop runs its inner side many times, or the inner side is a scan.
    ExpensiveNestedLoop,
    /// SQL Server looks up the base row for every index row (Key / RID Lookup).
    KeyLookup,
    /// A column is converted, which can prevent index use.
    ImplicitConversion,
    /// An index the engine reports (SQL Server) or a filter suggests (PostgreSQL).
    MissingIndex,
}

impl Rule {
    /// Every rule.
    pub const ALL: [Rule; 8] = [
        Rule::FullScan,
        Rule::BadEstimate,
        Rule::RowsRemovedByFilter,
        Rule::Spill,
        Rule::ExpensiveNestedLoop,
        Rule::KeyLookup,
        Rule::ImplicitConversion,
        Rule::MissingIndex,
    ];

    /// Short label.
    pub fn label(self) -> &'static str {
        match self {
            Rule::FullScan => "Full scan",
            Rule::BadEstimate => "Bad row estimate",
            Rule::RowsRemovedByFilter => "Rows removed by filter",
            Rule::Spill => "Spill to disk",
            Rule::ExpensiveNestedLoop => "Expensive nested loop",
            Rule::KeyLookup => "Key lookup",
            Rule::ImplicitConversion => "Implicit conversion",
            Rule::MissingIndex => "Missing index",
        }
    }

    /// Base weight for ranking (0–1).
    fn weight(self) -> f64 {
        match self {
            Rule::MissingIndex => 0.9,
            Rule::FullScan | Rule::Spill => 0.8,
            Rule::RowsRemovedByFilter | Rule::ExpensiveNestedLoop => 0.7,
            Rule::BadEstimate | Rule::KeyLookup => 0.6,
            Rule::ImplicitConversion => 0.5,
        }
    }
}

/// How serious a finding is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Worth knowing.
    Low,
    /// Likely costs time.
    Medium,
    /// Most of the plan's time is here.
    High,
}

/// Rule thresholds (Settings → Plans).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Thresholds {
    /// Rows a scan must read to count as a full-scan finding.
    pub full_scan_rows: f64,
    /// Factor between estimated and actual rows that counts as a bad estimate.
    pub bad_estimate_ratio: f64,
    /// Ignore estimates where both sides are below this many rows.
    pub bad_estimate_min_rows: f64,
    /// Rows a filter must remove (all loops).
    pub filter_removed_rows: f64,
    /// Share of the rows read that the filter removes (0–1).
    pub filter_removed_share: f64,
    /// Executions of a nested loop's inner side.
    pub nested_loop_inner_loops: f64,
    /// Time/cost share of the nested loop's inner side that makes it expensive (0–1).
    pub nested_loop_share: f64,
    /// Key Lookup executions.
    pub key_lookup_executions: f64,
    /// Share of time/cost at which a finding becomes High severity (0–1).
    pub high_share: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            full_scan_rows: 10_000.0,
            bad_estimate_ratio: 10.0,
            bad_estimate_min_rows: 100.0,
            filter_removed_rows: 1_000.0,
            filter_removed_share: 0.9,
            nested_loop_inner_loops: 1_000.0,
            nested_loop_share: 0.3,
            key_lookup_executions: 100.0,
            high_share: 0.4,
        }
    }
}

/// One finding.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// Which rule.
    pub rule: Rule,
    /// The node it is about (`None`: the whole plan).
    pub node_id: Option<u32>,
    /// Severity.
    pub severity: Severity,
    /// Ranking score (higher first).
    pub score: f64,
    /// One-line headline.
    pub title: String,
    /// What was measured.
    pub detail: String,
    /// What to try (text only).
    pub suggestion: Option<String>,
}

fn fmt_rows(n: f64) -> String {
    let n = n.round() as i64;
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// The table part of an object (`orders.orders_pkey` → `orders`).
fn table_of(node: &PlanNode, source: PlanSource) -> Option<String> {
    let obj = node.object.as_deref()?;
    Some(match source {
        // PostgreSQL objects are `table` or `table.index`.
        PlanSource::Postgres => obj.split('.').next().unwrap_or(obj).to_owned(),
        // SQL Server objects are `schema.table` or `schema.table.index`.
        PlanSource::SqlServer => {
            let parts: Vec<&str> = obj.split('.').collect();
            parts[..parts.len().min(2)].join(".")
        }
    })
}

fn is_full_scan(node: &PlanNode, source: PlanSource) -> bool {
    match source {
        PlanSource::Postgres => node.operation == "Seq Scan",
        PlanSource::SqlServer => matches!(
            node.operation.as_str(),
            "Table Scan" | "Clustered Index Scan" | "Index Scan"
        ),
    }
}

fn detail_num(node: &PlanNode, key: &str) -> Option<f64> {
    node.details.get(key)?.parse().ok()
}

/// Rows a scan read (before its filter), as far as the plan says.
fn rows_read(node: &PlanNode) -> Option<f64> {
    if let Some(read) = detail_num(node, "Actual Rows Read") {
        return Some(read);
    }
    let loops = node.loops.unwrap_or(1.0);
    if let Some(actual) = node.actual_rows {
        let removed = detail_num(node, "Rows Removed by Filter").unwrap_or(0.0);
        return Some((actual + removed) * loops);
    }
    detail_num(node, "Estimated Rows Read").or(node.estimated_rows)
}

/// Columns compared in a PostgreSQL condition: `((status = 'new'::text) AND (total > …))`
/// gives `status`, `total` (equality comparisons first).
fn compared_columns(cond: &str) -> Vec<String> {
    let mut eq = Vec::new();
    let mut range = Vec::new();
    for (i, _) in cond.match_indices('(') {
        let rest = &cond[i + 1..];
        let ident: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if ident.is_empty() || ident.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        let after = rest[ident.len()..].trim_start();
        if eq.contains(&ident) || range.contains(&ident) {
            continue;
        }
        if after.starts_with("= ") {
            eq.push(ident);
        } else if ["< ", "> ", "<= ", ">= ", "~~ ", "BETWEEN"]
            .iter()
            .any(|op| after.starts_with(op))
        {
            range.push(ident);
        }
    }
    eq.extend(range);
    eq
}

struct Ctx<'a> {
    plan: &'a Plan,
    t: &'a Thresholds,
    out: Vec<Finding>,
}

impl Ctx<'_> {
    fn add(
        &mut self,
        rule: Rule,
        node: Option<&PlanNode>,
        share: f64,
        title: String,
        detail: String,
        suggestion: Option<String>,
    ) {
        let severity = if share >= self.t.high_share {
            Severity::High
        } else if share >= self.t.high_share / 4.0 {
            Severity::Medium
        } else {
            Severity::Low
        };
        self.out.push(Finding {
            rule,
            node_id: node.map(|n| n.id),
            severity,
            score: rule.weight() * (0.25 + share),
            title,
            detail,
            suggestion,
        });
    }

    fn subtree_share(&self, node: &PlanNode) -> f64 {
        node.walk().iter().map(|n| self.plan.weight(n)).sum()
    }
}

/// Run every rule over `plan`; findings sorted by score, highest first.
pub fn analyze(plan: &Plan, t: &Thresholds) -> Vec<Finding> {
    let mut cx = Ctx {
        plan,
        t,
        out: Vec::new(),
    };
    let source = plan.source;
    // Below a LIMIT / TOP a node stops early, so fewer rows than estimated is expected.
    let mut limited = std::collections::HashSet::new();
    for n in plan.nodes() {
        if matches!(n.operation.as_str(), "Limit" | "Top") {
            limited.extend(n.walk().into_iter().skip(1).map(|d| d.id));
        }
    }
    for node in plan.nodes() {
        let share = plan.weight(node);
        let what = node
            .object
            .clone()
            .unwrap_or_else(|| node.operation.clone());

        // Full scan.
        if is_full_scan(node, source)
            && let Some(read) = rows_read(node).filter(|r| *r >= t.full_scan_rows)
        {
            let filter_cols: Vec<String> = node
                .predicates
                .iter()
                .filter(|p| p.kind == "Filter")
                .flat_map(|p| compared_columns(&p.text))
                .collect();
            let suggestion = match (source, table_of(node, source)) {
                (PlanSource::Postgres, Some(table)) if !filter_cols.is_empty() => Some(format!(
                    "CREATE INDEX ON {table} ({});",
                    filter_cols.join(", ")
                )),
                _ => Some(
                    "Add an index on the filtered or joined columns, or narrow the query.".into(),
                ),
            };
            cx.add(
                Rule::FullScan,
                Some(node),
                share,
                format!("{} on {what}", node.operation),
                format!("Reads {} rows.", fmt_rows(read)),
                suggestion,
            );
        }

        // Bad estimate.
        if let (Some(actual), Some(estimated), Some(ratio)) =
            (node.actual_rows, node.estimated_rows, node.estimate_ratio())
            && actual.max(estimated) >= t.bad_estimate_min_rows
            && (ratio >= t.bad_estimate_ratio
                || ratio <= 1.0 / t.bad_estimate_ratio && !limited.contains(&node.id))
        {
            let (factor, way) = if ratio >= 1.0 {
                (ratio, "under")
            } else {
                (1.0 / ratio, "over")
            };
            let stats = match (source, table_of(node, source)) {
                (PlanSource::Postgres, Some(tb)) => format!("ANALYZE {tb};"),
                (PlanSource::SqlServer, Some(tb)) => format!("UPDATE STATISTICS {tb};"),
                _ => "Refresh statistics on the tables involved.".into(),
            };
            cx.add(
                Rule::BadEstimate,
                Some(node),
                share,
                format!("Rows {way}-estimated {factor:.0}× at {}", node.operation),
                format!(
                    "Estimated {} rows, got {} per execution.",
                    fmt_rows(estimated),
                    fmt_rows(actual)
                ),
                Some(stats),
            );
        }

        // Rows removed by filter (PostgreSQL reports it per loop).
        if let (Some(removed), Some(actual)) =
            (detail_num(node, "Rows Removed by Filter"), node.actual_rows)
        {
            let loops = node.loops.unwrap_or(1.0);
            let removed_total = removed * loops;
            let kept_share = removed / (removed + actual).max(1.0);
            if removed_total >= t.filter_removed_rows && kept_share >= t.filter_removed_share {
                cx.add(
                    Rule::RowsRemovedByFilter,
                    Some(node),
                    share,
                    format!(
                        "Filter discards {:.0}% of rows at {what}",
                        kept_share * 100.0
                    ),
                    format!(
                        "Read {} rows to return {}.",
                        fmt_rows((removed + actual) * loops),
                        fmt_rows(actual * loops)
                    ),
                    Some("An index on the filtered columns lets the scan skip those rows.".into()),
                );
            }
        }

        // Spill.
        let spilled = node
            .warnings
            .iter()
            .any(|w| w.contains("pill") || w.contains("tempdb") || w.contains("batches"))
            || node.io.temp_written.is_some_and(|p| p > 0.0)
                && (node.operation.contains("Sort") || node.operation.contains("Hash"));
        if spilled {
            let detail = node
                .warnings
                .iter()
                .find(|w| w.contains("pill") || w.contains("tempdb") || w.contains("batches"))
                .cloned()
                .unwrap_or_else(|| {
                    format!(
                        "Wrote {} temporary pages.",
                        fmt_rows(node.io.temp_written.unwrap_or(0.0))
                    )
                });
            cx.add(
                Rule::Spill,
                Some(node),
                share,
                format!("{} spilled to disk", node.operation),
                detail,
                Some(match source {
                    PlanSource::Postgres => {
                        "Raise work_mem for this query (SET LOCAL work_mem = '64MB'), or sort fewer rows."
                            .into()
                    }
                    PlanSource::SqlServer => {
                        "Update statistics so the memory grant fits, or reduce the rows sorted or hashed."
                            .into()
                    }
                }),
            );
        }

        // Expensive nested loop: many inner executions, or a scan on the inner side.
        if node.operation.starts_with("Nested Loop")
            && let Some(inner) = node.children.get(1)
        {
            let inner_loops = inner.loops.or(node.children[0].rows()).unwrap_or(0.0);
            // The repeated inner side is what costs; the loop being the whole query is fine.
            let inner_share = cx.subtree_share(inner);
            if inner_loops >= t.nested_loop_inner_loops
                && (is_full_scan(inner, source) || inner_share >= t.nested_loop_share)
            {
                cx.add(
                    Rule::ExpensiveNestedLoop,
                    Some(node),
                    inner_share,
                    format!("Nested loop runs {} {} times", inner.operation, fmt_rows(inner_loops)),
                    format!(
                        "The inner side ({}) executes once per outer row and takes {:.0}% of the plan.",
                        inner.object.as_deref().unwrap_or(&inner.operation),
                        inner_share * 100.0
                    ),
                    Some("Index the inner side's join columns, or let the planner hash or merge join (check estimates).".into()),
                );
            }
        }

        // Key lookup (SQL Server).
        if matches!(node.operation.as_str(), "Key Lookup" | "RID Lookup")
            && node.loops.unwrap_or(t.key_lookup_executions) >= t.key_lookup_executions
        {
            cx.add(
                Rule::KeyLookup,
                Some(node),
                share,
                format!("{} on {what}", node.operation),
                format!(
                    "Looks up the base row {} times.",
                    node.loops.map(fmt_rows).unwrap_or_else(|| "many".into())
                ),
                Some("Add the looked-up columns to the index as INCLUDE columns.".into()),
            );
        }

        // Implicit conversion.
        let converted = node
            .warnings
            .iter()
            .find(|w| w.starts_with("Type conversion"))
            .cloned()
            .or_else(|| {
                // PostgreSQL shows a cast column as `((col)::type …`.
                node.predicates
                    .iter()
                    .find(|p| p.text.contains(")::") && p.text.starts_with("(("))
                    .map(|p| format!("{}: {}", p.kind, p.text))
            });
        if let Some(detail) = converted {
            cx.add(
                Rule::ImplicitConversion,
                Some(node),
                share,
                format!("Column converted at {what}"),
                detail,
                Some(
                    "Compare the column with a value of its own type so an index can be used."
                        .into(),
                ),
            );
        }
    }

    // Missing indexes the engine reported.
    for m in &plan.missing_indexes {
        let impact = m.impact.unwrap_or(50.0) / 100.0;
        let target = plan
            .nodes()
            .into_iter()
            .filter(|n| {
                n.object
                    .as_deref()
                    .is_some_and(|o| o.starts_with(&m.table.replace(['[', ']'], "")))
            })
            .max_by(|a, b| plan.weight(a).total_cmp(&plan.weight(b)));
        cx.add(
            Rule::MissingIndex,
            target,
            impact,
            format!("Missing index on {}", m.table.replace(['[', ']'], "")),
            format!(
                "SQL Server estimates {:.0}% improvement.",
                m.impact.unwrap_or(0.0)
            ),
            Some(m.create_statement()),
        );
    }

    let mut out = cx.out;
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compared_columns_from_conditions() {
        assert_eq!(
            compared_columns("((status = 'new'::text) AND (total > '900'::numeric))"),
            ["status", "total"]
        );
        assert_eq!(
            compared_columns("((total < '50'::numeric) AND (status = 'new'::text))"),
            ["status", "total"]
        );
        assert!(compared_columns("(lower(email) = 'x'::text)").is_empty());
        assert_eq!(fmt_rows(1234567.0), "1,234,567");
    }
}
