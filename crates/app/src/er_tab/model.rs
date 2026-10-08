//! ER diagram model (DBX-5d): tables and foreign-key edges built from catalog details,
//! their layout (the `dagre` crate's layered layout plus a grid for unrelated tables) and
//! the end marks of each edge. Everything here is plain data, so it runs on a background
//! executor and is shared by the GPUI view and the SVG export.

use std::collections::{HashMap, HashSet};

use switchyard_core::db::{ObjectDetail, ObjectKind};

use crate::object_tab::split_reference;

/// Most tables one diagram shows; the rest are left out with a notice.
pub const MAX_TABLES: usize = 150;
/// Columns a table box lists before collapsing the rest into "+k more".
pub const MAX_COLUMNS: usize = 12;
/// Height of a table's name header.
pub const HEADER_H: f32 = 28.;
/// Height of one column row.
pub const ROW_H: f32 = 20.;
/// Narrowest and widest table box.
const MIN_W: f32 = 170.;
const MAX_W: f32 = 320.;
/// Estimated width of one character of the 11 px column text (monospace).
const CHAR_W: f32 = 6.7;
/// Horizontal run of an edge out of a box before it turns (the end marks sit on it).
pub const STUB: f32 = 18.;
/// Gap between unrelated tables in the grid under the graph.
const GRID_GAP: f32 = 30.;
/// Margin around the whole diagram.
pub const MARGIN: f32 = 20.;

/// A column row of a table box.
#[derive(Clone, Debug, PartialEq)]
pub struct ErColumn {
    /// Column name.
    pub name: String,
    /// Formatted type (empty for a stub).
    pub data_type: String,
    /// Part of the primary key (or a referenced column of a stub).
    pub pk: bool,
    /// Part of a foreign key.
    pub fk: bool,
    /// NULL allowed.
    pub nullable: bool,
}

/// A table box.
#[derive(Clone, Debug, PartialEq)]
pub struct ErTable {
    /// Schema.
    pub schema: String,
    /// Name.
    pub name: String,
    /// Kind (stubs are tables).
    pub kind: ObjectKind,
    /// Columns shown, in order.
    pub columns: Vec<ErColumn>,
    /// Columns collapsed into the "+k more" row.
    pub hidden: usize,
    /// A table outside the selection that a foreign key references: drawn dashed with
    /// only the referenced columns.
    pub stub: bool,
}

impl ErTable {
    /// Rows below the header (columns plus the "+k more" row).
    pub fn rows(&self) -> usize {
        self.columns.len() + usize::from(self.hidden > 0)
    }

    /// Box height.
    pub fn height(&self) -> f32 {
        HEADER_H + self.rows() as f32 * ROW_H + 4.
    }

    /// Box width estimated from the text (the view truncates what does not fit).
    pub fn width(&self) -> f32 {
        let header = (self.name.chars().count() as f32 + 4.) * 7.2;
        let rows = self
            .columns
            .iter()
            .map(|c| (c.name.chars().count() + c.data_type.chars().count() + 7) as f32 * CHAR_W)
            .fold(0., f32::max);
        header.max(rows).clamp(MIN_W, MAX_W)
    }

    /// The row a column sits on: its own row, the "+k more" row when it is collapsed, or
    /// `None` when the table has no such column (the edge then meets the header).
    pub fn row_of(&self, column: &str) -> Option<usize> {
        match self.columns.iter().position(|c| c.name == column) {
            Some(i) => Some(i),
            None if self.hidden > 0 && !self.stub => Some(self.columns.len()),
            None => None,
        }
    }

    /// `schema.name`.
    #[cfg(test)]
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

/// A foreign key: from the referencing (child) table to the referenced (parent) one.
#[derive(Clone, Debug, PartialEq)]
pub struct ErEdge {
    /// Constraint name.
    pub name: String,
    /// Referencing table (index into [`ErGraph::tables`]).
    pub from: usize,
    /// Referenced table.
    pub to: usize,
    /// Referencing columns.
    pub from_columns: Vec<String>,
    /// Referenced columns.
    pub to_columns: Vec<String>,
    /// Row of the first referencing column in `from` (`None`: the header).
    pub from_row: Option<usize>,
    /// Row of the first referenced column in `to` (`None`: the header).
    pub to_row: Option<usize>,
}

/// Tables and foreign keys of one diagram.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ErGraph {
    /// Selected tables first (in the given order), then stubs.
    pub tables: Vec<ErTable>,
    /// Foreign keys, in table then constraint order.
    pub edges: Vec<ErEdge>,
}

