//! Streaming result events.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::batch::{ColumnMeta, RowBatch};
use crate::error::Result;

/// A server notice or informational message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    /// Severity (`NOTICE`, `WARNING`, `INFO`, ...).
    pub severity: String,
    /// SQLSTATE or message number, when available.
    pub code: Option<String>,
    /// Message text.
    pub message: String,
}

/// How a statement finished.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Completion {
    /// Rows affected by DML, when the server reports it.
    pub affected: Option<u64>,
    /// Wall-clock time from send to completion.
    pub elapsed: Duration,
}

/// One event in a query's result stream.
#[derive(Clone, Debug)]
pub enum ResultEvent {
    /// Column metadata for the current result set; always precedes its rows.
    Columns(Arc<[ColumnMeta]>),
    /// A batch of rows (typically 500–1,000).
    Rows(RowBatch),
    /// A server notice.
    Notice(Notice),
    /// The current result set ended and another follows.
    NextResultSet,
    /// The statement finished.
    Done(Completion),
}

/// A stream of result events for one executed statement or batch.
pub type ResultStream = BoxStream<'static, Result<ResultEvent>>;

/// Default number of rows per [`RowBatch`].
pub const DEFAULT_BATCH_ROWS: usize = 1000;
