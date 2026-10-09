//! Markdown in assistant answers, parsed (GFM, with the `markdown` crate) into the blocks the
//! assistant panel draws: paragraphs, headings, list items, code blocks, tables, quotes and
//! rules, each holding plain text plus style spans (bold, italic, inline code, strikethrough,
//! links). The text is what a selection copies; the markup characters are gone.

use std::ops::Range;

use markdown::mdast::Node;

/// How a span of text is styled.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Marks {
    /// `**strong**`.
    pub bold: bool,
    /// `*emphasis*`.
    pub italic: bool,
    /// `` `code` ``.
    pub code: bool,
    /// `~~delete~~`.
    pub strike: bool,
    /// A link's URL (http, https or mailto only; others show as text).
    pub link: Option<String>,
}

impl Marks {
    fn is_plain(&self) -> bool {
        *self == Self::default()
    }
}

/// Text with styled byte ranges (sorted, not overlapping).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inline {
    /// The text as shown.
    pub text: String,
    /// Styled ranges of `text`.
    pub spans: Vec<(Range<usize>, Marks)>,
}

impl Inline {
    fn push(&mut self, s: &str, marks: &Marks) {
        if s.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(s);
        if marks.is_plain() {
            return;
        }
        match self.spans.last_mut() {
            Some((r, m)) if r.end == start && m == marks => r.end = self.text.len(),
            _ => self.spans.push((start..self.text.len(), marks.clone())),
        }
    }

    fn append(&mut self, other: Inline) {
        let shift = self.text.len();
        self.text.push_str(&other.text);
        self.spans.extend(
            other
                .spans
                .into_iter()
                .map(|(r, m)| (r.start + shift..r.end + shift, m)),
        );
    }

    /// Drop surrounding whitespace, keeping spans in step.
    fn trimmed(self) -> Inline {
        let start = self.text.len() - self.text.trim_start().len();
        let end = self.text.trim_end().len().max(start);
        let text = self.text[start..end].to_owned();
        let spans = self
            .spans
            .into_iter()
            .filter_map(|(r, m)| {
                let s = r.start.clamp(start, end) - start;
                let e = r.end.clamp(start, end) - start;
                (s < e).then_some((s..e, m))
            })
            .collect();
        Inline { text, spans }
    }

    /// Plain text only.
    #[cfg(test)]
    pub fn plain(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            spans: Vec::new(),
        }
    }
}

/// One block of an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// A paragraph.
    Paragraph(Inline),
    /// `#`–`######`.
    Heading {
        /// 1–6.
        level: u8,
        /// Its text.
        text: Inline,
    },
    /// A list item: `•`, `3.`, `☐` or `☑`, nested `depth` levels (0 at the top).
    Item {
        /// Nesting, 0 at the top.
        depth: usize,
        /// The bullet or number.
        marker: String,
        /// Its text (nested lists follow as their own items).
        text: Inline,
    },
    /// A fenced or indented code block.
    Code {
        /// The info string's language, lowercase (may be empty).
        lang: String,
        /// The code.
        body: String,
    },
    /// A GFM table.
    Table {
        /// Header cells.
        head: Vec<Inline>,
        /// Body rows.
        rows: Vec<Vec<Inline>>,
    },
    /// A paragraph inside a block quote.
    Quote(Inline),
    /// `---`.
    Rule,
}

/// The blocks of `text`. An unclosed fence runs to the end (the answer is still streaming).
pub fn parse(text: &str) -> Vec<Block> {
    let mut out = Vec::new();
    match markdown::to_mdast(text, &markdown::ParseOptions::gfm()) {
        Ok(root) => blocks(
            root.children().map(Vec::as_slice).unwrap_or(&[]),
            0,
            &mut out,
        ),
        // Only MDX syntax fails to parse, and it is off.
        Err(_) => {
            let t = text.trim();
            if !t.is_empty() {
                out.push(Block::Paragraph(Inline {
                    text: t.to_owned(),
                    spans: Vec::new(),
                }));
            }
        }
    }
    out
}