impl ErGraph {
    /// Index of the table `schema.name`.
    pub fn find(&self, schema: &str, name: &str) -> Option<usize> {
        self.tables
            .iter()
            .position(|t| t.schema == schema && t.name == name)
    }
}

/// Build the graph of `details` (the selected tables). A foreign key to a table outside
/// the selection gets a stub box. Each table lists at most `max_columns` columns.
pub fn build_graph(details: &[&ObjectDetail], max_columns: usize) -> ErGraph {
    let mut tables: Vec<ErTable> = Vec::with_capacity(details.len());
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    for d in details {
        let fk_cols: HashSet<&str> = d
            .foreign_keys
            .iter()
            .flat_map(|f| f.columns.iter().map(String::as_str))
            .collect();
        let mut cols: Vec<_> = d.columns.iter().collect();
        cols.sort_by_key(|c| c.ordinal);
        let hidden = cols.len().saturating_sub(max_columns);
        let columns = cols
            .into_iter()
            .take(max_columns)
            .map(|c| ErColumn {
                name: c.name.clone(),
                data_type: c.data_type.clone(),
                pk: c.is_primary_key,
                fk: fk_cols.contains(c.name.as_str()),
                nullable: c.nullable,
            })
            .collect();
        let key = (d.object.schema.clone(), d.object.name.clone());
        if index.contains_key(&key) {
            continue;
        }
        index.insert(key, tables.len());
        tables.push(ErTable {
            schema: d.object.schema.clone(),
            name: d.object.name.clone(),
            kind: d.object.kind,
            columns,
            hidden,
            stub: false,
        });
    }
    let mut edges = Vec::new();
    for d in details {
        let Some(&from) = index.get(&(d.object.schema.clone(), d.object.name.clone())) else {
            continue;
        };
        for fk in &d.foreign_keys {
            let key = split_reference(&fk.references, &d.object.schema);
            let to = match index.get(&key) {
                Some(&i) => i,
                None => {
                    let i = tables.len();
                    tables.push(ErTable {
                        schema: key.0.clone(),
                        name: key.1.clone(),
                        kind: ObjectKind::Table,
                        columns: Vec::new(),
                        hidden: 0,
                        stub: true,
                    });
                    index.insert(key, i);
                    i
                }
            };
            if tables[to].stub {
                for c in &fk.referenced_columns {
                    if !tables[to].columns.iter().any(|x| &x.name == c) {
                        tables[to].columns.push(ErColumn {
                            name: c.clone(),
                            data_type: String::new(),
                            pk: true,
                            fk: false,
                            nullable: false,
                        });
                    }
                }
            }
            edges.push(ErEdge {
                name: fk.name.clone(),
                from,
                to,
                from_columns: fk.columns.clone(),
                to_columns: fk.referenced_columns.clone(),
                from_row: None,
                to_row: None,
            });
        }
    }
    // Rows are known once the stubs have all their referenced columns.
    for e in &mut edges {
        e.from_row = e
            .from_columns
            .first()
            .and_then(|c| tables[e.from].row_of(c));
        e.to_row = match e.to_columns.first() {
            Some(c) => tables[e.to].row_of(c),
            None => tables[e.to].columns.iter().position(|c| c.pk),
        };
    }
    ErGraph { tables, edges }
}

/// Names of `focus` and the tables it references or is referenced by, from the details
/// loaded so far (only tables in `schema`).
pub fn related_names<'a>(
    focus: &str,
    schema: &str,
    details: impl Iterator<Item = &'a ObjectDetail>,
) -> HashSet<String> {
    let mut out = HashSet::from([focus.to_owned()]);
    for d in details {
        for fk in &d.foreign_keys {
            let (s, n) = split_reference(&fk.references, &d.object.schema);
            if d.object.name == focus && d.object.schema == schema && s == schema {
                out.insert(n);
            } else if n == focus && s == schema && d.object.schema == schema {
                out.insert(d.object.name.clone());
            }
        }
    }
    out
}

/// The tables a diagram shows: the names matching `filter` (case-insensitive substring)
/// and, when given, in `related`; at most `cap`. Returns them with the number that matched.
pub fn select_tables(
    all: &[String],
    filter: &str,
    related: Option<&HashSet<String>>,
    cap: usize,
) -> (Vec<String>, usize) {
    let needle = filter.trim().to_lowercase();
    let matched: Vec<String> = all
        .iter()
        .filter(|n| needle.is_empty() || n.to_lowercase().contains(&needle))
        .filter(|n| related.is_none_or(|r| r.contains(*n)))
        .cloned()
        .collect();
    let total = matched.len();
    (matched.into_iter().take(cap).collect(), total)
}

