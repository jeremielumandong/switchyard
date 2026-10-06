//! Side-by-side comparison of two plans: totals and per-operator deltas.
//!
//! Operators are matched by operation and object (`Seq Scan on orders`); the n-th
//! occurrence in one plan pairs with the n-th in the other. Operators found in only one plan
//! appear with the other side empty, which is how a new index shows up (`Seq Scan` gone,
//! `Index Scan` new).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::model::{Plan, PlanNode};

/// One measure in both plans.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Pair {
    /// Value in the first (baseline) plan.
    pub a: Option<f64>,
    /// Value in the second plan.
    pub b: Option<f64>,
}

impl Pair {
    fn new(a: Option<f64>, b: Option<f64>) -> Self {
        Self { a, b }
    }

    /// `b - a`.
    pub fn delta(&self) -> Option<f64> {
        Some(self.b? - self.a?)
    }

    /// Change from `a` to `b` in percent (`-80` = 80% less).
    pub fn percent(&self) -> Option<f64> {
        let (a, b) = (self.a?, self.b?);
        if a == 0.0 {
            return (b == 0.0).then_some(0.0);
        }
        Some((b - a) / a * 100.0)
    }
}

/// One operator in either plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeDelta {
    /// `Seq Scan on orders`.
    pub label: String,
    /// Node id in the first plan.
    pub a: Option<u32>,
    /// Node id in the second plan.
    pub b: Option<u32>,
    /// Self time, ms.
    pub self_time_ms: Pair,
    /// Rows per execution (actual, else estimated).
    pub rows: Pair,
    /// Pages touched.
    pub pages: Pair,
    /// Subtree cost.
    pub cost: Pair,
}

/// Two plans compared.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
    /// Execution time (actual plans), ms.
    pub time_ms: Pair,
    /// Planning time, ms.
    pub planning_ms: Pair,
    /// Rows the statement returned (root rows).
    pub rows: Pair,
    /// Pages touched by all operators.
    pub pages: Pair,
    /// Root cost.
    pub cost: Pair,
    /// Operators, in the first plan's order, then those only in the second.
    pub nodes: Vec<NodeDelta>,
}

fn label(n: &PlanNode) -> String {
    match &n.object {
        Some(o) => format!("{} on {o}", n.operation),
        None => n.operation.clone(),
    }
}

fn total_pages(p: &Plan) -> Option<f64> {
    let pages: Vec<f64> = p.nodes().iter().filter_map(|n| n.io.pages()).collect();
    // Buffers are inclusive of children on PostgreSQL, so the root holds the total; SQL
    // Server's logical reads are per operator, so they add up.
    match p.source {
        crate::PlanSource::Postgres => p.root.io.pages(),
        crate::PlanSource::SqlServer => (!pages.is_empty()).then(|| pages.iter().sum()),
    }
}

/// A node's own pages (PostgreSQL buffers include children; subtract them).
fn own_pages(p: &Plan, n: &PlanNode) -> Option<f64> {
    let pages = n.io.pages()?;
    Some(match p.source {
        crate::PlanSource::Postgres => {
            let kids: f64 = n.children.iter().filter_map(|c| c.io.pages()).sum();
            (pages - kids).max(0.0)
        }
        crate::PlanSource::SqlServer => pages,
    })
}

