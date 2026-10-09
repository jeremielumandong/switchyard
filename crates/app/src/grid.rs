//! Results grid: a `TableDelegate` over columnar batches. Rows and columns are virtualized
//! by the table; cells are formatted only when they are visible.
//!
//! Also the table data view's server-side filter / sort / paging bar ([`Pager`], DBX-3a),
//! shared by the SQL tab and the object properties tab.

use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, IntoElement, ParentElement as _,
    Pixels, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use switchyard_core::db::dialect::SortKey;
use switchyard_core::db::edit::{key_condition, page_order, validate_where};
use switchyard_core::db::{
    BatchList, CellRef, ColumnMeta, DataType, Dialect, ForeignKeyInfo, ObjectDetail, ObjectKind,
    RowBatch, Value,
};

use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind, thousands};

/// Data-row keys of staged new rows start here; they are not in the loaded batches.
pub const NEW_ROW: usize = usize::MAX / 2;

/// Called with the new server-side order, as (data column, descending), after a header
/// sort click on a data-view grid.
pub type SortCallback = Rc<dyn Fn(Vec<(usize, bool)>, &mut Window, &mut App)>;

/// Builds the right-click menu of a view row.
pub type MenuBuilder = Rc<dyn Fn(usize, PopupMenu, &mut Window, &mut App) -> PopupMenu>;

/// Header sorts go to the server instead of sorting the loaded rows.
struct ServerSort {
    order: Vec<(usize, bool)>,
    on_change: SortCallback,
}

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
    /// Staged new rows, shown after the loaded rows (data rows `NEW_ROW + i`).
    inserted: usize,
    /// Data rows staged for deletion (struck through).
    deleted: HashSet<usize>,
    server_sort: Option<ServerSort>,
    /// Data columns that are part of a foreign key (marked in the header).
    fk_cols: Vec<usize>,
    menu: Option<MenuBuilder>,
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
            inserted: 0,
            deleted: HashSet::new(),
            server_sort: None,
            fk_cols: Vec::new(),
            menu: None,
        }
    }

    /// Send header sorts to the server: `order` is the current one (data column,
    /// descending); `on_change` gets the next one.
    pub fn set_server_sort(&mut self, order: Vec<(usize, bool)>, on_change: SortCallback) {
        self.server_sort = Some(ServerSort { order, on_change });
    }

    /// Mark foreign-key columns in the header.
    pub fn set_fk_cols(&mut self, cols: Vec<usize>) {
        self.fk_cols = cols;
    }

    /// The right-click menu of a row.
    pub fn set_menu(&mut self, menu: MenuBuilder) {
        self.menu = Some(menu);
    }

    /// Show `n` staged new rows after the loaded ones.
    pub fn set_inserted(&mut self, n: usize) {
        self.inserted = n;
    }

    /// Strike through these data rows (staged deletes).
    pub fn set_deleted(&mut self, rows: impl IntoIterator<Item = usize>) {
        self.deleted = rows.into_iter().collect();
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

    /// The result's columns.
    pub fn columns(&self) -> Arc<[ColumnMeta]> {
        self.columns.clone()
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
        let loaded = self.data.len() as u32;
        self.data.push(batch);
        if let Some(v) = &mut self.view {
            // New rows join the end of a sorted/filtered view unordered; resorting is
            // explicit (click the header again). Appending in place keeps streaming into
            // a sorted grid linear instead of copying the whole view per batch.
            Arc::make_mut(v).extend(loaded..self.data.len() as u32);
        }
    }

    /// Rows currently visible (after filter).
    pub fn visible_rows(&self) -> usize {
        self.view.as_ref().map_or(self.data.len(), |v| v.len())
    }

    /// Map a view row to a data row; rows after the loaded ones are staged new rows
    /// (`NEW_ROW + i`).
    pub fn data_row(&self, view_row: usize) -> usize {
        let visible = self.visible_rows();
        if view_row >= visible {
            return NEW_ROW + (view_row - visible);
        }
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

    /// Drop every staged value, new row and delete.
    pub fn clear_staged(&mut self) {
        self.staged.clear();
        self.inserted = 0;
        self.deleted.clear();
    }

    /// Apply a client-side filter: keep rows where any cell contains `needle`.
    pub fn set_filter(&mut self, needle: &str) {
        if needle.is_empty() {
            self.view = None;
            return;
        }
        let keep = self.data.rows_containing(needle);
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
            self.data
                .sort_rows(&mut order, col, sort == ColumnSort::Descending);
        }
        self.view = Some(Arc::new(order));
    }
}

