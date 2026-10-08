//! SVG export of an ER diagram ("Copy as SVG", "Save as SVG…"). The text is built here;
//! colours are a fixed light scheme so the file reads the same wherever it is pasted.

use std::fmt::Write as _;

use super::model::{ErGraph, ErLayout, HEADER_H, ROW_H, edge_marks};

const BG: &str = "#ffffff";
const BOX: &str = "#ffffff";
const HEADER: &str = "#eef1f5";
const BORDER: &str = "#8a94a3";
const TEXT: &str = "#1d232b";
const DIM: &str = "#6b7480";
const EDGE: &str = "#5b6675";
const KEY: &str = "#a36a00";

/// Escape text for an SVG text node or attribute.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// A coordinate with at most one decimal.
fn n(v: f32) -> String {
    let r = (v * 10.).round() / 10.;
    if r.fract() == 0. {
        format!("{}", r as i64)
    } else {
        format!("{r:.1}")
    }
}

/// Shorten `s` to `max` characters with an ellipsis.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// The whole diagram as a standalone SVG document.
pub fn to_svg(graph: &ErGraph, layout: &ErLayout) -> String {
    let (w, h) = (n(layout.width), n(layout.height));
    let mut s = String::new();
    let _ = writeln!(
        s,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" font-family="ui-monospace, SFMono-Regular, Menlo, Consolas, monospace" font-size="11">"#
    );
    let _ = writeln!(s, r#"<rect width="100%" height="100%" fill="{BG}"/>"#);
    let _ = writeln!(
        s,
        r#"<g fill="none" stroke="{EDGE}" stroke-width="1.2" stroke-linejoin="round">"#
    );
    for (e, route) in graph.edges.iter().zip(&layout.routes) {
        let pts: Vec<String> = route
            .iter()
            .map(|&(x, y)| format!("{},{}", n(x), n(y)))
            .collect();
        let _ = writeln!(
            s,
            r#"<polyline points="{}"><title>{}</title></polyline>"#,
            pts.join(" "),
            esc(&format!(
                "{} ({}) → {} ({})",
                graph.tables[e.from].name,
                e.from_columns.join(", "),
                graph.tables[e.to].name,
                e.to_columns.join(", ")
            ))
        );
        for ((x1, y1), (x2, y2)) in edge_marks(route) {
            let _ = writeln!(
                s,
                r#"<line x1="{}" y1="{}" x2="{}" y2="{}"/>"#,
                n(x1),
                n(y1),
                n(x2),
                n(y2)
            );
        }
    }
    let _ = writeln!(s, "</g>");
    for (t, b) in graph.tables.iter().zip(&layout.boxes) {
        let dash = if t.stub {
            r#" stroke-dasharray="4 3""#
        } else {
            ""
        };
        let chars = ((b.w - 16.) / 6.7).max(4.) as usize;
        let _ = writeln!(s, r#"<g>"#);
        let _ = writeln!(
            s,
            r#"<rect x="{}" y="{}" width="{}" height="{}" rx="5" fill="{BOX}" stroke="{BORDER}"{dash}/>"#,
            n(b.x),
            n(b.y),
            n(b.w),
            n(b.h)
        );
        let _ = writeln!(
            s,
            r#"<path d="M{} {}h{}a4.5 4.5 0 0 1 4.5 4.5v{}h-{}v-{}a4.5 4.5 0 0 1 4.5 -4.5z" fill="{HEADER}" stroke="none"/>"#,
            n(b.x + 5.),
            n(b.y + 0.5),
            n(b.w - 10.),
            n(HEADER_H - 5.),
            n(b.w - 1.),
            n(HEADER_H - 5.)
        );
        let _ = writeln!(
            s,
            r#"<line x1="{}" y1="{}" x2="{}" y2="{}" stroke="{BORDER}"/>"#,
            n(b.x),
            n(b.y + HEADER_H),
            n(b.x + b.w),
            n(b.y + HEADER_H)
        );
        let _ = writeln!(
            s,
            r#"<text x="{}" y="{}" fill="{TEXT}" font-weight="600" font-size="12">{}</text>"#,
            n(b.x + 8.),
            n(b.y + HEADER_H / 2. + 4.),
            esc(&clip(&t.name, chars))
        );
        for (i, c) in t.columns.iter().enumerate() {
            let y = b.y + HEADER_H + i as f32 * ROW_H + ROW_H / 2. + 4.;
            let mark = match (c.pk, c.fk) {
                (true, true) => "PF",
                (true, false) => "PK",
                (false, true) => "FK",
                _ => "",
            };
            if !mark.is_empty() {
                let _ = writeln!(
                    s,
                    r#"<text x="{}" y="{}" fill="{KEY}" font-size="9" font-weight="600">{mark}</text>"#,
                    n(b.x + 6.),
                    n(y)
                );
            }
            let name_chars = chars.saturating_sub(c.data_type.chars().count().min(14) + 4);
            let _ = writeln!(
                s,
                r#"<text x="{}" y="{}" fill="{TEXT}">{}</text>"#,
                n(b.x + 24.),
                n(y),
                esc(&clip(&c.name, name_chars.max(4)))
            );
            if !c.data_type.is_empty() {
                let _ = writeln!(
                    s,
                    r#"<text x="{}" y="{}" fill="{DIM}" text-anchor="end">{}</text>"#,
                    n(b.x + b.w - 8.),
                    n(y),
                    esc(&clip(&c.data_type, 14))
                );
            }
        }
        if t.hidden > 0 {
            let y = b.y + HEADER_H + t.columns.len() as f32 * ROW_H + ROW_H / 2. + 4.;
            let _ = writeln!(
                s,
                r#"<text x="{}" y="{}" fill="{DIM}">+{} more</text>"#,
                n(b.x + 24.),
                n(y),
                t.hidden
            );
        }
        let _ = writeln!(s, "</g>");
    }
    s.push_str("</svg>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::er_tab::model::{ErLayout, Rect, build_graph, layout};
    use switchyard_core::db::ObjectDetail;

    #[test]
    fn escapes_markup() {
        assert_eq!(esc(r#"a<b>&"c'"#), "a&lt;b&gt;&amp;&quot;c&apos;");
        assert_eq!(n(1.25), "1.3");
        assert_eq!(n(2.0), "2");
        assert_eq!(clip("abcdef", 4), "abc…");
    }

    /// A small diagram with hand-placed boxes, so the snapshot does not depend on the
    /// layout crate.
    #[test]
    fn small_diagram_snapshot() {
        let details = crate::er_tab::model::tests::sample();
        let refs: Vec<&ObjectDetail> = details[..3].iter().collect();
        let mut g = build_graph(&refs, 3);
        // A hostile name is escaped.
        g.tables[0].name = "cust<omers>".into();
        let boxes: Vec<Rect> = g
            .tables
            .iter()
            .enumerate()
            .map(|(i, t)| Rect {
                x: 20. + i as f32 * 240.,
                y: 20.,
                w: 200.,
                h: t.height(),
            })
            .collect();
        // Straight routes from each parent's right side to each child's left side.
        let routes = g
            .edges
            .iter()
            .map(|e| {
                let (p, c) = (boxes[e.to], boxes[e.from]);
                let py = p.y + HEADER_H + e.to_row.unwrap_or(0) as f32 * ROW_H + ROW_H / 2.;
                let cy = c.y + HEADER_H + e.from_row.unwrap_or(0) as f32 * ROW_H + ROW_H / 2.;
                vec![
                    (p.x + p.w, py),
                    (p.x + p.w + 18., py),
                    (c.x - 18., cy),
                    (c.x, cy),
                ]
            })
            .collect();
        let l = ErLayout {
            width: 20. + boxes.len() as f32 * 240.,
            height: 160.,
            boxes,
            routes,
        };
        insta::assert_snapshot!(to_svg(&g, &l));
    }

    #[test]
    fn laid_out_diagram_is_well_formed() {
        let details = crate::er_tab::model::tests::sample();
        let refs: Vec<&ObjectDetail> = details.iter().collect();
        let g = build_graph(&refs, 12);
        let svg = to_svg(&g, &layout(&g));
        assert!(svg.starts_with("<svg "));
        assert!(svg.ends_with("</svg>\n"));
        assert_eq!(svg.matches("<polyline").count(), g.edges.len());
        assert_eq!(svg.matches("<g>").count(), g.tables.len());
        assert_eq!(svg.matches("stroke-dasharray").count(), 1);
    }
}