/// A table box's position and size.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    /// Left.
    pub x: f32,
    /// Top.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

/// Positions of a graph's boxes and the polyline of each edge, in diagram units.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ErLayout {
    /// One per table, same order as [`ErGraph::tables`].
    pub boxes: Vec<Rect>,
    /// One per edge, from the referenced (parent) end to the referencing (child) end.
    /// The first and last segments are horizontal runs of [`STUB`].
    pub routes: Vec<Vec<(f32, f32)>>,
    /// Diagram width.
    pub width: f32,
    /// Diagram height.
    pub height: f32,
}

/// Lay out `graph`: connected tables left to right with referenced tables first (dagre),
/// unrelated ones in a grid underneath.
pub fn layout(graph: &ErGraph) -> ErLayout {
    use dagre::graph::{Graph, GraphOptions};
    use dagre::{EdgeLabel, LayoutOptions, NodeLabel, RankDir};

    let n = graph.tables.len();
    let sizes: Vec<(f32, f32)> = graph
        .tables
        .iter()
        .map(|t| (t.width(), t.height()))
        .collect();
    let mut linked = vec![false; n];
    for e in graph.edges.iter().filter(|e| e.from != e.to) {
        linked[e.from] = true;
        linked[e.to] = true;
    }
    let mut boxes = vec![Rect::default(); n];
    let mut g: Graph<NodeLabel, EdgeLabel> = Graph::with_options(GraphOptions {
        directed: true,
        multigraph: true,
        compound: false,
    });
    for (i, &(w, h)) in sizes.iter().enumerate().filter(|(i, _)| linked[*i]) {
        g.set_node(
            format!("t{i}"),
            Some(NodeLabel {
                width: f64::from(w),
                height: f64::from(h),
                ..NodeLabel::default()
            }),
        );
    }
    for (i, e) in graph
        .edges
        .iter()
        .enumerate()
        .filter(|(_, e)| e.from != e.to)
    {
        g.set_edge(
            format!("t{}", e.to),
            format!("t{}", e.from),
            Some(EdgeLabel::default()),
            Some(&format!("e{i}")),
        );
    }
    let (mut width, mut height) = (0f32, 0f32);
    if linked.iter().any(|l| *l) {
        dagre::layout(
            &mut g,
            Some(LayoutOptions {
                rankdir: RankDir::LR,
                nodesep: 28.,
                edgesep: 14.,
                ranksep: 90.,
                marginx: f64::from(MARGIN),
                marginy: f64::from(MARGIN),
                ..LayoutOptions::default()
            }),
        );
        for (i, &(w, h)) in sizes.iter().enumerate().filter(|(i, _)| linked[*i]) {
            let (cx, cy) = g
                .node(&format!("t{i}"))
                .map_or((0., 0.), |l| (l.x.unwrap_or(0.), l.y.unwrap_or(0.)));
            let (x, y) = (cx as f32 - w / 2., cy as f32 - h / 2.);
            boxes[i] = Rect { x, y, w, h };
            width = width.max(x + w);
            height = height.max(y + h);
        }
    }
    // Unrelated tables: rows under the graph, wrapping at the graph's width (or a sane
    // width when there is no graph).
    let wrap = width.max(1200.);
    let (mut x, mut y, mut row_h) = (
        MARGIN,
        if height > 0. {
            height + GRID_GAP
        } else {
            MARGIN
        },
        0f32,
    );
    for (i, &(w, h)) in sizes.iter().enumerate().filter(|(i, _)| !linked[*i]) {
        if x > MARGIN && x + w > wrap {
            x = MARGIN;
            y += row_h + GRID_GAP;
            row_h = 0.;
        }
        boxes[i] = Rect { x, y, w, h };
        width = width.max(x + w);
        height = height.max(y + h);
        x += w + GRID_GAP;
        row_h = row_h.max(h);
    }
    let routes = graph
        .edges
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let bends: Vec<(f32, f32)> = if e.from == e.to {
                Vec::new()
            } else {
                g.edge(
                    &format!("t{}", e.to),
                    &format!("t{}", e.from),
                    Some(&format!("e{i}")),
                )
                .map(|l| {
                    let pts = &l.points;
                    // dagre's first and last points sit on the box borders; ours sit
                    // on the rows.
                    pts.iter()
                        .skip(1)
                        .take(pts.len().saturating_sub(2))
                        .map(|p| (p.x as f32, p.y as f32))
                        .collect()
                })
                .unwrap_or_default()
            };
            route(&boxes, e, bends)
        })
        .collect::<Vec<_>>();
    for r in &routes {
        for &(px, py) in r {
            width = width.max(px);
            height = height.max(py);
        }
    }
    ErLayout {
        boxes,
        routes,
        width: width + MARGIN,
        height: height + MARGIN,
    }
}