/// The server-side order after a header click on data column `col`: ascending, then
/// descending, then off. Without Shift the click replaces the order; with Shift it adds
/// or changes that column and keeps the others.
pub fn cycle_sort(order: &[(usize, bool)], col: usize, shift: bool) -> Vec<(usize, bool)> {
    let pos = order.iter().position(|(c, _)| *c == col);
    let next = match pos.map(|i| order[i].1) {
        None => Some(false),
        Some(false) => Some(true),
        Some(true) => None,
    };
    if !shift {
        return next.map(|d| vec![(col, d)]).unwrap_or_default();
    }
    let mut out = order.to_vec();
    match (pos, next) {
        (Some(i), Some(d)) => out[i].1 = d,
        (Some(i), None) => {
            out.remove(i);
        }
        (None, Some(d)) => out.push((col, d)),
        (None, None) => {}
    }
    out
}

/// Page sizes the data view offers.
pub const PAGE_SIZES: [u64; 3] = [100, 500, 1000];

/// Server-side filter, sort and paging of one table (DBX-3a): the state behind the bar
/// above a table data view. The filter is raw SQL, checked with `sqlparser` as one
/// expression before it is sent.
pub struct Pager {
    /// Schema.
    pub schema: String,
    /// Table or view.
    pub name: String,
    /// Its kind (for the detail request).
    pub kind: ObjectKind,
    /// Primary-key columns: the default order, so pages are stable.
    pub pk: Vec<String>,
    /// The table's foreign keys (for "Open referenced row").
    pub foreign_keys: Vec<ForeignKeyInfo>,
    /// The WHERE input.
    pub where_input: Entity<InputState>,
    /// The applied filter.
    pub filter: Option<String>,
    /// Columns the user sorted by (empty: the primary key).
    pub order: Vec<SortKey>,
    /// Rows per page.
    pub page_size: u64,
    /// Rows skipped.
    pub offset: u64,
    /// Rows on the current page.
    pub rows: usize,
    /// Why the filter was refused.
    pub error: Option<String>,
    /// The table detail (key, foreign keys) has arrived.
    pub ready: bool,
    /// The statement of the current page.
    pub last_sql: String,
}

