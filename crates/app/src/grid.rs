//! Results grid: a `TableDelegate` over columnar batches. Rows and columns are virtualized
//! by the table; cells are formatted only when they are visible.

use std::sync::Arc;

use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Context, FontWeight, IntoElement, ParentElement as _, Pixels, SharedString, Styled as _,
    Window, div, px,
};
use switchyard_core::db::{BatchList, CellRef, ColumnMeta, DataType, RowBatch};

use crate::theme::{MONO, palette};
use crate::ui::thousands;

/// Characters shown per cell before truncating.
const CELL_PREVIEW_CHARS: usize = 120;

/// Grid data and view state for one result set.
pub struct GridDelegate {
    columns: Arc<[ColumnMeta]>,
    widths: Vec<Pixels>,
    data: BatchList,
    /// Sorted and/or filtered row order; `None` means natural order.
    view: Option<Arc<Vec<u32>>>,
    /// Staged edits: (data row, col) → new display text (`None` = NULL).
    staged: std::collections::HashMap<(usize, usize), Option<SharedString>>,
    /// Display order: position → data column (columns can be dragged).
    order: Vec<usize>,
    /// Selected rectangle: (view row, table column) corners, either order.
    range: Option<((usize, usize), (usize, usize))>,
}

/// Most cells one copy may take.
pub const MAX_COPY_CELLS: usize = 1_000_000;

fn initial_width(meta: &ColumnMeta) -> f32 {
    let name = meta.name.chars().count() as f32 * 7.5 + meta.type_name.len() as f32 * 6.0 + 40.0;
    let by_type: f32 = match meta.data_type {
        DataType::Bool => 64.0,
        DataType::Int16 | DataType::Int32 => 80.0,
        DataType::Int64 | DataType::Float32 | DataType::Float64 => 96.0,
        DataType::Numeric => 104.0,
        DataType::Date | DataType::Time => 104.0,
        DataType::Timestamp | DataType::TimestampTz => 184.0,
        DataType::Uuid => 290.0,
        DataType::Interval => 150.0,
        _ => 180.0,
    };
    name.max(by_type).clamp(56.0, 360.0)
}

impl GridDelegate {
    /// A grid for these columns.
    pub fn new(columns: Arc<[ColumnMeta]>) -> Self {
        let widths = columns.iter().map(|c| px(initial_width(c))).collect();
        let order = (0..columns.len()).collect();
        Self {
            columns,
            widths,
            data: BatchList::default(),
            view: None,
            staged: Default::default(),
            order,
            range: None,
        }
    }

    /// Width of the pinned row-number column; it grows with the row count.
    pub fn row_number_width(&self) -> Pixels {
        let digits = thousands(self.data.len().max(1) as u64).len() as f32;
        px((digits * 7.5 + 22.0).max(44.0))
    }

    /// The data column shown at table column `table_col` (0 is the row-number column).
    pub fn data_col(&self, table_col: usize) -> Option<usize> {
        table_col
            .checked_sub(1)
            .and_then(|i| self.order.get(i).copied())
    }

    /// Select the rectangle between two (view row, table column) corners, or clear it.
    pub fn set_range(&mut self, range: Option<((usize, usize), (usize, usize))>) {
        self.range = range;
    }

    /// The selected rectangle as view rows and table columns (row-number column excluded).
    pub fn range(&self) -> Option<(std::ops::Range<usize>, std::ops::Range<usize>)> {
        let ((r0, c0), (r1, c1)) = self.range?;
        let rows = r0.min(r1)..r0.max(r1) + 1;
        let cols = c0.min(c1).max(1)..c0.max(c1).max(1) + 1;
        let rows = rows.start.min(self.visible_rows())..rows.end.min(self.visible_rows());
        let cols = cols.start..cols.end.min(self.columns.len() + 1);
        (!rows.is_empty() && !cols.is_empty()).then_some((rows, cols))
    }

    fn in_range(&self, row: usize, table_col: usize) -> bool {
        self.range.is_some_and(|((r0, c0), (r1, c1))| {
            (r0.min(r1)..=r0.max(r1)).contains(&row)
                && (c0.min(c1).max(1)..=c0.max(c1)).contains(&table_col)
        })
    }