/// Y of a row's middle (`None`: the header's).
fn row_y(b: &Rect, row: Option<usize>) -> f32 {
    match row {
        Some(r) => b.y + HEADER_H + r as f32 * ROW_H + ROW_H / 2.,
        None => b.y + HEADER_H / 2.,
    }
}

/// The polyline of `e`: out of the parent's row, through dagre's bends, into the child's
/// row. A self-reference loops around the box's right side.
fn route(boxes: &[Rect], e: &ErEdge, mut bends: Vec<(f32, f32)>) -> Vec<(f32, f32)> {
    let (pb, cb) = (boxes[e.to], boxes[e.from]);
    let (py, cy) = (row_y(&pb, e.to_row), row_y(&cb, e.from_row));
    if e.from == e.to {
        let right = pb.x + pb.w;
        let out = right + STUB * 1.6;
        let cy = if (cy - py).abs() < 1. {
            cy + ROW_H / 2.
        } else {
            cy
        };
        return vec![
            (right, py),
            (right + STUB, py),
            (out, py),
            (out, cy),
            (right + STUB, cy),
            (right, cy),
        ];
    }
    let (pcx, ccx) = (pb.x + pb.w / 2., cb.x + cb.w / 2.);
    // Each end leaves from the side facing the other box.
    let (px, pdir) = if pcx <= ccx {
        (pb.x + pb.w, 1.)
    } else {
        (pb.x, -1.)
    };
    let (cx, cdir) = if ccx >= pcx {
        (cb.x, -1.)
    } else {
        (cb.x + cb.w, 1.)
    };
    let start = (px, py);
    if let (Some(first), Some(last)) = (bends.first(), bends.last()) {
        let d = |a: (f32, f32), b: (f32, f32)| (a.0 - b.0).powi(2) + (a.1 - b.1).powi(2);
        if d(*first, start) > d(*last, start) {
            bends.reverse();
        }
    }
    let mut pts = Vec::with_capacity(bends.len() + 4);
    pts.push(start);
    pts.push((px + pdir * STUB, py));
    pts.extend(bends);
    pts.push((cx + cdir * STUB, cy));
    pts.push((cx, cy));
    pts
}

