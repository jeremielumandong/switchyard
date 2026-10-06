//! The normalized plan tree every engine converts into.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Which engine produced a plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanSource {
    /// PostgreSQL `EXPLAIN (FORMAT JSON)`.
    Postgres,
    /// SQL Server showplan XML.
    SqlServer,
}

/// Estimated plans come from the optimizer alone; actual plans ran the statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanKind {
    /// `EXPLAIN` / `SHOWPLAN_XML`: nothing was executed.
    Estimated,
    /// `EXPLAIN ANALYZE` / `STATISTICS XML`: the statement ran (and was rolled back).
    Actual,
}

/// Buffer and I/O counters for one node (all loops). Pages are 8 KB on both engines.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Io {
    /// Pages found in cache (PostgreSQL shared hit, SQL Server logical reads).
    pub cache_hits: Option<f64>,
    /// Pages read from disk (PostgreSQL shared read, SQL Server physical reads).
    pub disk_reads: Option<f64>,
    /// Pages written to temporary storage (spills).
    pub temp_written: Option<f64>,
}

impl Io {
    /// Total pages touched (hits plus reads).
    pub fn pages(&self) -> Option<f64> {
        match (self.cache_hits, self.disk_reads) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        }
    }
}

/// A condition on a node, labelled the way the engine labels it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Predicate {
    /// `Filter`, `Index Cond`, `Hash Cond`, `Seek`, `Predicate`, …
    pub kind: String,
    /// The condition text.
    pub text: String,
}

/// One operator in a plan tree.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanNode {
    /// Pre-order position in the plan, from 0 (the root).
    pub id: u32,
    /// Operation (`Seq Scan`, `Hash Join`, `Clustered Index Seek`, …).
    pub operation: String,
    /// Object touched (table, or `table.index`), if any.
    pub object: Option<String>,
    /// Estimated rows per execution.
    pub estimated_rows: Option<f64>,
    /// Actual rows per execution (actual plans).
    pub actual_rows: Option<f64>,
    /// Executions (PostgreSQL loops, SQL Server executions).
    pub loops: Option<f64>,
    /// Optimizer cost of this subtree, in the engine's own units.
    pub cost: Option<f64>,
    /// Time in this subtree across all executions, ms (actual plans).
    pub total_time_ms: Option<f64>,
    /// Buffer and I/O counters.
    pub io: Io,
    /// Conditions (filters, join and index conditions).
    pub predicates: Vec<Predicate>,
    /// Engine warnings (spills, conversions, missing join predicates, …).
    pub warnings: Vec<String>,
    /// Other engine attributes worth showing (`Sort Method`, `Join Type`, …).
    pub details: BTreeMap<String, String>,
    /// Inputs.
    pub children: Vec<PlanNode>,
}

impl PlanNode {
    /// A node with just an operation (tests and hand-built trees).
    pub fn op(operation: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            ..Default::default()
        }
    }

    /// Time spent in this node alone: its total minus its children's, never negative.
    pub fn self_time_ms(&self) -> Option<f64> {
        let total = self.total_time_ms?;
        let kids: f64 = self.children.iter().filter_map(|c| c.total_time_ms).sum();
        Some((total - kids).max(0.0))
    }

    /// Actual rows across all executions.
    pub fn actual_rows_total(&self) -> Option<f64> {
        Some(self.actual_rows? * self.loops.unwrap_or(1.0))
    }

    /// Rows the node produces as far as it is known: actual if measured, else estimated.
    pub fn rows(&self) -> Option<f64> {
        self.actual_rows.or(self.estimated_rows)
    }

    /// How far the estimate was off: `actual / estimated` (≥ 1 means under-estimated).
    pub fn estimate_ratio(&self) -> Option<f64> {
        let (actual, estimated) = (self.actual_rows?, self.estimated_rows?);
        Some(actual.max(1.0) / estimated.max(1.0))
    }

    /// This node and every descendant, pre-order.
    pub fn walk(&self) -> Vec<&PlanNode> {
        let mut out = Vec::new();
        let mut stack = vec![self];
        while let Some(n) = stack.pop() {
            out.push(n);
            stack.extend(n.children.iter().rev());
        }
        out
    }

    /// Give every node its pre-order id, starting at `next`.
    pub(crate) fn number(&mut self, next: &mut u32) {
        self.id = *next;
        *next += 1;
        for c in &mut self.children {
            c.number(next);
        }
    }
}

/// An index the engine itself reported as missing (SQL Server `MissingIndexes`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MissingIndex {
    /// Estimated improvement, percent.
    pub impact: Option<f64>,
    /// `schema.table`.
    pub table: String,
    /// Equality columns, in order.
    pub equality: Vec<String>,
    /// Inequality columns.
    pub inequality: Vec<String>,
    /// Included columns.
    pub include: Vec<String>,
}

impl MissingIndex {
    /// A `CREATE INDEX` statement for it (text only; nothing runs it).
    pub fn create_statement(&self) -> String {
        let keys: Vec<&str> = self
            .equality
            .iter()
            .chain(&self.inequality)
            .map(String::as_str)
            .collect();
        let name: String = std::iter::once("ix")
            .chain(
                self.table
                    .rsplit('.')
                    .next()
                    .map(|t| t.trim_matches(['[', ']'])),
            )
            .chain(keys.iter().map(|k| k.trim_matches(['[', ']'])))
            .collect::<Vec<_>>()
            .join("_");
        let mut s = format!(
            "CREATE INDEX {name} ON {} ({})",
            self.table,
            keys.join(", ")
        );
        if !self.include.is_empty() {
            s.push_str(&format!(" INCLUDE ({})", self.include.join(", ")));
        }
        s
    }
}

