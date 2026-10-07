//! Plan geometry, free of GPUI: the graph layout (root on the left, inputs to the right),
//! the flame (icicle) bars, heat buckets, edge widths, and where a node's objects appear in
//! the statement text.

use std::collections::HashMap;
use std::ops::Range;

use switchyard_core::plan::{Plan, PlanNode};

/// Node card width in the graph, at zoom 1.
pub const NODE_W: f32 = 200.;
/// Node card height in the graph, at zoom 1.
pub const NODE_H: f32 = 66.;
/// Horizontal gap between a node and its inputs.
pub const COL_GAP: f32 = 44.;
/// Vertical gap between sibling subtrees.
pub const ROW_GAP: f32 = 16.;

/// A node's card position (top-left, zoom 1).
#[derive(Clone, Debug, PartialEq)]
pub struct NodeBox {
    /// Node id.
    pub id: u32,
    /// Parent node id (`None` for the root).
    pub parent: Option<u32>,
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
}

/// The whole graph's layout.
#[derive(Clone, Debug, Default)]
pub struct GraphLayout {
    /// Every node, pre-order.
    pub boxes: Vec<NodeBox>,
    index: HashMap<u32, usize>,
    /// Extent of all cards.
    pub width: f32,
    /// Extent of all cards.
    pub height: f32,
    /// Largest row count flowing along any edge (for edge widths).
    pub max_rows: f64,
}

impl GraphLayout {
    /// The card of node `id`.
    pub fn get(&self, id: u32) -> Option<&NodeBox> {
        self.index.get(&id).map(|&i| &self.boxes[i])
    }
}

/// Rows a node hands to its parent, across all executions.
pub fn flow_rows(n: &PlanNode) -> Option<f64> {
    n.actual_rows_total()
        .or_else(|| Some(n.estimated_rows? * n.loops.unwrap_or(1.0).max(1.0)))
}

/// Lay out the plan left to right: each leaf takes the next row, a parent sits centred on
/// its inputs.
pub fn graph(plan: &Plan) -> GraphLayout {
    let mut out = GraphLayout::default();
    let mut next_y = 0.0_f32;
    // Post-order without recursion (plans can be deep): (node, depth, parent, visited).
    let mut stack: Vec<(&PlanNode, u32, Option<u32>, bool)> = vec![(&plan.root, 0, None, false)];
    let mut centre: HashMap<u32, f32> = HashMap::new();
    let mut placed: Vec<NodeBox> = Vec::new();
    while let Some((n, depth, parent, visited)) = stack.pop() {
        if !visited {
            stack.push((n, depth, parent, true));
            for c in n.children.iter().rev() {
                stack.push((c, depth + 1, Some(n.id), false));
            }
            continue;
        }
        let y = if n.children.is_empty() {
            let y = next_y;
            next_y += NODE_H + ROW_GAP;
            y
        } else {
            let first = centre.get(&n.children[0].id).copied().unwrap_or(0.0);
            let last = n
                .children
                .last()
                .and_then(|c| centre.get(&c.id))
                .copied()
                .unwrap_or(first);
            (first + last) / 2.0
        };
        centre.insert(n.id, y);
        if let Some(r) = flow_rows(n) {
            out.max_rows = out.max_rows.max(r);
        }
        placed.push(NodeBox {
            id: n.id,
            parent,
            x: depth as f32 * (NODE_W + COL_GAP),
            y,
        });
    }
    // Pre-order, so the root comes first and paints first.
    let order: HashMap<u32, usize> = plan
        .nodes()
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id, i))
        .collect();
    placed.sort_by_key(|b| order.get(&b.id).copied().unwrap_or(usize::MAX));
    for b in &placed {
        out.width = out.width.max(b.x + NODE_W);
        out.height = out.height.max(b.y + NODE_H);
    }
    out.index = placed.iter().enumerate().map(|(i, b)| (b.id, i)).collect();
    out.boxes = placed;
    out
}

/// Edge stroke width for `rows` when the busiest edge carries `max` (log scale, 1–8 px).
pub fn edge_width(rows: Option<f64>, max: f64) -> f32 {
    match rows {
        Some(r) if max > 0.0 => {
            (1.0 + 7.0 * ((1.0 + r.max(0.0)).ln() / (1.0 + max).ln()).clamp(0.0, 1.0)) as f32
        }
        _ => 1.0,
    }
}

/// How hot a node is by its share of the plan's time (or cost).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Heat {
    /// Under 5%.
    None,
    /// 5–15%.
    Low,
    /// 15–40%.
    Mid,
    /// 40% and up.
    High,
}

impl Heat {
    /// Bucket a 0–1 share.
    pub fn of(share: f64) -> Self {
        match share {
            s if s >= 0.40 => Heat::High,
            s if s >= 0.15 => Heat::Mid,
            s if s >= 0.05 => Heat::Low,
            _ => Heat::None,
        }
    }
}

