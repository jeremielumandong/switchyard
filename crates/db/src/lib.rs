//! Database contracts shared by every engine: the [`Driver`] / [`DbSession`] / [`Dialect`]
//! traits, the [`Value`] type, columnar [`RowBatch`]es and [`ResultEvent`] streams.
//!
//! Engine-specific code lives in one module per driver (`pg`). Nothing outside a driver
//! module should branch on [`Engine`]; dialect differences go through [`Dialect`].

pub mod batch;
pub mod catalog;
pub mod complete;
pub mod diagnostics;
pub mod dialect;
pub mod driver;
pub mod error;
pub mod format;
pub mod guard;
pub mod mock;
pub mod pg;
pub mod stream;
pub mod value;

pub use batch::{BatchList, CellRef, Column, ColumnData, ColumnMeta, RowBatch, RowBatchBuilder};
pub use catalog::{
    CatalogChunk, ColumnInfo, ConstraintInfo, ForeignKeyInfo, IndexInfo, IntrospectScope,
    ObjectDetail, ObjectInfo, ObjectKind, SchemaInfo,
};
pub use dialect::{Dialect, ParamRef, ParamStyle, StatementSpan, dialect_for};
pub use driver::{
    CancelHandle, ComponentId, DbAuthMethod, DbConfig, DbSession, Driver, SslMode, TunnelEndpoint,
};
pub use error::{DbError, ErrorPosition, Result, ServerError};
pub use stream::{Completion, Notice, ResultEvent, ResultStream};
pub use value::{DataType, Engine, Value};