/// Compare plan `b` against baseline `a`.
pub fn compare(a: &Plan, b: &Plan) -> Comparison {
    let mut b_left: HashMap<String, Vec<&PlanNode>> = HashMap::new();
    for n in b.nodes() {
        b_left.entry(label(n)).or_default().push(n);
    }
    for v in b_left.values_mut() {
        v.reverse(); // pop() yields them in plan order
    }
    let delta = |x: Option<&PlanNode>, y: Option<&PlanNode>| NodeDelta {
        label: x.or(y).map(label).unwrap_or_default(),
        a: x.map(|n| n.id),
        b: y.map(|n| n.id),
        self_time_ms: Pair::new(
            x.and_then(PlanNode::self_time_ms),
            y.and_then(PlanNode::self_time_ms),
        ),
        rows: Pair::new(x.and_then(PlanNode::rows), y.and_then(PlanNode::rows)),
        pages: Pair::new(
            x.and_then(|n| own_pages(a, n)),
            y.and_then(|n| own_pages(b, n)),
        ),
        cost: Pair::new(x.and_then(|n| n.cost), y.and_then(|n| n.cost)),
    };
    let mut nodes = Vec::new();
    for n in a.nodes() {
        let other = b_left.get_mut(&label(n)).and_then(Vec::pop);
        nodes.push(delta(Some(n), other));
    }
    let mut only_b: Vec<&PlanNode> = b_left.into_values().flatten().collect();
    only_b.sort_by_key(|n| n.id);
    nodes.extend(only_b.into_iter().map(|n| delta(None, Some(n))));
    Comparison {
        time_ms: Pair::new(a.total_time_ms(), b.total_time_ms()),
        planning_ms: Pair::new(a.planning_ms, b.planning_ms),
        rows: Pair::new(a.root.rows(), b.root.rows()),
        pages: Pair::new(total_pages(a), total_pages(b)),
        cost: Pair::new(a.root.cost, b.root.cost),
        nodes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Io, PlanKind, PlanSource};

    fn node(op: &str, obj: Option<&str>, time: f64, rows: f64, pages: f64) -> PlanNode {
        PlanNode {
            object: obj.map(str::to_owned),
            total_time_ms: Some(time),
            actual_rows: Some(rows),
            estimated_rows: Some(rows),
            io: Io {
                cache_hits: Some(pages),
                disk_reads: None,
                temp_written: None,
            },
            ..PlanNode::op(op)
        }
    }

    #[test]
    fn index_replaces_scan() {
        let before = Plan {
            execution_ms: Some(200.0),
            ..Plan::new(
                PlanSource::Postgres,
                PlanKind::Actual,
                "q",
                PlanNode {
                    children: vec![node("Seq Scan", Some("orders"), 180.0, 25.0, 8000.0)],
                    ..node("Aggregate", None, 200.0, 1.0, 8000.0)
                },
            )
        };
        let after = Plan {
            execution_ms: Some(2.0),
            ..Plan::new(
                PlanSource::Postgres,
                PlanKind::Actual,
                "q",
                PlanNode {
                    children: vec![node(
                        "Index Scan",
                        Some("orders.orders_status_idx"),
                        1.5,
                        25.0,
                        30.0,
                    )],
                    ..node("Aggregate", None, 2.0, 1.0, 30.0)
                },
            )
        };
        let c = compare(&before, &after);
        assert_eq!(c.time_ms.delta(), Some(-198.0));
        assert_eq!(c.time_ms.percent(), Some(-99.0));
        assert_eq!(c.pages, Pair::new(Some(8000.0), Some(30.0)));
        assert_eq!(c.rows.percent(), Some(0.0));
        let labels: Vec<(&str, Option<u32>, Option<u32>)> = c
            .nodes
            .iter()
            .map(|n| (n.label.as_str(), n.a, n.b))
            .collect();
        assert_eq!(
            labels,
            [
                ("Aggregate", Some(0), Some(0)),
                ("Seq Scan on orders", Some(1), None),
                ("Index Scan on orders.orders_status_idx", None, Some(1)),
            ]
        );
        // Aggregate's own time: 20 ms → 0.5 ms; own pages 0 (all in the scan).
        assert_eq!(c.nodes[0].self_time_ms, Pair::new(Some(20.0), Some(0.5)));
        assert_eq!(c.nodes[0].pages, Pair::new(Some(0.0), Some(0.0)));
    }

    #[test]
    fn repeated_operators_pair_in_order() {
        let mk = |times: [f64; 2]| {
            Plan::new(
                PlanSource::SqlServer,
                PlanKind::Actual,
                "q",
                PlanNode {
                    children: vec![
                        node("Index Seek", Some("t.ix"), times[0], 1.0, 3.0),
                        node("Index Seek", Some("t.ix"), times[1], 1.0, 3.0),
                    ],
                    ..node("Nested Loops", None, 10.0, 1.0, 0.0)
                },
            )
        };
        let c = compare(&mk([2.0, 4.0]), &mk([1.0, 8.0]));
        assert_eq!(c.nodes.len(), 3);
        assert_eq!(c.nodes[1].self_time_ms, Pair::new(Some(2.0), Some(1.0)));
        assert_eq!(c.nodes[2].self_time_ms, Pair::new(Some(4.0), Some(8.0)));
        assert_eq!(c.pages, Pair::new(Some(6.0), Some(6.0)));
        assert_eq!(Pair::new(Some(0.0), Some(5.0)).percent(), None);
    }
}
