//! Columnar result batches.
//!
//! A [`RowBatch`] stores one typed buffer per column. Strings and byte values live in a
//! per-column arena with an offsets vector, so decoding a batch never allocates per cell.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::value::{self, DataType, Value};

/// [`ColumnMeta::type_name`] of a JSON column that holds each whole source row as one
/// document (MongoDB). Row viewers show that document instead of rebuilding an object
/// from the other columns.
pub const DOCUMENT_TYPE: &str = "document";

/// Name of the [`DOCUMENT_TYPE`] column.
pub const DOCUMENT_COLUMN: &str = "(document)";

/// Metadata for one result column.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColumnMeta {
    /// Column name as returned by the server.
    pub name: String,
    /// Engine type name (`int8`, `nvarchar`, ...).
    pub type_name: String,
    /// Logical type, which decides the column buffer.
    pub data_type: DataType,
    /// Source table identifier (engine-specific, e.g. PostgreSQL table OID), when known.
    pub table_id: Option<u32>,
    /// Source column number within the table, when known.
    pub table_column: Option<i16>,
}

impl ColumnMeta {
    /// A column with only name and type.
    pub fn new(name: impl Into<String>, type_name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            type_name: type_name.into(),
            data_type,
            table_id: None,
            table_column: None,
        }
    }
}

/// Variable-length values stored back to back.
pub struct Arena<T: ArenaData + ?Sized> {
    offsets: Vec<u32>,
    data: T::Owned,
}

impl<T: ArenaData + ?Sized> Clone for Arena<T> {
    fn clone(&self) -> Self {
        Self {
            offsets: self.offsets.clone(),
            data: self.data.clone(),
        }
    }
}

impl<T: ArenaData + ?Sized> std::fmt::Debug for Arena<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Arena")
            .field("len", &(self.offsets.len().saturating_sub(1)))
            .finish()
    }
}

/// Storage trait for [`Arena`] (`str` or `[u8]`).
pub trait ArenaData {
    /// Owned buffer type.
    type Owned: Default + Clone + std::fmt::Debug;
    /// Current length of the buffer.
    fn len(buf: &Self::Owned) -> usize;
    /// Append a value.
    fn push(buf: &mut Self::Owned, v: &Self);
    /// Borrow a range.
    fn slice(buf: &Self::Owned, start: usize, end: usize) -> &Self;
}

impl ArenaData for str {
    type Owned = String;
    fn len(buf: &String) -> usize {
        buf.len()
    }
    fn push(buf: &mut String, v: &str) {
        buf.push_str(v)
    }
    fn slice(buf: &String, start: usize, end: usize) -> &str {
        &buf[start..end]
    }
}

impl ArenaData for [u8] {
    type Owned = Vec<u8>;
    fn len(buf: &Vec<u8>) -> usize {
        buf.len()
    }
    fn push(buf: &mut Vec<u8>, v: &[u8]) {
        buf.extend_from_slice(v)
    }
    fn slice(buf: &Vec<u8>, start: usize, end: usize) -> &[u8] {
        &buf[start..end]
    }
}

impl<T: ArenaData + ?Sized> Arena<T> {
    fn with_capacity(rows: usize) -> Self {
        let mut offsets = Vec::with_capacity(rows + 1);
        offsets.push(0);
        Self {
            offsets,
            data: T::Owned::default(),
        }
    }

    fn push(&mut self, v: &T) {
        T::push(&mut self.data, v);
        self.offsets.push(T::len(&self.data) as u32);
    }

    fn get(&self, row: usize) -> &T {
        let start = self.offsets[row] as usize;
        let end = self.offsets[row + 1] as usize;
        T::slice(&self.data, start, end)
    }

    fn heap_bytes(&self) -> usize {
        self.offsets.capacity() * 4 + T::len(&self.data)
    }
}