/// A captured plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// Engine that produced it.
    pub source: PlanSource,
    /// Estimated or actual.
    pub kind: PlanKind,
    /// The statement the plan is for.
    pub sql: String,
    /// Root operator.
    pub root: PlanNode,
    /// Planning (compile) time, ms.
    pub planning_ms: Option<f64>,
    /// Execution time, ms (actual plans).
    pub execution_ms: Option<f64>,
    /// Statement-level warnings.
    pub warnings: Vec<String>,
    /// Indexes the engine reported as missing.
    pub missing_indexes: Vec<MissingIndex>,
}

impl Plan {
    /// A plan around `root`, numbering its nodes.
    pub fn new(source: PlanSource, kind: PlanKind, sql: impl Into<String>, root: PlanNode) -> Self {
        let mut plan = Self {
            source,
            kind,
            sql: sql.into(),
            root,
            planning_ms: None,
            execution_ms: None,
            warnings: Vec::new(),
            missing_indexes: Vec::new(),
        };
        plan.renumber();
        plan
    }

    /// Re-assign pre-order ids after editing the tree.
    pub fn renumber(&mut self) {
        let mut next = 0;
        self.root.number(&mut next);
    }

    /// Every node, pre-order (index = id).
    pub fn nodes(&self) -> Vec<&PlanNode> {
        self.root.walk()
    }

    /// The node with this id.
    pub fn node(&self, id: u32) -> Option<&PlanNode> {
        self.nodes().into_iter().find(|n| n.id == id)
    }

    /// Total measured time: the execution time, else the root's subtree time.
    pub fn total_time_ms(&self) -> Option<f64> {
        self.execution_ms.or(self.root.total_time_ms)
    }

    /// Share of the plan's time spent in `node` alone (0–1), when timed.
    pub fn self_share(&self, node: &PlanNode) -> Option<f64> {
        let total = self.total_time_ms().filter(|t| *t > 0.0)?;
        Some((node.self_time_ms()? / total).clamp(0.0, 1.0))
    }

    /// Share of the root's cost in `node`'s subtree minus its children (0–1), for
    /// estimated plans.
    pub fn self_cost_share(&self, node: &PlanNode) -> Option<f64> {
        let total = self.root.cost.filter(|c| *c > 0.0)?;
        let kids: f64 = node.children.iter().filter_map(|c| c.cost).sum();
        Some(((node.cost? - kids).max(0.0) / total).clamp(0.0, 1.0))
    }

    /// Time share if measured, else cost share: what the UI colours by.
    pub fn weight(&self, node: &PlanNode) -> f64 {
        self.self_share(node)
            .or_else(|| self.self_cost_share(node))
            .unwrap_or(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timed(op: &str, total: f64, children: Vec<PlanNode>) -> PlanNode {
        PlanNode {
            total_time_ms: Some(total),
            children,
            ..PlanNode::op(op)
        }
    }

    #[test]
    fn self_time_subtracts_children_and_never_goes_negative() {
        let n = timed(
            "Hash Join",
            10.0,
            vec![timed("Seq Scan", 4.0, vec![]), timed("Hash", 3.5, vec![])],
        );
        assert_eq!(n.self_time_ms(), Some(2.5));
        // Parallel workers can make children add up to more than the parent.
        let odd = timed("Gather", 5.0, vec![timed("Seq Scan", 9.0, vec![])]);
        assert_eq!(odd.self_time_ms(), Some(0.0));
        assert_eq!(PlanNode::op("x").self_time_ms(), None);
    }

    #[test]
    fn ids_are_pre_order_and_shares_add_up() {
        let root = timed(
            "Nested Loop",
            20.0,
            vec![
                timed("Seq Scan", 5.0, vec![]),
                timed("Index Scan", 10.0, vec![timed("Bitmap", 2.0, vec![])]),
            ],
        );
        let plan = Plan::new(PlanSource::Postgres, PlanKind::Actual, "select 1", root);
        let ops: Vec<(u32, &str)> = plan
            .nodes()
            .iter()
            .map(|n| (n.id, n.operation.as_str()))
            .collect();
        assert_eq!(
            ops,
            [
                (0, "Nested Loop"),
                (1, "Seq Scan"),
                (2, "Index Scan"),
                (3, "Bitmap")
            ]
        );
        let shares: f64 = plan.nodes().iter().map(|n| plan.weight(n)).sum();
        assert!((shares - 1.0).abs() < 1e-9, "{shares}");
        assert_eq!(plan.node(2).map(|n| n.self_time_ms()), Some(Some(8.0)));
    }

    #[test]
    fn estimate_ratio_and_totals() {
        let n = PlanNode {
            estimated_rows: Some(10.0),
            actual_rows: Some(5000.0),
            loops: Some(3.0),
            ..PlanNode::op("Seq Scan")
        };
        assert_eq!(n.estimate_ratio(), Some(500.0));
        assert_eq!(n.actual_rows_total(), Some(15000.0));
    }

    #[test]
    fn missing_index_statement() {
        let m = MissingIndex {
            impact: Some(91.0),
            table: "[dbo].[orders]".into(),
            equality: vec!["[customer_id]".into()],
            inequality: vec!["[created_at]".into()],
            include: vec!["[total]".into()],
        };
        assert_eq!(
            m.create_statement(),
            "CREATE INDEX ix_orders_customer_id_created_at ON [dbo].[orders] ([customer_id], [created_at]) INCLUDE ([total])"
        );
    }
}