    /// Cells of `rows` × `cols` (table columns) as text, tab-separated, one line per row;
    /// NULL is empty and tabs or line breaks inside values become spaces.
    pub fn range_tsv(&self, rows: std::ops::Range<usize>, cols: std::ops::Range<usize>) -> String {
        let mut out = String::new();
        let mut buf = String::new();
        for (i, r) in rows.enumerate() {
            if i > 0 {
                out.push('\n');
            }
            for (j, c) in cols.clone().enumerate() {
                if j > 0 {
                    out.push('\t');
                }
                buf.clear();
                if let Some(cell) = self.data_col(c).and_then(|d| self.cell(r, d))
                    && cell != CellRef::Null
                {
                    cell.write_display(&mut buf, 0);
                }
                out.extend(buf.chars().map(|ch| {
                    if ch == '\t' || ch == '\n' || ch == '\r' {
                        ' '
                    } else {
                        ch
                    }
                }));
            }
        }
        out
    }

    /// The loaded rows.
    pub fn data(&self) -> &BatchList {
        &self.data
    }

    /// Append a batch; widen text columns from the first rows seen.
    pub fn push(&mut self, batch: RowBatch) {
        if self.data.is_empty() {
            for (c, w) in self.widths.iter_mut().enumerate() {
                let dt = self.columns[c].data_type;
                if matches!(dt, DataType::Text | DataType::Other | DataType::Json)
                    || dt.is_numeric()
                {
                    let mut longest = 0usize;
                    let mut buf = String::new();
                    for r in 0..batch.len().min(50) {
                        buf.clear();
                        let cell = batch.cell(r, c);
                        if dt.is_numeric() {
                            format_number(cell, &mut buf);
                        } else if let CellRef::Text(s) = cell {
                            buf.extend(s.chars().take(60));
                        }
                        longest = longest.max(buf.chars().count());
                    }
                    let want = (longest as f32 * 7.4 + 24.0).clamp(80.0, 360.0);
                    if want > f32::from(*w) {
                        *w = px(want);
                    }
                }
            }
        }
        self.data.push(batch);
        if let Some(v) = &self.view {
            // New rows join the end of a sorted/filtered view unordered; resorting is
            // explicit (click the header again).
            let mut v2 = (**v).clone();
            v2.extend((v.len() as u32)..(self.data.len() as u32));
            self.view = Some(Arc::new(v2));
        }
    }

    /// Rows currently visible (after filter).
    pub fn visible_rows(&self) -> usize {
        self.view.as_ref().map_or(self.data.len(), |v| v.len())
    }

    /// Map a view row to a data row.
    pub fn data_row(&self, view_row: usize) -> usize {
        self.view
            .as_ref()
            .map_or(view_row, |v| v.get(view_row).copied().unwrap_or(0) as usize)
    }

    /// Borrow a cell by view row and data column.
    pub fn cell(&self, view_row: usize, col: usize) -> Option<CellRef<'_>> {
        self.data.cell(self.data_row(view_row), col)
    }

    /// Show a staged value for a cell (`None` displays NULL).
    pub fn stage(&mut self, data_row: usize, col: usize, value: Option<SharedString>) {
        self.staged.insert((data_row, col), value);
    }

    /// Drop every staged value.
    pub fn clear_staged(&mut self) {
        self.staged.clear();
    }

    /// Apply a client-side filter: keep rows where any cell contains `needle`.
    pub fn set_filter(&mut self, needle: &str) {
        if needle.is_empty() {
            self.view = None;
            return;
        }
        let n = needle.to_lowercase();
        let mut keep = Vec::new();
        let mut buf = String::new();
        for r in 0..self.data.len() {
            let hit = (0..self.columns.len()).any(|c| {
                buf.clear();
                if let Some(cell) = self.data.cell(r, c) {
                    cell.write_display(&mut buf, 0);
                }
                buf.to_lowercase().contains(&n)
            });
            if hit {
                keep.push(r as u32);
            }
        }
        self.view = Some(Arc::new(keep));
    }

    fn sort_by(&mut self, col: usize, sort: ColumnSort) {
        let mut order: Vec<u32> = match &self.view {
            Some(v) => (**v).clone(),
            None => (0..self.data.len() as u32).collect(),
        };
        if sort == ColumnSort::Default {
            order.sort_unstable();
        } else {
            let data = &self.data;
            order.sort_by(|a, b| {
                let ca = data.cell(*a as usize, col).unwrap_or(CellRef::Null);
                let cb = data.cell(*b as usize, col).unwrap_or(CellRef::Null);
                let o = compare(ca, cb);
                if sort == ColumnSort::Descending {
                    o.reverse()
                } else {
                    o
                }
            });
        }
        self.view = Some(Arc::new(order));
    }
}

