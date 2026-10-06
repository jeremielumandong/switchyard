//! Query plans (milestone M5): one normalized [`PlanNode`] tree for PostgreSQL JSON plans
//! and SQL Server showplan XML. Findings rules and the plan UI only ever see `PlanNode`.
//!
//! * [`model`]: the tree and plan metadata.
//! * [`pg`], [`mssql`]: engine output → [`Plan`].
//! * [`capture`]: run `EXPLAIN` / showplan on a session (actual plans are rolled back).
//! * [`findings`]: ranked rules over a plan.
//! * [`compare`]: totals and per-operator deltas between two plans.

pub mod capture;
pub mod compare;
pub mod findings;
pub mod model;
pub mod mssql;
pub mod pg;

pub use compare::{Comparison, NodeDelta, Pair, compare};
pub use findings::{Finding, Rule, Severity, Thresholds, analyze};
pub use model::{Io, MissingIndex, Plan, PlanKind, PlanNode, PlanSource, Predicate};

/// Plan errors.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// The engine's plan output could not be read.
    #[error("could not read the plan: {0}")]
    Parse(String),
    /// Plans are not available for this engine or statement.
    #[error("{0}")]
    Unsupported(String),
    /// The database reported an error.
    #[error(transparent)]
    Db(#[from] switchyard_db::error::DbError),
    /// Rolling back after an actual plan failed: the session's state is unknown.
    #[error("the plan ran but rolling back failed: {0}")]
    Rollback(String),
}

/// Result alias.
pub type Result<T> = std::result::Result<T, PlanError>;