/// Typed buffer for one column.
#[derive(Clone, Debug)]
pub enum ColumnData {
    /// Booleans.
    Bool(Vec<bool>),
    /// 16-bit integers.
    Int16(Vec<i16>),
    /// 32-bit integers.
    Int32(Vec<i32>),
    /// 64-bit integers.
    Int64(Vec<i64>),
    /// 32-bit floats.
    Float32(Vec<f32>),
    /// 64-bit floats.
    Float64(Vec<f64>),
    /// Days since 1970-01-01.
    Date(Vec<i32>),
    /// Microseconds since midnight.
    Time(Vec<i64>),
    /// Microseconds since epoch, no zone.
    Timestamp(Vec<i64>),
    /// Microseconds since epoch, UTC.
    TimestampTz(Vec<i64>),
    /// UUIDs.
    Uuid(Vec<[u8; 16]>),
    /// Text (also numeric, json, xml, interval and fallback types).
    Text(Arena<str>),
    /// Binary.
    Bytes(Arena<[u8]>),
}

impl ColumnData {
    fn for_type(t: DataType, rows: usize) -> Self {
        match t {
            DataType::Bool => ColumnData::Bool(Vec::with_capacity(rows)),
            DataType::Int16 => ColumnData::Int16(Vec::with_capacity(rows)),
            DataType::Int32 => ColumnData::Int32(Vec::with_capacity(rows)),
            DataType::Int64 => ColumnData::Int64(Vec::with_capacity(rows)),
            DataType::Float32 => ColumnData::Float32(Vec::with_capacity(rows)),
            DataType::Float64 => ColumnData::Float64(Vec::with_capacity(rows)),
            DataType::Date => ColumnData::Date(Vec::with_capacity(rows)),
            DataType::Time => ColumnData::Time(Vec::with_capacity(rows)),
            DataType::Timestamp => ColumnData::Timestamp(Vec::with_capacity(rows)),
            DataType::TimestampTz => ColumnData::TimestampTz(Vec::with_capacity(rows)),
            DataType::Uuid => ColumnData::Uuid(Vec::with_capacity(rows)),
            DataType::Bytes => ColumnData::Bytes(Arena::with_capacity(rows)),
            DataType::Numeric
            | DataType::Text
            | DataType::Json
            | DataType::Xml
            | DataType::Interval
            | DataType::Other => ColumnData::Text(Arena::with_capacity(rows)),
        }
    }

    fn push_default(&mut self) {
        match self {
            ColumnData::Bool(v) => v.push(false),
            ColumnData::Int16(v) => v.push(0),
            ColumnData::Int32(v) | ColumnData::Date(v) => v.push(0),
            ColumnData::Int64(v)
            | ColumnData::Time(v)
            | ColumnData::Timestamp(v)
            | ColumnData::TimestampTz(v) => v.push(0),
            ColumnData::Float32(v) => v.push(0.0),
            ColumnData::Float64(v) => v.push(0.0),
            ColumnData::Uuid(v) => v.push([0; 16]),
            ColumnData::Text(a) => a.push(""),
            ColumnData::Bytes(a) => a.push(&[]),
        }
    }

    fn heap_bytes(&self) -> usize {
        match self {
            ColumnData::Bool(v) => v.capacity(),
            ColumnData::Int16(v) => v.capacity() * 2,
            ColumnData::Int32(v) | ColumnData::Date(v) => v.capacity() * 4,
            ColumnData::Float32(v) => v.capacity() * 4,
            ColumnData::Int64(v)
            | ColumnData::Time(v)
            | ColumnData::Timestamp(v)
            | ColumnData::TimestampTz(v) => v.capacity() * 8,
            ColumnData::Float64(v) => v.capacity() * 8,
            ColumnData::Uuid(v) => v.capacity() * 16,
            ColumnData::Text(a) => a.heap_bytes(),
            ColumnData::Bytes(a) => a.heap_bytes(),
        }
    }
}

/// One column: typed values plus a validity bitmap (bit set = NULL).
#[derive(Clone, Debug)]
pub struct Column {
    data: ColumnData,
    nulls: Vec<u64>,
    data_type: DataType,
}