/// Total order for cells: NULLs last, numbers numerically, everything else as text.
pub fn compare(a: CellRef<'_>, b: CellRef<'_>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (CellRef::Null, CellRef::Null) => Ordering::Equal,
        (CellRef::Null, _) => Ordering::Greater,
        (_, CellRef::Null) => Ordering::Less,
        (CellRef::Int(x), CellRef::Int(y)) => x.cmp(&y),
        (CellRef::Float(x), CellRef::Float(y)) => x.total_cmp(&y),
        (CellRef::Int(x), CellRef::Float(y)) => (x as f64).total_cmp(&y),
        (CellRef::Float(x), CellRef::Int(y)) => x.total_cmp(&(y as f64)),
        (CellRef::Bool(x), CellRef::Bool(y)) => x.cmp(&y),
        (CellRef::Date(x), CellRef::Date(y)) => x.cmp(&y),
        (CellRef::Time(x), CellRef::Time(y))
        | (CellRef::Timestamp(x), CellRef::Timestamp(y))
        | (CellRef::TimestampTz(x), CellRef::TimestampTz(y)) => x.cmp(&y),
        (CellRef::Text(x), CellRef::Text(y)) => {
            // Numeric text (NUMERIC columns) compares as numbers when both parse.
            match (x.parse::<f64>(), y.parse::<f64>()) {
                (Ok(p), Ok(q)) => p.total_cmp(&q),
                _ => x.cmp(y),
            }
        }
        (x, y) => x.to_display().cmp(&y.to_display()),
    }
}