impl Pager {
    /// A pager on `schema.name`, filtered by `filter` when given.
    pub fn new(
        schema: String,
        name: String,
        kind: ObjectKind,
        filter: Option<String>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let text = filter.clone().unwrap_or_default();
        let where_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("WHERE condition, e.g. status = 'open'  (Enter applies)")
                .default_value(text)
        });
        Self {
            schema,
            name,
            kind,
            pk: Vec::new(),
            foreign_keys: Vec::new(),
            where_input,
            filter: filter.filter(|f| !f.trim().is_empty()),
            order: Vec::new(),
            page_size: PAGE_SIZES[0],
            offset: 0,
            rows: 0,
            error: None,
            ready: false,
            last_sql: String::new(),
        }
    }

    /// Take the key and foreign keys from the table's detail.
    pub fn set_detail(&mut self, d: &ObjectDetail) {
        let mut pk: Vec<String> = d
            .columns
            .iter()
            .filter(|c| c.is_primary_key)
            .map(|c| c.name.clone())
            .collect();
        if pk.is_empty()
            && let Some(i) = d.indexes.iter().find(|i| i.is_primary)
        {
            pk = i.columns.clone();
        }
        self.pk = pk;
        self.foreign_keys = d.foreign_keys.clone();
        self.ready = true;
    }

    /// Whether `d` is this pager's table.
    pub fn is_for(&self, d: &ObjectDetail) -> bool {
        d.object.name.eq_ignore_ascii_case(&self.name)
            && d.object.schema.eq_ignore_ascii_case(&self.schema)
    }

    /// The statement for the current page.
    pub fn sql(&self, dialect: &dyn Dialect) -> String {
        dialect.select_page(
            &dialect.qualified(&self.schema, &self.name),
            &[],
            self.filter.as_deref(),
            &page_order(&self.order, &self.pk),
            self.page_size,
            self.offset,
        )
    }

    /// Validate and apply the WHERE input; the page goes back to the first.
    pub fn apply_filter(&mut self, dialect: &dyn Dialect, cx: &App) -> Result<(), String> {
        let text = self.where_input.read(cx).value().trim().to_owned();
        if text.is_empty() {
            self.filter = None;
        } else {
            validate_where(dialect, &text).map_err(|e| format!("Invalid condition: {e}"))?;
            self.filter = Some(text);
        }
        self.error = None;
        self.offset = 0;
        Ok(())
    }

    /// The server order as (data column, descending) of `columns`.
    pub fn sort_indexes(&self, columns: &[ColumnMeta]) -> Vec<(usize, bool)> {
        self.order
            .iter()
            .filter_map(|k| {
                columns
                    .iter()
                    .position(|c| c.name == k.column)
                    .map(|i| (i, k.descending))
            })
            .collect()
    }

    /// Sort by (data column, descending) of `columns`; the page goes back to the first.
    pub fn set_sort(&mut self, columns: &[ColumnMeta], order: &[(usize, bool)]) {
        self.order = order
            .iter()
            .filter_map(|&(i, descending)| {
                columns.get(i).map(|c| SortKey {
                    column: c.name.clone(),
                    descending,
                })
            })
            .collect();
        self.offset = 0;
    }

    /// Result columns that belong to a foreign key.
    pub fn fk_indexes(&self, columns: &[ColumnMeta]) -> Vec<usize> {
        columns
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                self.foreign_keys
                    .iter()
                    .any(|f| f.columns.contains(&c.name))
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Whether a next page may exist (this one is full).
    pub fn has_next(&self) -> bool {
        self.rows as u64 >= self.page_size
    }

    /// `Rows 101–200`, or `No rows`.
    pub fn range_label(&self) -> String {
        if self.rows == 0 {
            return if self.offset == 0 {
                "No rows".into()
            } else {
                format!("No rows after {}", thousands(self.offset))
            };
        }
        format!(
            "Rows {}–{}",
            thousands(self.offset + 1),
            thousands(self.offset + self.rows as u64)
        )
    }
}

/// Where "Open referenced row" goes from `column` (DBX-3c): the referenced table's
/// (schema, name) and a WHERE matching the key, values written as `dialect` literals.
/// `value_of` reads a column of the clicked row (`None` when the result lacks it); every
/// column of a multi-column foreign key is used.
pub fn reference_filter(
    dialect: &dyn Dialect,
    foreign_keys: &[ForeignKeyInfo],
    column: &str,
    default_schema: &str,
    value_of: impl Fn(&str) -> Option<Value>,
) -> Result<(String, String, String), String> {
    let fk = foreign_keys
        .iter()
        .find(|f| f.columns.iter().any(|c| c == column))
        .ok_or_else(|| format!("{column} is not part of a foreign key"))?;
    if fk.referenced_columns.len() != fk.columns.len() {
        return Err(format!("The referenced columns of {} are unknown", fk.name));
    }
    let mut key = Vec::with_capacity(fk.columns.len());
    for (c, rc) in fk.columns.iter().zip(&fk.referenced_columns) {
        let v = value_of(c)
            .ok_or_else(|| format!("Include {c} in the result to follow {}", fk.name))?;
        if v.is_null() {
            return Err(format!("{c} is NULL: there is no referenced row"));
        }
        key.push((rc.clone(), v));
    }
    let (schema, name) = crate::object_tab::split_reference(&fk.references, default_schema);
    Ok((schema, name, key_condition(dialect, &key)))
}

/// A view with a [`Pager`] bar.
pub trait PagedView: Sized + 'static {
    /// The pager, when the view shows a table's data.
    fn pager_mut(&mut self) -> Option<&mut Pager>;
    /// The dialect for validating filters.
    fn pager_dialect(&self) -> &'static dyn Dialect;
    /// Run the current page.
    fn reload_page(&mut self, window: &mut Window, cx: &mut Context<Self>);
}