impl Column {
    fn new(data_type: DataType, rows: usize) -> Self {
        Self {
            data: ColumnData::for_type(data_type, rows),
            nulls: Vec::with_capacity(rows.div_ceil(64)),
            data_type,
        }
    }

    /// Logical type.
    pub fn data_type(&self) -> DataType {
        self.data_type
    }

    /// Raw typed buffer.
    pub fn data(&self) -> &ColumnData {
        &self.data
    }

    /// Whether `row` is NULL.
    pub fn is_null(&self, row: usize) -> bool {
        self.nulls
            .get(row / 64)
            .is_some_and(|w| w & (1u64 << (row % 64)) != 0)
    }

    fn set_null_bit(&mut self, row: usize) {
        let word = row / 64;
        if self.nulls.len() <= word {
            self.nulls.resize(word + 1, 0);
        }
        self.nulls[word] |= 1u64 << (row % 64);
    }

    /// Borrow the cell at `row`.
    pub fn get(&self, row: usize) -> CellRef<'_> {
        if self.is_null(row) {
            return CellRef::Null;
        }
        match &self.data {
            ColumnData::Bool(v) => CellRef::Bool(v[row]),
            ColumnData::Int16(v) => CellRef::Int(v[row] as i64),
            ColumnData::Int32(v) => CellRef::Int(v[row] as i64),
            ColumnData::Int64(v) => CellRef::Int(v[row]),
            ColumnData::Float32(v) => CellRef::Float(v[row] as f64),
            ColumnData::Float64(v) => CellRef::Float(v[row]),
            ColumnData::Date(v) => CellRef::Date(v[row]),
            ColumnData::Time(v) => CellRef::Time(v[row]),
            ColumnData::Timestamp(v) => CellRef::Timestamp(v[row]),
            ColumnData::TimestampTz(v) => CellRef::TimestampTz(v[row]),
            ColumnData::Uuid(v) => CellRef::Uuid(&v[row]),
            ColumnData::Text(a) => CellRef::Text(a.get(row)),
            ColumnData::Bytes(a) => CellRef::Bytes(a.get(row)),
        }
    }
}