fn blocks(nodes: &[Node], depth: usize, out: &mut Vec<Block>) {
    for n in nodes {
        match n {
            Node::Paragraph(p) => {
                let text = inline_of(&p.children);
                if !text.text.is_empty() {
                    out.push(Block::Paragraph(text));
                }
            }
            Node::Heading(h) => out.push(Block::Heading {
                level: h.depth.clamp(1, 6),
                text: inline_of(&h.children),
            }),
            Node::Code(c) => out.push(Block::Code {
                lang: c
                    .lang
                    .as_deref()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase(),
                body: c.value.trim_end().to_owned(),
            }),
            Node::Math(m) => out.push(Block::Code {
                lang: "math".into(),
                body: m.value.clone(),
            }),
            Node::List(l) => {
                let mut number = l.start.unwrap_or(1);
                for item in &l.children {
                    let Node::ListItem(item) = item else {
                        continue;
                    };
                    let marker = match (item.checked, l.ordered) {
                        (Some(true), _) => "☑".to_owned(),
                        (Some(false), _) => "☐".to_owned(),
                        (None, true) => format!("{number}."),
                        (None, false) => "•".to_owned(),
                    };
                    number += 1;
                    // The item's own paragraphs make its text; other blocks follow it.
                    let mut text = Inline::default();
                    let mut rest = Vec::new();
                    for child in &item.children {
                        match child {
                            Node::Paragraph(p) if rest.is_empty() => {
                                if !text.text.is_empty() {
                                    text.push("\n", &Marks::default());
                                }
                                text.append(inline_of(&p.children));
                            }
                            other => rest.push(other.clone()),
                        }
                    }
                    out.push(Block::Item {
                        depth,
                        marker,
                        text,
                    });
                    blocks(&rest, depth + 1, out);
                }
            }
            Node::Table(t) => {
                let mut rows = t.children.iter().filter_map(|r| match r {
                    Node::TableRow(r) => Some(
                        r.children
                            .iter()
                            .map(|c| inline_of(c.children().map(Vec::as_slice).unwrap_or(&[])))
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                });
                let head = rows.next().unwrap_or_default();
                out.push(Block::Table {
                    head,
                    rows: rows.collect(),
                });
            }
            Node::Blockquote(q) => {
                let mut inner = Vec::new();
                blocks(&q.children, depth, &mut inner);
                out.extend(inner.into_iter().map(|b| match b {
                    Block::Paragraph(t) => Block::Quote(t),
                    other => other,
                }));
            }
            Node::ThematicBreak(_) => out.push(Block::Rule),
            Node::Html(h) => {
                let t = h.value.trim();
                if !t.is_empty() {
                    out.push(Block::Paragraph(Inline {
                        text: t.to_owned(),
                        spans: Vec::new(),
                    }));
                }
            }
            // Link definitions and front matter show nothing.
            Node::Definition(_) | Node::Yaml(_) | Node::Toml(_) => {}
            other => {
                if let Some(children) = other.children() {
                    blocks(children, depth, out);
                }
            }
        }
    }
}

fn inline_of(nodes: &[Node]) -> Inline {
    let mut out = Inline::default();
    inline(nodes, &Marks::default(), &mut out);
    out.trimmed()
}

fn inline(nodes: &[Node], marks: &Marks, out: &mut Inline) {
    for n in nodes {
        match n {
            Node::Text(t) => out.push(&t.value, marks),
            Node::Break(_) => out.push("\n", marks),
            Node::InlineCode(c) => out.push(
                &c.value,
                &Marks {
                    code: true,
                    ..marks.clone()
                },
            ),
            Node::InlineMath(m) => out.push(
                &m.value,
                &Marks {
                    code: true,
                    ..marks.clone()
                },
            ),
            Node::Strong(s) => inline(
                &s.children,
                &Marks {
                    bold: true,
                    ..marks.clone()
                },
                out,
            ),
            Node::Emphasis(s) => inline(
                &s.children,
                &Marks {
                    italic: true,
                    ..marks.clone()
                },
                out,
            ),
            Node::Delete(s) => inline(
                &s.children,
                &Marks {
                    strike: true,
                    ..marks.clone()
                },
                out,
            ),
            Node::Link(l) => {
                let link = safe_url(&l.url).map(str::to_owned);
                inline(
                    &l.children,
                    &Marks {
                        link,
                        ..marks.clone()
                    },
                    out,
                );
            }
            Node::Image(i) => out.push(&i.alt, marks),
            Node::Html(h) => out.push(&h.value, marks),
            other => {
                if let Some(children) = other.children() {
                    inline(children, marks, out);
                }
            }
        }
    }
}

/// Links an answer may open: web and mail only (never `file:` or app schemes).
fn safe_url(url: &str) -> Option<&str> {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://") || lower.starts_with("mailto:"))
        .then_some(u)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bold() -> Marks {
        Marks {
            bold: true,
            ..Marks::default()
        }
    }

    fn code() -> Marks {
        Marks {
            code: true,
            ..Marks::default()
        }
    }

    #[test]
    fn inline_styles_drop_their_markup() {
        let b = parse("Use **an index** on `orders(customer_id)`, *not* ~~a scan~~.");
        let [Block::Paragraph(p)] = b.as_slice() else {
            panic!("{b:?}");
        };
        assert_eq!(p.text, "Use an index on orders(customer_id), not a scan.");
        let styled: Vec<(&str, &Marks)> = p
            .spans
            .iter()
            .map(|(r, m)| (&p.text[r.clone()], m))
            .collect();
        assert_eq!(styled[0], ("an index", &bold()));
        assert_eq!(styled[1], ("orders(customer_id)", &code()));
        assert!(styled[2].1.italic && styled[2].0 == "not");
        assert!(styled[3].1.strike && styled[3].0 == "a scan");
    }

    #[test]
    fn nested_marks_combine() {
        let b = parse("***both*** and **bold `code`**");
        let [Block::Paragraph(p)] = b.as_slice() else {
            panic!("{b:?}");
        };
        assert_eq!(p.text, "both and bold code");
        assert!(p.spans[0].1.bold && p.spans[0].1.italic);
        let last = p
            .spans
            .last()
            .map(|(r, m)| (&p.text[r.clone()], m.bold && m.code));
        assert_eq!(last, Some(("code", true)));
        // Spans are sorted and do not overlap.
        assert!(p.spans.windows(2).all(|w| w[0].0.end <= w[1].0.start));
    }

    #[test]
    fn headings_lists_and_rules() {
        let b = parse(
            "## Findings\n\n- first **one**\n- second\n  1. nested\n  2. again\n\n---\n\n3. three\n4. four\n\n- [x] done\n- [ ] todo",
        );
        assert_eq!(
            b[0],
            Block::Heading {
                level: 2,
                text: Inline::plain("Findings")
            }
        );
        let items: Vec<(usize, &str, &str)> = b
            .iter()
            .filter_map(|b| match b {
                Block::Item {
                    depth,
                    marker,
                    text,
                } => Some((*depth, marker.as_str(), text.text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            items,
            [
                (0, "•", "first one"),
                (0, "•", "second"),
                (1, "1.", "nested"),
                (1, "2.", "again"),
                (0, "3.", "three"),
                (0, "4.", "four"),
                (0, "☑", "done"),
                (0, "☐", "todo"),
            ]
        );
        assert!(b.contains(&Block::Rule));
    }

    #[test]
    fn tables() {
        let b = parse(
            "| Index | Gain |\n|---|---:|\n| `ix_orders_customer` | **40%** |\n| none | 0 |\n",
        );
        let [Block::Table { head, rows }] = b.as_slice() else {
            panic!("{b:?}");
        };
        let texts = |r: &[Inline]| r.iter().map(|c| c.text.clone()).collect::<Vec<_>>();
        assert_eq!(texts(head), ["Index", "Gain"]);
        assert_eq!(texts(&rows[0]), ["ix_orders_customer", "40%"]);
        assert_eq!(texts(&rows[1]), ["none", "0"]);
        assert!(rows[0][0].spans[0].1.code);
        assert!(rows[0][1].spans[0].1.bold);
    }

    #[test]
    fn code_blocks_stay_whole_even_while_streaming() {
        let b = parse("Try this:\n\n```sql\nSELECT *\nFROM orders;\n```\nThen **check**.");
        assert_eq!(b[0], Block::Paragraph(Inline::plain("Try this:")));
        assert_eq!(
            b[1],
            Block::Code {
                lang: "sql".into(),
                body: "SELECT *\nFROM orders;".into()
            }
        );
        assert!(matches!(&b[2], Block::Paragraph(p) if p.text == "Then check."));
        // Unclosed: the code runs to the end.
        let b = parse("Plan:\n```sql\nSELECT 1");
        assert_eq!(
            b[1],
            Block::Code {
                lang: "sql".into(),
                body: "SELECT 1".into()
            }
        );
    }

    #[test]
    fn links_open_only_web_and_mail() {
        let b = parse(
            "See [the docs](https://www.postgresql.org/docs/) or [this](file:///etc/passwd) or <https://example.com>.",
        );
        let [Block::Paragraph(p)] = b.as_slice() else {
            panic!("{b:?}");
        };
        assert_eq!(p.text, "See the docs or this or https://example.com.");
        let links: Vec<(&str, &str)> = p
            .spans
            .iter()
            .filter_map(|(r, m)| m.link.as_deref().map(|u| (&p.text[r.clone()], u)))
            .collect();
        assert_eq!(
            links,
            [
                ("the docs", "https://www.postgresql.org/docs/"),
                ("https://example.com", "https://example.com"),
            ]
        );
    }

    #[test]
    fn quotes_and_line_breaks() {
        let b = parse("> **Note:** rolled back\n\nline one\nline two");
        assert!(matches!(&b[0], Block::Quote(q) if q.text == "Note: rolled back"));
        // Single newlines stay line breaks, as agents write them.
        assert_eq!(b[1], Block::Paragraph(Inline::plain("line one\nline two")));
    }

    #[test]
    fn plain_text_is_one_paragraph() {
        assert_eq!(
            parse("  just text  "),
            [Block::Paragraph(Inline::plain("just text"))]
        );
        assert!(parse("").is_empty());
    }
}