/// What a bar control does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PagerAction {
    /// Validate and apply the WHERE input.
    Apply,
    /// Drop the filter.
    Clear,
    /// Back to the primary-key order.
    ClearSort,
    /// Rows per page.
    Size(u64),
    /// First page.
    First,
    /// Previous page.
    Prev,
    /// Next page.
    Next,
}

/// Apply a bar action and reload the page when something changed.
pub fn pager_action<T: PagedView>(
    this: &mut T,
    action: PagerAction,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let dialect = this.pager_dialect();
    let Some(pager) = this.pager_mut() else {
        return;
    };
    match action {
        PagerAction::Apply => {
            if let Err(e) = pager.apply_filter(dialect, cx) {
                pager.error = Some(e);
                cx.notify();
                return;
            }
        }
        PagerAction::Clear => {
            let input = pager.where_input.clone();
            input.update(cx, |i, cx| i.set_value("", window, cx));
            pager.filter = None;
            pager.error = None;
            pager.offset = 0;
        }
        PagerAction::ClearSort => {
            pager.order.clear();
            pager.offset = 0;
        }
        PagerAction::Size(n) => {
            pager.page_size = n;
            pager.offset = 0;
        }
        PagerAction::First => pager.offset = 0,
        PagerAction::Prev => pager.offset = pager.offset.saturating_sub(pager.page_size),
        PagerAction::Next => {
            if !pager.has_next() {
                return;
            }
            pager.offset += pager.page_size;
        }
    }
    this.reload_page(window, cx);
}

/// The filter / sort / paging bar.
pub fn render_pager<T: PagedView>(
    pager: &Pager,
    busy: bool,
    p: &Palette,
    cx: &mut Context<T>,
) -> AnyElement {
    let act = |id: &'static str, label: String, kind: Kind, enabled: bool, a: PagerAction| {
        ui::button(id, label, kind, p)
            .h(px(22.))
            .px(px(7.))
            .text_size(px(11.5))
            .when(!enabled, |d| d.opacity(0.4))
            .when(enabled, |d| {
                d.on_click(cx.listener(move |this: &mut T, _, w, cx| pager_action(this, a, w, cx)))
            })
    };
    let first = pager.offset > 0 && !busy;
    let next = pager.has_next() && !busy;
    let sort_label = if pager.order.is_empty() {
        if pager.pk.is_empty() {
            "Unsorted".to_owned()
        } else {
            format!("By key ({})", pager.pk.join(", "))
        }
    } else {
        let keys: Vec<String> = pager
            .order
            .iter()
            .map(|k| format!("{} {}", k.column, if k.descending { "↓" } else { "↑" }))
            .collect();
        format!("Sorted by {}", keys.join(", "))
    };
    let status = pager.error.clone();
    let mut bar = div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(4.))
        .px(px(10.))
        .py(px(5.))
        .border_b_1()
        .border_color(p.bd)
        .bg(p.panel)
        .text_size(px(11.5))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(
                    div()
                        .font_family(MONO)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.fg2)
                        .child("WHERE"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .px(px(6.))
                        .border_1()
                        .border_color(if status.is_some() { p.prod } else { p.bd })
                        .rounded(px(5.))
                        .bg(p.bg)
                        .font_family(MONO)
                        .child(
                            Input::new(&pager.where_input)
                                .appearance(false)
                                .text_size(px(11.5)),
                        ),
                )
                .child(act(
                    "pager-apply",
                    "Apply".into(),
                    Kind::Secondary,
                    !busy,
                    PagerAction::Apply,
                ))
                .when(pager.filter.is_some(), |d| {
                    d.child(act(
                        "pager-clear",
                        "Clear".into(),
                        Kind::Ghost,
                        !busy,
                        PagerAction::Clear,
                    ))
                }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(p.fg3)
                        .child(sort_label),
                )
                .when(!pager.order.is_empty(), |d| {
                    d.child(act(
                        "pager-unsort",
                        "Key order".into(),
                        Kind::Ghost,
                        !busy,
                        PagerAction::ClearSort,
                    ))
                })
                .child(div().text_color(p.fg3).child("Page size"))
                .children(PAGE_SIZES.into_iter().enumerate().map(|(i, n)| {
                    let on = pager.page_size == n;
                    ui::button(("pager-size", i), n.to_string(), Kind::Ghost, p)
                        .h(px(22.))
                        .px(px(6.))
                        .text_size(px(11.5))
                        .when(on, |d| d.bg(p.sel).text_color(p.fg))
                        .when(!on && !busy, |d| {
                            d.on_click(cx.listener(move |this: &mut T, _, w, cx| {
                                pager_action(this, PagerAction::Size(n), w, cx)
                            }))
                        })
                }))
                .child(
                    div()
                        .min_w(px(110.))
                        .text_color(p.fg2)
                        .font_family(MONO)
                        .flex()
                        .justify_center()
                        .child(if busy {
                            "Loading…".to_owned()
                        } else {
                            pager.range_label()
                        }),
                )
                .child(act(
                    "pager-first",
                    "⏮".into(),
                    Kind::Ghost,
                    first,
                    PagerAction::First,
                ))
                .child(act(
                    "pager-prev",
                    "‹ Prev".into(),
                    Kind::Secondary,
                    first,
                    PagerAction::Prev,
                ))
                .child(act(
                    "pager-next",
                    "Next ›".into(),
                    Kind::Secondary,
                    next,
                    PagerAction::Next,
                )),
        );
    if let Some(e) = status {
        bar = bar.child(div().text_color(p.prod).truncate().child(e));
    }
    bar.into_any_element()
}