/// A borrowed cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CellRef<'a> {
    /// NULL.
    Null,
    /// Boolean.
    Bool(bool),
    /// Integer of any width.
    Int(i64),
    /// Float of any width.
    Float(f64),
    /// Days since epoch.
    Date(i32),
    /// Microseconds since midnight.
    Time(i64),
    /// Microseconds since epoch.
    Timestamp(i64),
    /// Microseconds since epoch, UTC.
    TimestampTz(i64),
    /// UUID.
    Uuid(&'a [u8; 16]),
    /// Text-backed value.
    Text(&'a str),
    /// Binary value.
    Bytes(&'a [u8]),
}

impl CellRef<'_> {
    /// Whether this is NULL.
    pub fn is_null(&self) -> bool {
        matches!(self, CellRef::Null)
    }

    /// Append display text, truncated to roughly `max_chars` characters (0 = no limit).
    pub fn write_display(&self, out: &mut String, max_chars: usize) {
        let start = out.len();
        match *self {
            CellRef::Null => out.push_str("NULL"),
            CellRef::Bool(b) => out.push_str(if b { "true" } else { "false" }),
            CellRef::Int(i) => {
                use std::fmt::Write as _;
                let _ = write!(out, "{i}");
            }
            CellRef::Float(f) => value::write_float(out, f),
            CellRef::Date(d) => value::write_date(out, d),
            CellRef::Time(t) => value::write_time(out, t),
            CellRef::Timestamp(t) => value::write_timestamp(out, t, false),
            CellRef::TimestampTz(t) => value::write_timestamp(out, t, true),
            CellRef::Uuid(u) => value::write_uuid(out, u),
            CellRef::Text(s) => {
                if max_chars > 0 {
                    // Single-line preview: stop at the first newline or the limit.
                    for (n, ch) in s.chars().enumerate() {
                        if n >= max_chars {
                            out.push('…');
                            break;
                        }
                        out.push(if ch == '\n' || ch == '\r' || ch == '\t' {
                            ' '
                        } else {
                            ch
                        });
                    }
                } else {
                    out.push_str(s)
                }
            }
            CellRef::Bytes(b) => {
                let limit = if max_chars > 0 {
                    (max_chars / 2).max(1)
                } else {
                    b.len()
                };
                value::write_hex(out, &b[..b.len().min(limit)]);
                if b.len() > limit {
                    out.push('…');
                }
            }
        }
        debug_assert!(out.len() >= start);
    }

    /// Display text with no truncation.
    pub fn to_display(&self) -> String {
        let mut s = String::new();
        self.write_display(&mut s, 0);
        s
    }

    /// Convert to an owned [`Value`], using `data_type` to pick the text-backed variant.
    pub fn to_value(&self, data_type: DataType) -> Value {
        match *self {
            CellRef::Null => Value::Null,
            CellRef::Bool(b) => Value::Bool(b),
            CellRef::Int(i) => Value::Int(i),
            CellRef::Float(f) => Value::Float(f),
            CellRef::Date(d) => Value::Date(d),
            CellRef::Time(t) => Value::Time(t),
            CellRef::Timestamp(t) => Value::Timestamp(t),
            CellRef::TimestampTz(t) => Value::TimestampTz(t),
            CellRef::Uuid(u) => Value::Uuid(*u),
            CellRef::Bytes(b) => Value::Bytes(b.to_vec()),
            CellRef::Text(s) => match data_type {
                DataType::Numeric => Value::Numeric(s.to_owned()),
                DataType::Json => Value::Json(s.to_owned()),
                DataType::Text => Value::Text(s.to_owned()),
                _ => Value::Other(s.to_owned()),
            },
        }
    }
}

/// A batch of rows stored column by column.
#[derive(Clone, Debug)]
pub struct RowBatch {
    columns: Vec<Column>,
    len: usize,
}

impl RowBatch {
    /// Number of rows.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the batch has no rows.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of columns.
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Borrow a column.
    pub fn column(&self, ix: usize) -> &Column {
        &self.columns[ix]
    }

    /// Borrow one cell.
    pub fn cell(&self, row: usize, col: usize) -> CellRef<'_> {
        self.columns[col].get(row)
    }

    /// Approximate heap usage in bytes.
    pub fn heap_bytes(&self) -> usize {
        self.columns
            .iter()
            .map(|c| c.data.heap_bytes() + c.nulls.capacity() * 8)
            .sum()
    }
}

/// Builds a [`RowBatch`] one cell at a time, row-major.
#[derive(Debug)]
pub struct RowBatchBuilder {
    columns: Vec<Column>,
    len: usize,
    col: usize,
    capacity: usize,
}

impl RowBatchBuilder {
    /// A builder for columns of the given types, sized for `rows` rows.
    pub fn new(types: &[DataType], rows: usize) -> Self {
        Self {
            columns: types.iter().map(|t| Column::new(*t, rows)).collect(),
            len: 0,
            col: 0,
            capacity: rows,
        }
    }

    /// A builder matching `meta`.
    pub fn for_columns(meta: &[ColumnMeta], rows: usize) -> Self {
        let types: Vec<DataType> = meta.iter().map(|m| m.data_type).collect();
        Self::new(&types, rows)
    }

    /// Rows completed so far.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no row has been completed.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the builder reached the capacity it was created with.
    pub fn is_full(&self) -> bool {
        self.len >= self.capacity
    }

    fn current(&mut self) -> &mut Column {
        let ix = self.col;
        self.col += 1;
        if self.col == self.columns.len() {
            self.col = 0;
            self.len += 1;
        }
        &mut self.columns[ix]
    }

    /// Append NULL to the next column.
    pub fn push_null(&mut self) {
        let row = self.len;
        let c = self.current();
        c.data.push_default();
        c.set_null_bit(row);
    }