/// One bar of the flame (icicle) view: a node's span as fractions of the full width.
#[derive(Clone, Debug, PartialEq)]
pub struct FlameBar {
    /// Node id.
    pub id: u32,
    /// Row (root is 0).
    pub depth: u32,
    /// Left edge, 0–1.
    pub x0: f64,
    /// Right edge, 0–1.
    pub x1: f64,
}

/// Flame bars: each node is as wide as its inclusive time (or cost, for estimated plans);
/// inputs share their parent's span, and whatever they leave is the parent's own work.
pub fn flame(plan: &Plan) -> Vec<FlameBar> {
    let timed = plan.root.total_time_ms.is_some();
    let weight = |n: &PlanNode| -> f64 {
        let w = if timed { n.total_time_ms } else { n.cost };
        w.unwrap_or(0.0).max(0.0)
    };
    let mut out = Vec::new();
    let mut stack: Vec<(&PlanNode, u32, f64, f64)> = vec![(&plan.root, 0, 0.0, 1.0)];
    while let Some((n, depth, x0, x1)) = stack.pop() {
        out.push(FlameBar {
            id: n.id,
            depth,
            x0,
            x1,
        });
        let kids: f64 = n.children.iter().map(weight).sum();
        let denom = weight(n).max(kids);
        if denom <= 0.0 {
            // Nothing measured: split evenly so the shape still shows.
            let k = n.children.len().max(1) as f64;
            for (i, c) in n.children.iter().enumerate().rev() {
                let w = (x1 - x0) / k;
                stack.push((c, depth + 1, x0 + w * i as f64, x0 + w * (i + 1) as f64));
            }
            continue;
        }
        let scale = (x1 - x0) / denom;
        let mut x = x0;
        let mut spans = Vec::new();
        for c in &n.children {
            let w = weight(c) * scale;
            spans.push((c, x, x + w));
            x += w;
        }
        for (c, a, b) in spans.into_iter().rev() {
            stack.push((c, depth + 1, a, b));
        }
    }
    out.sort_by_key(|b| (b.depth, b.id));
    out
}