/// The end marks of a route as line segments: a crow's foot ("many") where it enters the
/// referencing table and a double bar ("exactly one") where it leaves the referenced one.
pub fn edge_marks(route: &[(f32, f32)]) -> Vec<((f32, f32), (f32, f32))> {
    let mut out = Vec::new();
    if route.len() < 2 {
        return out;
    }
    const SPREAD: f32 = 5.;
    // Parent end: two bars across the first segment.
    let (p, q) = (route[0], route[1]);
    let d = (q.0 - p.0).signum();
    for off in [6., 10.] {
        let x = p.0 + d * off;
        out.push(((x, p.1 - SPREAD), (x, p.1 + SPREAD)));
    }
    // Child end: three prongs from a point on the last segment onto the border.
    let (c, q) = (route[route.len() - 1], route[route.len() - 2]);
    let d = (q.0 - c.0).signum();
    let f = (c.0 + d * 11., c.1);
    for dy in [-SPREAD, 0., SPREAD] {
        out.push((f, (c.0, c.1 + dy)));
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use switchyard_core::db::{ColumnInfo, ForeignKeyInfo, ObjectInfo};

    fn col(name: &str, ty: &str, ordinal: i32, pk: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: ty.into(),
            ordinal,
            is_primary_key: pk,
            nullable: !pk,
            ..ColumnInfo::default()
        }
    }

    fn fk(name: &str, cols: &[&str], references: &str, refs: &[&str]) -> ForeignKeyInfo {
        ForeignKeyInfo {
            name: name.into(),
            columns: cols.iter().map(|s| s.to_string()).collect(),
            references: references.into(),
            referenced_columns: refs.iter().map(|s| s.to_string()).collect(),
            ..ForeignKeyInfo::default()
        }
    }

    pub(crate) fn table(
        name: &str,
        columns: Vec<ColumnInfo>,
        fks: Vec<ForeignKeyInfo>,
    ) -> ObjectDetail {
        ObjectDetail {
            object: ObjectInfo {
                schema: "app".into(),
                name: name.into(),
                kind: ObjectKind::Table,
                ..ObjectInfo::default()
            },
            columns,
            foreign_keys: fks,
            ..ObjectDetail::default()
        }
    }

    /// customers ← orders (customer_id) ← order_lines (order_id, order_no: two columns),
    /// employees.manager_id → employees, orders.region_code → ref.regions (not selected).
    pub(crate) fn sample() -> Vec<ObjectDetail> {
        vec![
            table(
                "customers",
                vec![col("id", "int", 1, true), col("name", "text", 2, false)],
                vec![],
            ),
            table(
                "orders",
                vec![
                    col("id", "int", 1, true),
                    col("no", "int", 2, true),
                    col("customer_id", "int", 3, false),
                    col("region_code", "char(2)", 4, false),
                ],
                vec![
                    fk(
                        "orders_customer_fk",
                        &["customer_id"],
                        "app.customers",
                        &["id"],
                    ),
                    fk(
                        "orders_region_fk",
                        &["region_code"],
                        "ref.regions",
                        &["code"],
                    ),
                ],
            ),
            table(
                "order_lines",
                vec![
                    col("order_id", "int", 1, true),
                    col("order_no", "int", 2, true),
                    col("line", "int", 3, true),
                ],
                vec![fk(
                    "lines_order_fk",
                    &["order_id", "order_no"],
                    "app.orders",
                    &["id", "no"],
                )],
            ),
            table(
                "employees",
                vec![
                    col("id", "int", 1, true),
                    col("manager_id", "int", 2, false),
                ],
                vec![fk(
                    "employees_manager_fk",
                    &["manager_id"],
                    "employees",
                    &["id"],
                )],
            ),
        ]
    }

    #[test]
    fn graph_has_nodes_edges_and_stubs() {
        let details = sample();
        let refs: Vec<&ObjectDetail> = details.iter().collect();
        let g = build_graph(&refs, MAX_COLUMNS);
        let names: Vec<String> = g.tables.iter().map(ErTable::qualified).collect();
        assert_eq!(
            names,
            [
                "app.customers",
                "app.orders",
                "app.order_lines",
                "app.employees",
                "ref.regions"
            ]
        );
        assert!(g.tables[4].stub);
        assert_eq!(g.tables[4].columns.len(), 1);
        assert_eq!(g.tables[4].columns[0].name, "code");
        assert_eq!(g.edges.len(), 4);
        // orders.customer_id (row 2) → customers.id (row 0).
        let e = &g.edges[0];
        assert_eq!(
            (e.from, e.to, e.from_row, e.to_row),
            (1, 0, Some(2), Some(0))
        );
        assert!(g.tables[1].columns[2].fk);
        assert!(!g.tables[1].columns[1].fk);
        // Stub edge to the outside table.
        assert_eq!(
            (g.edges[1].from, g.edges[1].to, g.edges[1].to_row),
            (1, 4, Some(0))
        );
    }

    #[test]
    fn multi_column_fk_is_one_edge_anchored_on_its_first_column() {
        let details = sample();
        let refs: Vec<&ObjectDetail> = details.iter().collect();
        let g = build_graph(&refs, MAX_COLUMNS);
        let e = &g.edges[2];
        assert_eq!(e.name, "lines_order_fk");
        assert_eq!((e.from, e.to), (2, 1));
        assert_eq!(e.from_columns, ["order_id", "order_no"]);
        assert_eq!(e.to_columns, ["id", "no"]);
        assert_eq!((e.from_row, e.to_row), (Some(0), Some(0)));
        assert!(g.tables[2].columns[0].fk && g.tables[2].columns[1].fk);
        assert!(!g.tables[2].columns[2].fk);
    }

    #[test]
    fn self_reference_resolves_a_bare_name_to_the_own_schema() {
        let details = sample();
        let refs: Vec<&ObjectDetail> = details.iter().collect();
        let g = build_graph(&refs, MAX_COLUMNS);
        let e = &g.edges[3];
        assert_eq!(
            (e.from, e.to, e.from_row, e.to_row),
            (3, 3, Some(1), Some(0))
        );
        let l = layout(&g);
        let r = &l.routes[3];
        let b = l.boxes[3];
        // Leaves and re-enters the right side.
        assert_eq!(r.first().map(|p| p.0), Some(b.x + b.w));
        assert_eq!(r.last().map(|p| p.0), Some(b.x + b.w));
    }

    #[test]
    fn stub_only_when_the_reference_is_outside_the_selection() {
        let details = sample();
        // Only order_lines: orders becomes a stub with the two referenced columns.
        let g = build_graph(&[&details[2]], MAX_COLUMNS);
        assert_eq!(g.tables.len(), 2);
        assert!(g.tables[1].stub);
        let cols: Vec<&str> = g.tables[1]
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(cols, ["id", "no"]);
        // Two FKs to one stub add its columns once.
        let mut d = details[2].clone();
        d.foreign_keys
            .push(fk("again", &["line"], "app.orders", &["no"]));
        let g = build_graph(&[&d], MAX_COLUMNS);
        assert_eq!(g.tables.len(), 2);
        assert_eq!(g.tables[1].columns.len(), 2);
        assert_eq!(g.edges[1].to_row, Some(1));
    }

    #[test]
    fn long_tables_collapse_and_hidden_fk_columns_use_the_more_row() {
        let cols = (1..=20)
            .map(|i| col(&format!("c{i}"), "int", i, i == 1))
            .collect();
        let d = table(
            "wide",
            cols,
            vec![fk("wide_fk", &["c18"], "app.wide", &["c1"])],
        );
        let g = build_graph(&[&d], 5);
        let t = &g.tables[0];
        assert_eq!((t.columns.len(), t.hidden, t.rows()), (5, 15, 6));
        assert_eq!(g.edges[0].from_row, Some(5));
        assert_eq!(g.edges[0].to_row, Some(0));
    }

    #[test]
    fn layout_places_every_table_without_overlap() {
        let details = sample();
        let refs: Vec<&ObjectDetail> = details.iter().collect();
        let g = build_graph(&refs, MAX_COLUMNS);
        let l = layout(&g);
        assert_eq!(l.boxes.len(), g.tables.len());
        assert_eq!(l.routes.len(), g.edges.len());
        for (i, a) in l.boxes.iter().enumerate() {
            assert!(a.w > 0. && a.h > 0.);
            assert!(a.x + a.w <= l.width && a.y + a.h <= l.height);
            for b in &l.boxes[i + 1..] {
                let apart =
                    a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y;
                assert!(apart, "{a:?} overlaps {b:?}");
            }
        }
        // Referenced tables sit left of their referencing ones.
        assert!(l.boxes[0].x < l.boxes[1].x);
        assert!(l.boxes[1].x < l.boxes[2].x);
        for r in &l.routes {
            assert!(r.len() >= 4);
            assert_eq!(r[0].1, r[1].1, "first run is horizontal");
            assert_eq!(r[r.len() - 1].1, r[r.len() - 2].1, "last run is horizontal");
        }
    }

    #[test]
    fn unrelated_tables_go_in_a_grid() {
        let d: Vec<ObjectDetail> = (0..3)
            .map(|i| table(&format!("t{i}"), vec![col("id", "int", 1, true)], vec![]))
            .collect();
        let refs: Vec<&ObjectDetail> = d.iter().collect();
        let l = layout(&build_graph(&refs, MAX_COLUMNS));
        assert_eq!(l.boxes[0].y, l.boxes[1].y);
        assert!(l.boxes[0].x + l.boxes[0].w < l.boxes[1].x);
    }

    #[test]
    fn selection_filters_relates_and_caps() {
        let all: Vec<String> = ["customers", "orders", "order_lines", "employees", "audit"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (sel, total) = select_tables(&all, "ORDER", None, 10);
        assert_eq!(
            (sel, total),
            (vec!["orders".to_owned(), "order_lines".to_owned()], 2)
        );
        let (sel, total) = select_tables(&all, "", None, 3);
        assert_eq!((sel.len(), total), (3, 5));
        let details = sample();
        let related = related_names("orders", "app", details.iter());
        let (sel, _) = select_tables(&all, "", Some(&related), 10);
        assert_eq!(sel, ["customers", "orders", "order_lines"]);
    }

    #[test]
    fn marks_sit_on_the_end_runs() {
        let route = [
            (100., 50.),
            (118., 50.),
            (200., 80.),
            (282., 80.),
            (300., 80.),
        ];
        let m = edge_marks(&route);
        assert_eq!(m.len(), 5);
        assert_eq!(m[0], ((106., 45.), (106., 55.)));
        assert_eq!(m[2], ((289., 80.), (300., 75.)));
    }
}