    /// Append a boolean (column must be `Bool`, else stored as text).
    pub fn push_bool(&mut self, v: bool) {
        let c = self.current();
        match &mut c.data {
            ColumnData::Bool(b) => b.push(v),
            ColumnData::Text(a) => a.push(if v { "true" } else { "false" }),
            other => other.push_default(),
        }
    }

    /// Append an integer.
    pub fn push_i64(&mut self, v: i64) {
        let c = self.current();
        match &mut c.data {
            ColumnData::Int16(b) => b.push(v as i16),
            ColumnData::Int32(b) | ColumnData::Date(b) => b.push(v as i32),
            ColumnData::Int64(b)
            | ColumnData::Time(b)
            | ColumnData::Timestamp(b)
            | ColumnData::TimestampTz(b) => b.push(v),
            ColumnData::Float64(b) => b.push(v as f64),
            ColumnData::Text(a) => a.push(&v.to_string()),
            other => other.push_default(),
        }
    }

    /// Append a float.
    pub fn push_f64(&mut self, v: f64) {
        let c = self.current();
        match &mut c.data {
            ColumnData::Float32(b) => b.push(v as f32),
            ColumnData::Float64(b) => b.push(v),
            ColumnData::Text(a) => {
                let mut s = String::new();
                value::write_float(&mut s, v);
                a.push(&s)
            }
            other => other.push_default(),
        }
    }

    /// Append a UUID.
    pub fn push_uuid(&mut self, v: [u8; 16]) {
        let c = self.current();
        match &mut c.data {
            ColumnData::Uuid(b) => b.push(v),
            other => other.push_default(),
        }
    }

    /// Append text.
    pub fn push_str(&mut self, v: &str) {
        let c = self.current();
        match &mut c.data {
            ColumnData::Text(a) => a.push(v),
            ColumnData::Bytes(a) => a.push(v.as_bytes()),
            other => other.push_default(),
        }
    }

    /// Append bytes.
    pub fn push_bytes(&mut self, v: &[u8]) {
        let c = self.current();
        match &mut c.data {
            ColumnData::Bytes(a) => a.push(v),
            ColumnData::Text(a) => a.push(&String::from_utf8_lossy(v)),
            other => other.push_default(),
        }
    }

    /// Append an owned value, converting as needed.
    pub fn push_value(&mut self, v: &Value) {
        match v {
            Value::Null => self.push_null(),
            Value::Bool(b) => self.push_bool(*b),
            Value::Int(i) => self.push_i64(*i),
            Value::Float(f) => self.push_f64(*f),
            Value::Date(d) => self.push_i64(*d as i64),
            Value::Time(t) | Value::Timestamp(t) | Value::TimestampTz(t) => self.push_i64(*t),
            Value::Uuid(u) => self.push_uuid(*u),
            Value::Bytes(b) => self.push_bytes(b),
            Value::Numeric(s) | Value::Text(s) | Value::Json(s) | Value::Other(s) => {
                self.push_str(s)
            }
        }
    }

    /// Finish the batch. Panics in debug builds if a row is incomplete.
    pub fn finish(self) -> RowBatch {
        debug_assert_eq!(self.col, 0, "incomplete row");
        RowBatch {
            columns: self.columns,
            len: self.len,
        }
    }

    /// Finish the current batch and start a new one with the same column types.
    pub fn take(&mut self) -> RowBatch {
        let types: Vec<DataType> = self.columns.iter().map(|c| c.data_type).collect();
        let next = RowBatchBuilder::new(&types, self.capacity);
        std::mem::replace(self, next).finish()
    }
}

/// An append-only list of batches with O(log n) row lookup, used by result views.
#[derive(Clone, Debug, Default)]
pub struct BatchList {
    batches: Vec<Arc<RowBatch>>,
    starts: Vec<usize>,
    rows: usize,
}

impl BatchList {
    /// Append a batch.
    pub fn push(&mut self, batch: RowBatch) {
        if batch.is_empty() {
            return;
        }
        self.starts.push(self.rows);
        self.rows += batch.len();
        self.batches.push(Arc::new(batch));
    }