/// Names a node refers to in SQL: the table (and alias or CTE name), without schema and
/// index parts that cannot appear in the statement anyway.
fn names(node: &PlanNode) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(o) = &node.object {
        for seg in o.split('.') {
            let s = seg.trim_matches(|c| matches!(c, '"' | '[' | ']' | '`'));
            if !s.is_empty() {
                out.push(s.to_owned());
            }
        }
    }
    for key in ["Alias", "CTE Name"] {
        if let Some(v) = node.details.get(key) {
            out.push(v.clone());
        }
    }
    out.sort();
    out.dedup();
    out
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Byte ranges in `sql` where the node's table, alias or CTE name appears as a whole
/// identifier (case-insensitive; quoted forms included).
pub fn sql_ranges(sql: &str, node: &PlanNode) -> Vec<Range<usize>> {
    let hay = sql.to_ascii_lowercase();
    let bytes = hay.as_bytes();
    let mut out = Vec::new();
    for name in names(node) {
        let needle = name.to_ascii_lowercase();
        if needle.is_empty() {
            continue;
        }
        let mut from = 0;
        while let Some(pos) = hay[from..].find(&needle) {
            let start = from + pos;
            let end = start + needle.len();
            let before = start.checked_sub(1).map(|i| bytes[i]);
            let after = bytes.get(end).copied();
            if !before.is_some_and(is_ident) && !after.is_some_and(is_ident) {
                // Take the quotes along when the name is quoted.
                let quoted = matches!(
                    (before, after),
                    (Some(b'"'), Some(b'"')) | (Some(b'['), Some(b']')) | (Some(b'`'), Some(b'`'))
                );
                out.push(if quoted {
                    start - 1..end + 1
                } else {
                    start..end
                });
            }
            from = end;
        }
    }
    out.sort_by_key(|r| r.start);
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::plan::{PlanKind, PlanSource};

    fn n(op: &str, children: Vec<PlanNode>) -> PlanNode {
        PlanNode {
            children,
            ..PlanNode::op(op)
        }
    }

    fn plan(root: PlanNode) -> Plan {
        Plan::new(PlanSource::Postgres, PlanKind::Actual, "q", root)
    }

    #[test]
    fn graph_centres_parents_on_their_inputs() {
        let p = plan(n(
            "Hash Join",
            vec![
                n("Seq Scan", vec![]),
                n("Hash", vec![n("Seq Scan", vec![])]),
            ],
        ));
        let g = graph(&p);
        assert_eq!(g.boxes.len(), 4);
        assert_eq!(g.boxes[0].id, 0, "root first");
        let (root, a, hash, b) = (
            g.get(0).unwrap(),
            g.get(1).unwrap(),
            g.get(2).unwrap(),
            g.get(3).unwrap(),
        );
        assert_eq!(root.x, 0.0);
        assert_eq!(a.x, NODE_W + COL_GAP);
        assert_eq!(b.x, 2.0 * (NODE_W + COL_GAP));
        assert_eq!(a.y, 0.0);
        assert_eq!(b.y, NODE_H + ROW_GAP);
        assert_eq!(hash.y, b.y, "single input: same row");
        assert_eq!(root.y, (a.y + hash.y) / 2.0);
        assert_eq!(hash.parent, Some(0));
        assert_eq!(g.height, b.y + NODE_H);
    }

    #[test]
    fn graph_of_200_nodes_has_no_overlaps() {
        // A wide and deep tree: a chain of joins, each with a scan on its right.
        let mut root = n("Seq Scan", vec![]);
        for _ in 0..99 {
            root = n("Nested Loop", vec![root, n("Index Scan", vec![])]);
        }
        let p = plan(root);
        assert_eq!(p.nodes().len(), 199);
        let g = graph(&p);
        assert_eq!(g.boxes.len(), 199);
        let mut seen = std::collections::HashSet::new();
        for b in &g.boxes {
            assert!(
                seen.insert((b.x as i64, b.y as i64)),
                "two cards at one spot"
            );
        }
        // Leaves sit on distinct rows.
        let leaves = g
            .boxes
            .iter()
            .filter(|b| p.node(b.id).is_some_and(|n| n.children.is_empty()))
            .count();
        assert_eq!(g.height, leaves as f32 * (NODE_H + ROW_GAP) - ROW_GAP);
    }

    #[test]
    fn flame_spans_follow_time() {
        let mut p = plan(PlanNode {
            total_time_ms: Some(100.0),
            children: vec![
                PlanNode {
                    total_time_ms: Some(60.0),
                    ..PlanNode::op("Seq Scan")
                },
                PlanNode {
                    total_time_ms: Some(20.0),
                    ..PlanNode::op("Index Scan")
                },
            ],
            ..PlanNode::op("Hash Join")
        });
        p.renumber();
        let bars = flame(&p);
        assert_eq!(
            bars[0],
            FlameBar {
                id: 0,
                depth: 0,
                x0: 0.0,
                x1: 1.0
            }
        );
        assert!((bars[1].x1 - 0.6).abs() < 1e-9);
        assert!((bars[2].x0 - 0.6).abs() < 1e-9 && (bars[2].x1 - 0.8).abs() < 1e-9);
    }

    #[test]
    fn flame_children_never_outgrow_parent() {
        // Parallel workers: inputs report more time than their parent.
        let p = plan(PlanNode {
            total_time_ms: Some(10.0),
            children: vec![
                PlanNode {
                    total_time_ms: Some(15.0),
                    ..PlanNode::op("A")
                },
                PlanNode {
                    total_time_ms: Some(5.0),
                    ..PlanNode::op("B")
                },
            ],
            ..PlanNode::op("Gather")
        });
        let bars = flame(&p);
        let last = bars.iter().map(|b| b.x1).fold(0.0, f64::max);
        assert!((last - 1.0).abs() < 1e-9);
        // Estimated plans with no cost split evenly.
        let p = Plan::new(
            PlanSource::Postgres,
            PlanKind::Estimated,
            "q",
            n("Append", vec![n("A", vec![]), n("B", vec![])]),
        );
        let bars = flame(&p);
        assert!((bars[1].x1 - 0.5).abs() < 1e-9);
    }

    #[test]
    fn edge_widths_and_heat() {
        assert_eq!(edge_width(None, 100.0), 1.0);
        assert_eq!(edge_width(Some(100.0), 100.0), 8.0);
        assert!(edge_width(Some(10.0), 1e6) < edge_width(Some(1e4), 1e6));
        assert_eq!(Heat::of(0.01), Heat::None);
        assert_eq!(Heat::of(0.1), Heat::Low);
        assert_eq!(Heat::of(0.2), Heat::Mid);
        assert_eq!(Heat::of(0.73), Heat::High);
    }

    #[test]
    fn sql_ranges_find_tables_and_aliases() {
        let sql = "SELECT * FROM public.orders o JOIN \"Customers\" c ON o.customer_id = c.id \
                   WHERE o.total > 990 -- orders_total";
        let mut node = PlanNode {
            object: Some("orders.orders_total_idx".into()),
            ..PlanNode::op("Index Scan")
        };
        node.details.insert("Alias".into(), "o".into());
        let got: Vec<&str> = sql_ranges(sql, &node)
            .iter()
            .map(|r| &sql[r.clone()])
            .collect();
        assert_eq!(got, ["orders", "o", "o", "o"]);
        let node = PlanNode {
            object: Some("dbo.Customers".into()),
            ..PlanNode::op("Clustered Index Scan")
        };
        let got: Vec<&str> = sql_ranges(sql, &node)
            .iter()
            .map(|r| &sql[r.clone()])
            .collect();
        assert_eq!(got, ["\"Customers\""]);
        assert!(sql_ranges(sql, &PlanNode::op("Hash")).is_empty());
    }
}