impl TableDelegate for GridDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len() + 1
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.visible_rows()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> Column {
        if col_ix == 0 {
            return Column::new("#", "#")
                .width(self.row_number_width())
                .text_right()
                .fixed_left()
                .resizable(false)
                .movable(false)
                .selectable(false);
        }
        let data = self.order[col_ix - 1];
        let meta = &self.columns[data];
        let mut c = Column::new(SharedString::from(format!("c{data}")), meta.name.clone())
            .width(self.widths[col_ix - 1])
            .min_width(px(48.))
            .resizable(true)
            .movable(true)
            .sortable();
        if meta.data_type.is_numeric() {
            c = c.text_right();
        }
        c
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        if let Some(col) = self.data_col(col_ix) {
            self.sort_by(col, sort);
        }
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let p = palette(cx);
        if col_ix == 0 {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_end()
                .text_color(p.fg2)
                .text_size(px(11.5))
                .font_weight(FontWeight::SEMIBOLD)
                .child("#");
        }
        let meta = &self.columns[self.order[col_ix - 1]];
        div()
            .size_full()
            .flex()
            .items_center()
            .gap(px(6.))
            .when(meta.data_type.is_numeric(), |d| d.justify_end())
            .whitespace_nowrap()
            .overflow_hidden()
            .child(
                div()
                    .text_color(p.fg2)
                    .text_size(px(11.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(meta.name.clone()),
            )
            .child(
                div()
                    .font_family(MONO)
                    .text_size(px(10.))
                    .text_color(p.fg3)
                    .child(meta.type_name.clone()),
            )
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let p = palette(cx);
        let base = div()
            .size_full()
            .flex()
            .items_center()
            .font_family(MONO)
            .text_size(px(12.))
            .whitespace_nowrap()
            .overflow_hidden();
        if col_ix == 0 {
            return base
                .justify_end()
                .text_color(p.fg3)
                .child(SharedString::from((row_ix + 1).to_string()));
        }
        let col = self.order[col_ix - 1];
        let base = if self.in_range(row_ix, col_ix) {
            base.bg(p.sel)
        } else {
            base
        };
        let numeric = self.columns[col].data_type.is_numeric();
        let data_row = self.data_row(row_ix);
        if let Some(staged) = self.staged.get(&(data_row, col)) {
            let base = base.bg(p.staged).border_l_2().border_color(p.stg);
            return match staged {
                None => base.italic().text_color(p.fg3).child("NULL"),
                Some(v) => base
                    .when(numeric, |d| d.justify_end())
                    .text_color(p.fg)
                    .child(v.clone()),
            };
        }
        match self.data.cell(data_row, col) {
            None | Some(CellRef::Null) => base.italic().text_color(p.fg3).child("NULL"),
            Some(cell) => {
                let mut s = String::new();
                if numeric {
                    format_number(cell, &mut s);
                } else {
                    cell.write_display(&mut s, CELL_PREVIEW_CHARS);
                }
                base.when(numeric, |d| d.justify_end())
                    .text_color(p.fg)
                    .child(SharedString::from(s))
            }
        }
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let p = palette(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(40.))
            .gap(px(4.))
            .text_color(p.fg2)
            .text_size(px(13.))
            .child("Query returned no rows")
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _cx: &App) -> String {
        if col_ix == 0 {
            return (row_ix + 1).to_string();
        }
        self.data_col(col_ix)
            .and_then(|c| self.cell(row_ix, c))
            .map(|c| c.to_display())
            .unwrap_or_default()
    }

    fn move_column(
        &mut self,
        col_ix: usize,
        to_ix: usize,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) {
        // The table moves its header; the delegate maps positions to data columns.
        if col_ix == 0 || to_ix == 0 || col_ix > self.order.len() {
            return;
        }
        let to = (to_ix - 1).min(self.order.len() - 1);
        let w = self.widths.remove(col_ix - 1);
        self.widths.insert(to, w);
        let c = self.order.remove(col_ix - 1);
        self.order.insert(to, c);
        self.range = None;
    }
}

/// Numbers with thousands separators for integers; decimals keep their scale.
fn format_number(cell: CellRef<'_>, out: &mut String) {
    match cell {
        CellRef::Int(i) => {
            if i < 0 {
                out.push('-');
            }
            out.push_str(&thousands(i.unsigned_abs()));
        }
        CellRef::Text(t) => {
            let (sign, body) = t.strip_prefix('-').map_or(("", t), |b| ("-", b));
            let (int, frac) = body.split_once('.').unwrap_or((body, ""));
            match int.parse::<u128>() {
                Ok(n) => {
                    out.push_str(sign);
                    out.push_str(&thousands(n));
                    if !frac.is_empty() {
                        out.push('.');
                        out.push_str(frac);
                    }
                }
                Err(_) => out.push_str(t),
            }
        }
        other => other.write_display(out, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::db::RowBatchBuilder;

    fn delegate() -> GridDelegate {
        let cols: Arc<[ColumnMeta]> = Arc::from(vec![
            ColumnMeta::new("id", "int8", DataType::Int64),
            ColumnMeta::new("name", "text", DataType::Text),
        ]);
        let mut g = GridDelegate::new(cols.clone());
        let mut b = RowBatchBuilder::for_columns(&cols, 4);
        for (i, n) in [(3, "c"), (1, "a"), (2, "b")] {
            b.push_i64(i);
            b.push_str(n);
        }
        b.push_i64(4);
        b.push_null();
        g.push(b.finish());
        g
    }

    #[test]
    fn sort_and_filter() {
        let mut g = delegate();
        g.sort_by(0, ColumnSort::Ascending);
        let ids: Vec<_> = (0..4).map(|r| g.cell(r, 0).unwrap().to_display()).collect();
        assert_eq!(ids, ["1", "2", "3", "4"]);
        g.sort_by(1, ColumnSort::Descending);
        // NULL sorts last ascending, so first descending.
        assert_eq!(g.cell(0, 1), Some(CellRef::Null));
        g.set_filter("b");
        assert_eq!(g.visible_rows(), 1);
        g.set_filter("");
        assert_eq!(g.visible_rows(), 4);
    }

    #[test]
    fn ranges_copy_as_tsv_in_display_order() {
        let mut g = delegate();
        // Row 3 has a NULL name.
        g.set_range(Some(((3, 2), (0, 1))));
        let (rows, cols) = g.range().unwrap();
        assert_eq!((rows.clone(), cols.clone()), (0..4, 1..3));
        assert_eq!(g.range_tsv(rows, cols), "3\tc\n1\ta\n2\tb\n4\t");
        // Move "name" in front of "id": the copy follows the screen.
        g.order = vec![1, 0];
        assert_eq!(g.data_col(1), Some(1));
        assert_eq!(g.range_tsv(0..2, 1..3), "c\t3\na\t1");
        // The row-number column and out-of-range corners are clipped.
        g.set_range(Some(((0, 0), (9, 9))));
        assert_eq!(g.range(), Some((0..4, 1..3)));
        assert_eq!(g.data_col(0), None);
    }

    #[test]
    fn number_formatting() {
        let mut s = String::new();
        format_number(CellRef::Text("4812.40"), &mut s);
        assert_eq!(s, "4,812.40");
        s.clear();
        format_number(CellRef::Int(-1204871), &mut s);
        assert_eq!(s, "-1,204,871");
    }
}