    /// Total rows.
    pub fn len(&self) -> usize {
        self.rows
    }

    /// Whether there are no rows.
    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// All batches.
    pub fn batches(&self) -> &[Arc<RowBatch>] {
        &self.batches
    }

    /// Locate a global row: (batch, row within batch).
    pub fn locate(&self, row: usize) -> Option<(&RowBatch, usize)> {
        if row >= self.rows {
            return None;
        }
        let b = match self.starts.binary_search(&row) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        Some((&self.batches[b], row - self.starts[b]))
    }

    /// Borrow one cell by global row.
    pub fn cell(&self, row: usize, col: usize) -> Option<CellRef<'_>> {
        self.locate(row).map(|(b, r)| b.cell(r, col))
    }

    /// Approximate heap usage in bytes.
    pub fn heap_bytes(&self) -> usize {
        self.batches.iter().map(|b| b.heap_bytes()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_read() {
        let mut b = RowBatchBuilder::new(&[DataType::Int64, DataType::Text, DataType::Bool], 4);
        b.push_i64(1);
        b.push_str("alpha");
        b.push_bool(true);
        b.push_null();
        b.push_null();
        b.push_bool(false);
        b.push_i64(3);
        b.push_str("");
        b.push_null();
        let batch = b.finish();
        assert_eq!(batch.len(), 3);
        assert_eq!(batch.cell(0, 0), CellRef::Int(1));
        assert_eq!(batch.cell(0, 1), CellRef::Text("alpha"));
        assert_eq!(batch.cell(1, 0), CellRef::Null);
        assert_eq!(batch.cell(1, 1), CellRef::Null);
        assert_eq!(batch.cell(2, 1), CellRef::Text(""));
        assert!(!batch.cell(2, 1).is_null(), "empty string is not NULL");
        assert_eq!(batch.cell(2, 2), CellRef::Null);
    }

    #[test]
    fn null_bitmap_crosses_words() {
        let mut b = RowBatchBuilder::new(&[DataType::Int32], 200);
        for i in 0..200 {
            if i % 3 == 0 {
                b.push_null()
            } else {
                b.push_i64(i)
            }
        }
        let batch = b.finish();
        for i in 0..200 {
            assert_eq!(batch.cell(i, 0).is_null(), i % 3 == 0, "row {i}");
        }
    }

    #[test]
    fn batch_list_locate() {
        let mut list = BatchList::default();
        for start in [0i64, 10, 25] {
            let n = if start == 10 { 15 } else { 10 };
            let mut b = RowBatchBuilder::new(&[DataType::Int64], n);
            for i in 0..n as i64 {
                b.push_i64(start + i);
            }
            list.push(b.finish());
        }
        assert_eq!(list.len(), 35);
        for r in 0..35 {
            assert_eq!(list.cell(r, 0), Some(CellRef::Int(r as i64)));
        }
        assert!(list.cell(35, 0).is_none());
    }

    #[test]
    fn million_rows_ten_numeric_columns_fit_the_memory_budget() {
        // Budget (SPEC): 1M rows × 10 numeric columns under 150 MB.
        let types = [DataType::Int64; 10];
        let mut list = BatchList::default();
        for b in 0..1000i64 {
            let mut builder = RowBatchBuilder::new(&types, 1000);
            for r in 0..1000i64 {
                for c in 0..10 {
                    builder.push_i64(b * 1000 + r * c);
                }
            }
            list.push(builder.finish());
        }
        assert_eq!(list.len(), 1_000_000);
        let mb = list.heap_bytes() as f64 / (1024.0 * 1024.0);
        assert!(mb < 150.0, "{mb:.1} MB");
        assert!(mb > 70.0, "accounting looks wrong: {mb:.1} MB");
    }

    #[test]
    fn display_truncates_text() {
        let mut out = String::new();
        CellRef::Text("line one\nline two").write_display(&mut out, 6);
        assert_eq!(out, "line o…");
    }
}
