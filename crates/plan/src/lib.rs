//! Query plans (milestone M5): one normalized [`PlanNode`] tree for PostgreSQL JSON plans
//! and SQL Server showplan XML. Findings rules and the plan UI only ever see `PlanNode`.

use serde::{Deserialize, Serialize};

/// One operator in a plan tree.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanNode {
    /// Stable id within the plan.
    pub id: u32,
    /// Operation (`Seq Scan`, `Hash Join`, ...).
    pub operation: String,
    /// Object touched (table or index), if any.
    pub object: Option<String>,
    /// Estimated rows.
    pub estimated_rows: Option<f64>,
    /// Actual rows per loop.
    pub actual_rows: Option<f64>,
    /// Loops.
    pub loops: Option<f64>,
    /// Total time including children, ms (all loops).
    pub total_time_ms: Option<f64>,
    /// Children.
    pub children: Vec<PlanNode>,
}

impl PlanNode {
    /// Time spent in this node alone: total minus children's totals, never negative.
    pub fn self_time_ms(&self) -> Option<f64> {
        let total = self.total_time_ms?;
        let kids: f64 = self.children.iter().filter_map(|c| c.total_time_ms).sum();
        Some((total - kids).max(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_time() {
        let n = PlanNode {
            total_time_ms: Some(10.0),
            children: vec![
                PlanNode {
                    total_time_ms: Some(4.0),
                    ..Default::default()
                },
                PlanNode {
                    total_time_ms: Some(3.5),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(n.self_time_ms(), Some(2.5));
    }
}