impl TableDelegate for GridDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len() + 1
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.visible_rows() + self.inserted
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
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let Some(col) = self.data_col(col_ix) else {
            return;
        };
        if let Some(ss) = self.server_sort.as_mut() {
            let order = cycle_sort(&ss.order, col, window.modifiers().shift);
            ss.order = order.clone();
            let on_change = ss.on_change.clone();
            // The owner replaces this grid; let the table finish its click first.
            window.defer(cx, move |window, cx| on_change(order, window, cx));
            return;
        }
        self.sort_by(col, sort);
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        match &self.menu {
            Some(build) => build(row_ix, menu, window, cx),
            None => menu,
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
        let data = self.order[col_ix - 1];
        let meta = &self.columns[data];
        let sort_mark = self.server_sort.as_ref().and_then(|ss| {
            let i = ss.order.iter().position(|(c, _)| *c == data)?;
            let arrow = if ss.order[i].1 { "↓" } else { "↑" };
            Some(if ss.order.len() > 1 {
                format!("{arrow}{}", i + 1)
            } else {
                arrow.to_owned()
            })
        });
        let fk = self.fk_cols.contains(&data);
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
            .when(fk, |d| {
                d.child(
                    div()
                        .font_family(MONO)
                        .text_size(px(10.))
                        .text_color(p.acc)
                        .child("FK"),
                )
            })
            .child(
                div()
                    .font_family(MONO)
                    .text_size(px(10.))
                    .text_color(p.fg3)
                    .child(meta.type_name.clone()),
            )
            .children(sort_mark.map(|m| {
                div()
                    .font_family(MONO)
                    .text_size(px(10.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.acc)
                    .child(m)
            }))
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
        let data_row = self.data_row(row_ix);
        let new_row = data_row >= NEW_ROW;
        let deleted = self.deleted.contains(&data_row);
        if col_ix == 0 {
            return base
                .justify_end()
                .text_color(p.fg3)
                .when(new_row, |d| {
                    d.text_color(p.dev).font_weight(FontWeight::SEMIBOLD)
                })
                .when(deleted, |d| d.text_color(p.prod))
                .child(SharedString::from(if new_row {
                    "+".to_owned()
                } else {
                    (row_ix + 1).to_string()
                }));
        }
        let col = self.order[col_ix - 1];
        let base = if self.in_range(row_ix, col_ix) {
            base.bg(p.sel)
        } else if deleted {
            base.bg(p.prod_bg).line_through()
        } else if new_row {
            base.bg(p.dev_bg)
        } else {
            base
        };
        let numeric = self.columns[col].data_type.is_numeric();
        if new_row && !self.staged.contains_key(&(data_row, col)) {
            return base.italic().text_color(p.fg3).child("DEFAULT");
        }
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
            Some(cell) if deleted => {
                let mut s = String::new();
                cell.write_display(&mut s, CELL_PREVIEW_CHARS);
                base.text_color(p.fg3).child(SharedString::from(s))
            }
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
        let data_row = self.data_row(row_ix);
        if data_row >= NEW_ROW {
            return self
                .data_col(col_ix)
                .and_then(|c| self.staged.get(&(data_row, c)))
                .and_then(|v| v.as_ref().map(|v| v.to_string()))
                .unwrap_or_default();
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
    fn batches_streamed_into_a_view_join_its_end_once() {
        let mut g = delegate();
        g.set_filter("b");
        let mut b = RowBatchBuilder::for_columns(&g.columns(), 2);
        for (i, n) in [(5, "e"), (6, "f")] {
            b.push_i64(i);
            b.push_str(n);
        }
        g.push(b.finish());
        let ids: Vec<_> = (0..g.visible_rows())
            .map(|r| g.cell(r, 0).unwrap().to_display())
            .collect();
        assert_eq!(ids, ["2", "5", "6"]);
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
    fn header_sorts_cycle_and_shift_adds_columns() {
        assert_eq!(cycle_sort(&[], 2, false), [(2, false)]);
        assert_eq!(cycle_sort(&[(2, false)], 2, false), [(2, true)]);
        assert_eq!(cycle_sort(&[(2, true)], 2, false), []);
        // Without Shift another column replaces the order.
        assert_eq!(cycle_sort(&[(2, true)], 0, false), [(0, false)]);
        // With Shift it is added, changed or removed in place.
        assert_eq!(cycle_sort(&[(2, true)], 0, true), [(2, true), (0, false)]);
        assert_eq!(
            cycle_sort(&[(2, true), (0, false)], 0, true),
            [(2, true), (0, true)]
        );
        assert_eq!(cycle_sort(&[(2, true), (0, true)], 2, true), [(0, true)]);
    }

    #[test]
    fn staged_new_rows_follow_the_loaded_rows() {
        let mut g = delegate();
        g.set_inserted(2);
        assert_eq!(g.data_row(3), 3);
        assert_eq!(g.data_row(4), NEW_ROW);
        assert_eq!(g.data_row(5), NEW_ROW + 1);
        g.set_filter("b");
        assert_eq!(g.data_row(1), NEW_ROW);
        g.clear_staged();
        assert_eq!(g.inserted, 0);
    }

    #[test]
    fn references_filter_on_every_key_column() {
        use switchyard_core::db::Engine;
        use switchyard_core::db::dialect_for;
        let fks = vec![
            ForeignKeyInfo {
                name: "fk_store".into(),
                columns: vec!["customer_id".into(), "store".into()],
                references: "sales.customers".into(),
                referenced_columns: vec!["id".into(), "Store Code".into()],
                ..ForeignKeyInfo::default()
            },
            ForeignKeyInfo {
                name: "fk_rep".into(),
                columns: vec!["rep_id".into()],
                references: "staff".into(),
                referenced_columns: vec!["id".into()],
                ..ForeignKeyInfo::default()
            },
        ];
        let row = |c: &str| match c {
            "customer_id" => Some(Value::Int(42)),
            "store" => Some(Value::Text("N'1".into())),
            "rep_id" => Some(Value::Null),
            _ => None,
        };
        let pg = dialect_for(Engine::Postgres);
        assert_eq!(
            reference_filter(pg, &fks, "store", "public", row),
            Ok((
                "sales".into(),
                "customers".into(),
                "id = 42 AND \"Store Code\" = 'N''1'".into()
            ))
        );
        let ms = dialect_for(Engine::SqlServer);
        assert_eq!(
            reference_filter(ms, &fks, "customer_id", "dbo", row).map(|r| r.2),
            Ok("id = 42 AND [Store Code] = N'N''1'".into())
        );
        assert!(reference_filter(pg, &fks, "rep_id", "public", row).is_err());
        assert!(reference_filter(pg, &fks, "name", "public", row).is_err());
        assert!(reference_filter(pg, &fks, "store", "public", |_| None).is_err());
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
