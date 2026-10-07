//! Pure text helpers behind the Workbench's Pretty and Diff panes.
//!
//! Response presentation is prepared once on GPUI's background executor. The
//! diff panes likewise cap their painted rows at [`MAX_DIFF_LINES`]. This
//! module does not know about GPUI: the tokenizer names *what* a span is and
//! the caller picks the colour, which keeps the design's token map in one
//! place.

use std::ops::Range;
use std::sync::Arc;

/// Above this size the response uses a virtualized line list instead of
/// asking GPUI to shape one document-sized `StyledText` on every frame.
pub const VIRTUALIZED_BODY_BYTES: usize = 128 * 1024;
const VIRTUAL_ROW_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualRow {
    pub range: Range<usize>,
    /// `None` marks a continuation chunk of an unusually long source line.
    pub line_number: Option<usize>,
}

/// What a span of a formatted body is, for colouring.
///
/// One vocabulary for every format the Pretty pane knows, so the colour map
/// stays in one place: a variant means the same thing whichever tokenizer
/// emitted it, and a token without a colour does not compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    /// A JSON object key, quotes included.
    Key,
    /// A JSON string value, or a markup attribute value, quotes included.
    Str,
    /// A JSON number, `true`, `false` or `null`.
    Literal,
    /// Braces, brackets, colons and commas; markup's angle brackets, slashes
    /// and `=`.
    Punct,
    /// A `{{variable}}` reference inside a string — the Workbench's own
    /// notation, which the design paints lavender wherever it appears.
    Var,
    /// A markup element name.
    Tag,
    /// A markup attribute name.
    Attr,
    /// Character data between markup tags.
    Text,
    /// A comment, doctype, processing instruction or CDATA section, its
    /// marker characters included.
    Comment,
}

/// How the Pretty pane presents a response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFormat {
    /// Re-indented JSON, coloured by [`json_spans`].
    Json,
    /// Re-indented XML or HTML, coloured by [`xml_spans`].
    Xml,
    /// Anything else: the payload verbatim, uncoloured.
    Text,
    Yaml,
    JavaScript,
    Markdown,
    Html,
}

/// A response body after its expensive, body-dependent work has finished.
/// Strings are shared so tabs and render passes never copy a multi-megabyte
/// response merely to hand it to an element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedBody {
    pub raw: Arc<str>,
    pub raw_lines: Option<Arc<Vec<VirtualRow>>>,
    pub presentation: BodyPresentation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyPresentation {
    Inline {
        format: BodyFormat,
        text: Arc<str>,
        spans: Arc<Vec<(Range<usize>, Token)>>,
        line_numbers: Option<Arc<str>>,
    },
    /// The fully formatted response plus byte ranges for its lines. The UI's
    /// list callback materializes only the visible ranges.
    Virtualized {
        format: BodyFormat,
        text: Arc<str>,
        lines: Arc<Vec<VirtualRow>>,
    },
}

/// Format and tokenize one response away from GPUI's frame thread.
pub fn prepare(content_type: Option<&str>, body: &str, binary: bool) -> PreparedBody {
    let raw: Arc<str> = Arc::from(body);
    let raw_lines = (raw.len() >= VIRTUALIZED_BODY_BYTES).then(|| Arc::new(line_ranges(&raw)));
    let (format, text) = if binary {
        (BodyFormat::Text, raw.clone())
    } else {
        let (format, text) = present(content_type, body);
        let text = if format == BodyFormat::Text && text == body {
            raw.clone()
        } else {
            Arc::from(text)
        };
        (format, text)
    };

    let presentation = if text.len() >= VIRTUALIZED_BODY_BYTES {
        BodyPresentation::Virtualized {
            format,
            lines: Arc::new(line_ranges(&text)),
            text,
        }
    } else {
        let rows = text.split('\n').count();
        let line_numbers = (rows > 1).then(|| {
            Arc::from(
                (1..=rows)
                    .map(|row| row.to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        });
        BodyPresentation::Inline {
            format,
            spans: Arc::new(spans(&text, format)),
            text,
            line_numbers,
        }
    };
    PreparedBody {
        raw,
        raw_lines,
        presentation,
    }
}

fn line_ranges(text: &str) -> Vec<VirtualRow> {
    let mut rows = Vec::with_capacity(text.bytes().filter(|byte| *byte == b'\n').count() + 1);
    let mut start = 0;
    let mut line_number = 1;
    for (newline, _) in text.match_indices('\n') {
        push_virtual_line(&mut rows, text, start, newline, line_number);
        start = newline + 1;
        line_number += 1;
    }
    // Deliberately retain the last empty row when the body ends in a newline,
    // matching the inline gutter's `split('\n')` behavior.
    push_virtual_line(&mut rows, text, start, text.len(), line_number);
    rows
}

fn push_virtual_line(
    rows: &mut Vec<VirtualRow>,
    text: &str,
    start: usize,
    end: usize,
    line_number: usize,
) {
    if start == end {
        rows.push(VirtualRow {
            range: start..end,
            line_number: Some(line_number),
        });
        return;
    }
    let mut at = start;
    let mut first = true;
    while at < end {
        let mut chunk_end = (at + VIRTUAL_ROW_BYTES).min(end);
        while chunk_end > at && !text.is_char_boundary(chunk_end) {
            chunk_end -= 1;
        }
        if chunk_end == at {
            chunk_end = at + text[at..end].chars().next().map_or(1, char::len_utf8);
        }
        rows.push(VirtualRow {
            range: at..chunk_end,
            line_number: first.then_some(line_number),
        });
        first = false;
        at = chunk_end;
    }
}

/// The text the Pretty pane paints for `body`, and which tokenizer colours it.
///
/// The body's own shape decides, with the declared `Content-Type` as a
/// tiebreaker rather than as the authority: services label JSON `text/plain`
/// often enough that trusting the header would leave real JSON unformatted,
/// and a header promising XML does not make a truncated payload parse. A
/// format that does not apply returns the body exactly as it arrived, so this
/// is safe to call on any response.
pub fn present(content_type: Option<&str>, body: &str) -> (BodyFormat, String) {
    if let Some(formatted) = format_json(body) {
        return (BodyFormat::Json, formatted);
    }
    let declared = content_type.unwrap_or_default().to_ascii_lowercase();
    let markup = declared.contains("xml") || declared.contains("html");
    if (markup || body.trim_start().starts_with('<'))
        && let Some(formatted) = format_xml(body)
    {
        return (BodyFormat::Xml, formatted);
    }
    (BodyFormat::Text, body.to_string())
}

/// Coloured byte ranges for text already put through [`present`]. Calling
/// this with a format the text did not come back as is meaningless, which is
/// why the two are returned together.
pub fn spans(text: &str, format: BodyFormat) -> Vec<(Range<usize>, Token)> {
    match format {
        BodyFormat::Json => json_spans(text),
        BodyFormat::Xml => xml_spans(text),
        BodyFormat::Text
        | BodyFormat::Yaml
        | BodyFormat::JavaScript
        | BodyFormat::Markdown
        | BodyFormat::Html => Vec::new(),
    }
}

/// Tokenize `text` — expected to be pretty-printed JSON — into coloured
/// byte ranges. Whitespace is skipped; ranges are ascending, disjoint and on
/// char boundaries. Text that is not JSON still tokenizes (a bare word is a
/// literal), which is what makes this safe to run on any body: the caller
/// decides whether to call it at all.
pub fn json_spans(text: &str) -> Vec<(Range<usize>, Token)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        match byte {
            b' ' | b'\n' | b'\r' | b'\t' => at += 1,
            b'{' | b'}' | b'[' | b']' | b':' | b',' => {
                spans.push((at..at + 1, Token::Punct));
                at += 1;
            }
            b'"' => {
                let end = string_end(bytes, at);
                let mut after = end;
                while after < bytes.len() && matches!(bytes[after], b' ' | b'\t') {
                    after += 1;
                }
                let kind = if bytes.get(after) == Some(&b':') {
                    Token::Key
                } else {
                    Token::Str
                };
                push_string(&mut spans, text, at..end, kind);
                at = end;
            }
            _ => {
                let mut end = at + 1;
                while end < bytes.len()
                    && !matches!(
                        bytes[end],
                        b' ' | b'\n'
                            | b'\r'
                            | b'\t'
                            | b'{'
                            | b'}'
                            | b'['
                            | b']'
                            | b':'
                            | b','
                            | b'"'
                    )
                {
                    end += 1;
                }
                spans.push((at..end, Token::Literal));
                at = end;
            }
        }
    }
    spans
}

/// The byte just past the string that opens at `open` (its closing quote
/// included), honouring backslash escapes; the end of the text when unclosed.
fn string_end(bytes: &[u8], open: usize) -> usize {
    let mut at = open + 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'"' => return at + 1,
            _ => at += 1,
        }
    }
    bytes.len()
}

/// Re-indent a JSON body with two-space indentation — the Body tab's
/// **Format** action. `None` when the text is not JSON, so a `{{template}}`
/// outside a string or a half-typed body is left exactly as the user wrote
/// it. Keys keep their order and values their spelling: this only moves
/// whitespace, unlike a parse-and-serialize round trip that would sort keys
/// and rewrite numbers.
pub fn format_json(source: &str) -> Option<String> {
    serde_json::from_str::<serde::de::IgnoredAny>(source).ok()?;
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len() + source.len() / 4);
    let mut depth = 0usize;
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        for _ in 0..depth {
            out.push_str("  ");
        }
    };
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'"' => {
                let end = string_end(bytes, at);
                out.push_str(&source[at..end]);
                at = end;
            }
            open @ (b'{' | b'[') => {
                let close = if open == b'{' { b'}' } else { b']' };
                let mut next = at + 1;
                while next < bytes.len() && bytes[next].is_ascii_whitespace() {
                    next += 1;
                }
                out.push(open as char);
                if bytes.get(next) == Some(&close) {
                    // `{}` and `[]` stay on one line.
                    out.push(close as char);
                    at = next + 1;
                } else {
                    depth += 1;
                    newline(&mut out, depth);
                    at += 1;
                }
            }
            close @ (b'}' | b']') => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(close as char);
                at += 1;
            }
            b',' => {
                out.push(',');
                newline(&mut out, depth);
                at += 1;
            }
            b':' => {
                out.push_str(": ");
                at += 1;
            }
            byte if byte.is_ascii_whitespace() => at += 1,
            // Valid JSON has only ASCII outside strings.
            byte => {
                out.push(byte as char);
                at += 1;
            }
        }
    }
    Some(out)
}

/// A string span, with any `{{var}}` inside it split out as [`Token::Var`].
fn push_string(
    spans: &mut Vec<(Range<usize>, Token)>,
    text: &str,
    range: Range<usize>,
    kind: Token,
) {
    let mut cursor = range.start;
    let inner = &text[range.clone()];
    let mut search = 0;
    while let Some(open) = inner[search..].find("{{") {
        let open = search + open;
        let Some(close) = inner[open..].find("}}") else {
            break;
        };
        let close = open + close + 2;
        if range.start + open > cursor {
            spans.push((cursor..range.start + open, kind));
        }
        spans.push((range.start + open..range.start + close, Token::Var));
        cursor = range.start + close;
        search = close;
    }
    if cursor < range.end {
        spans.push((cursor..range.end, kind));
    }
}

// ---------------------------------------------------------------------------
// Markup
// ---------------------------------------------------------------------------

/// HTML elements that never carry a closing tag. XML has none of these names
/// reserved, but a well-formed XML document that uses one writes it
/// self-closing, so treating them as empty is safe for both dialects.
const VOID_ELEMENTS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements whose content is raw text rather than markup: a `<` inside one
/// opens nothing, so the scanner skips to the matching close tag instead of
/// reading a bogus element out of a script's `a < b`.
const RAW_TEXT_ELEMENTS: [&str; 2] = ["script", "style"];

/// HTML's optional end tags, keyed by the element being *opened*: the value is
/// the open elements that opening it ends.
///
/// Without this a list of 100 `<li>`s indents 100 levels deep, because each
/// item nests inside the one before it — the single ugliest thing about
/// re-indenting a real HTML page. XML never needs the table: its end tags are
/// mandatory, so the element is already off the stack by the time the sibling
/// opens.
const IMPLICIT_CLOSES: [(&str, &[&str]); 8] = [
    ("li", &["li"]),
    ("p", &["p"]),
    ("dt", &["dt", "dd"]),
    ("dd", &["dt", "dd"]),
    ("tr", &["tr", "td", "th"]),
    ("td", &["td", "th"]),
    ("th", &["td", "th"]),
    ("option", &["option"]),
];

/// One thing the markup scanner found, at the depth it sits at.
struct Placed<'a> {
    depth: usize,
    node: Node<'a>,
}

enum Node<'a> {
    /// `<name attrs>`, angle brackets included.
    Open { name: &'a str, raw: &'a str },
    /// `<name/>`, or a void HTML element written without the slash.
    Empty(&'a str),
    /// `</name>`.
    Close { name: &'a str, raw: &'a str },
    /// A comment, doctype, processing instruction or CDATA section.
    Standalone(&'a str),
    /// Character data, already trimmed and never empty.
    Text(&'a str),
}

/// Re-indent an XML or HTML body with two-space indentation — what the Pretty
/// pane shows for a markup response.
///
/// `None` when the text is not markup this can walk: no element at all, a tag
/// that never closes, or a close tag naming nothing that is open. A body that
/// merely starts with `<` therefore falls back to being shown verbatim rather
/// than being mangled. Only whitespace *between* nodes moves — tags, attribute
/// spelling, entity references and the text inside an element are copied byte
/// for byte, so nothing here can change what the response said.
pub fn format_xml(source: &str) -> Option<String> {
    let placed = xml_nodes(source)?;
    let mut out = String::with_capacity(source.len() + source.len() / 4);
    let mut index = 0;
    while index < placed.len() {
        let Placed { depth, node } = &placed[index];
        match node {
            Node::Open { name, raw } => {
                // A leaf element stays on one line. Splitting `<id>7</id>`
                // across three would add indentation the document does not
                // have, and leaves are most of what an XML body is.
                let text = match placed.get(index + 1) {
                    Some(Placed {
                        node: Node::Text(text),
                        ..
                    }) if !text.contains('\n') => Some(*text),
                    _ => None,
                };
                let closes_at = index + if text.is_some() { 2 } else { 1 };
                if let Some(Placed {
                    node:
                        Node::Close {
                            name: close,
                            raw: close_raw,
                        },
                    ..
                }) = placed.get(closes_at)
                    && close.eq_ignore_ascii_case(name)
                {
                    line(&mut out, *depth);
                    out.push_str(raw);
                    out.push_str(text.unwrap_or_default());
                    out.push_str(close_raw);
                    index = closes_at + 1;
                    continue;
                }
                line(&mut out, *depth);
                out.push_str(raw);
            }
            Node::Empty(raw) | Node::Standalone(raw) | Node::Close { raw, .. } => {
                line(&mut out, *depth);
                out.push_str(raw);
            }
            Node::Text(text) => {
                line(&mut out, *depth);
                out.push_str(text);
            }
        }
        index += 1;
    }
    Some(out)
}

fn line(out: &mut String, depth: usize) {
    if !out.is_empty() {
        out.push('\n');
    }
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// Split `source` into markup nodes, each carrying the depth it sits at.
///
/// The depth comes from the open-element stack rather than from counting tags
/// on the way out, which is what makes HTML's implicit closes come out right:
/// `</ul>` around unclosed `<li>`s pops all of them at once, and the `</ul>`
/// itself lands back at the `<ul>`'s own depth.
fn xml_nodes(source: &str) -> Option<Vec<Placed<'_>>> {
    let bytes = source.as_bytes();
    let mut nodes = Vec::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut elements = 0usize;
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes[at] != b'<' {
            let end = source[at..]
                .find('<')
                .map_or(bytes.len(), |index| at + index);
            let text = source[at..end].trim();
            if !text.is_empty() {
                nodes.push(Placed {
                    depth: stack.len(),
                    node: Node::Text(text),
                });
            }
            at = end;
            continue;
        }
        let rest = &source[at..];
        let standalone = if rest.starts_with("<!--") {
            rest.find("-->").map(|index| at + index + 3)
        } else if rest.starts_with("<![CDATA[") {
            rest.find("]]>").map(|index| at + index + 3)
        } else if rest.starts_with("<?") {
            rest.find("?>").map(|index| at + index + 2)
        } else if rest.starts_with("<!") {
            tag_end(bytes, at)
        } else {
            None
        };
        if let Some(end) = standalone {
            nodes.push(Placed {
                depth: stack.len(),
                node: Node::Standalone(&source[at..end]),
            });
            at = end;
            continue;
        }
        // An unterminated `<!--` or `<?` is not markup; neither is a tag with
        // no `>`. Either way the body is left alone.
        if rest.starts_with("<!--") || rest.starts_with("<![CDATA[") || rest.starts_with("<?") {
            return None;
        }
        let end = tag_end(bytes, at)?;
        let raw = &source[at..end];
        if rest.starts_with("</") {
            let name = raw
                .trim_start_matches("</")
                .trim_end_matches('>')
                .trim_end_matches('/')
                .trim();
            let open = stack
                .iter()
                .rposition(|open| open.eq_ignore_ascii_case(name))?;
            stack.truncate(open);
            nodes.push(Placed {
                depth: stack.len(),
                node: Node::Close { name, raw },
            });
            at = end;
            continue;
        }
        let name = element_name(raw)?;
        elements += 1;
        if let Some((_, closes)) = IMPLICIT_CLOSES
            .iter()
            .find(|(element, _)| name.eq_ignore_ascii_case(element))
        {
            while stack.last().is_some_and(|open| {
                closes
                    .iter()
                    .any(|closed| open.eq_ignore_ascii_case(closed))
            }) {
                stack.pop();
            }
        }
        let self_closing = raw.trim_end_matches('>').trim_end().ends_with('/');
        let void = VOID_ELEMENTS
            .iter()
            .any(|element| name.eq_ignore_ascii_case(element));
        if self_closing || void {
            nodes.push(Placed {
                depth: stack.len(),
                node: Node::Empty(raw),
            });
            at = end;
            continue;
        }
        if RAW_TEXT_ELEMENTS
            .iter()
            .any(|element| name.eq_ignore_ascii_case(element))
        {
            nodes.push(Placed {
                depth: stack.len(),
                node: Node::Open { name, raw },
            });
            let close = find_close_tag(source, end, name)?;
            let text = source[end..close].trim();
            if !text.is_empty() {
                nodes.push(Placed {
                    depth: stack.len() + 1,
                    node: Node::Text(text),
                });
            }
            // The close tag itself is read by the next turn of the loop, with
            // the element on the stack so it lands at the right depth.
            stack.push(name);
            at = close;
            continue;
        }
        nodes.push(Placed {
            depth: stack.len(),
            node: Node::Open { name, raw },
        });
        stack.push(name);
        at = end;
    }
    (elements > 0).then_some(nodes)
}

/// The byte just past the `>` that closes the tag opening at `open`, skipping
/// quoted attribute values so a `>` inside one does not end it early. `None`
/// when the tag never closes.
fn tag_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut at = open + 1;
    let mut quote: Option<u8> = None;
    while at < bytes.len() {
        let byte = bytes[at];
        match quote {
            Some(open_quote) if byte == open_quote => quote = None,
            Some(_) => {}
            None if byte == b'"' || byte == b'\'' => quote = Some(byte),
            None if byte == b'>' => return Some(at + 1),
            None => {}
        }
        at += 1;
    }
    None
}

/// The element name in `raw` (`<name …>`), or `None` when what follows `<` is
/// not a name — which is how `a < b` in a plain-text body is kept out of the
/// markup path.
fn element_name(raw: &str) -> Option<&str> {
    let inner = raw.strip_prefix('<')?;
    let end = inner
        .find(|character: char| character.is_whitespace() || character == '>' || character == '/')
        .unwrap_or(inner.len());
    let name = &inner[..end];
    name.starts_with(|first: char| first.is_ascii_alphabetic() || first == '_' || first == ':')
        .then_some(name)
}

/// Where `</name` next appears at or after `from`, matched case-insensitively
/// without lower-casing the rest of the body — a raw-text element in a large
/// HTML page would otherwise re-allocate the remainder once per script tag.
fn find_close_tag(source: &str, from: usize, name: &str) -> Option<usize> {
    let mut at = from;
    while let Some(index) = source[at..].find("</") {
        let start = at + index;
        let after = start + 2;
        if source
            .get(after..after + name.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
        {
            return Some(start);
        }
        at = start + 2;
    }
    None
}

/// Tokenize `text` — expected to be markup put through [`format_xml`] — into
/// coloured byte ranges. Ranges are ascending, disjoint and on char
/// boundaries. Malformed markup still tokenizes: an unclosed tag colours as
/// punctuation rather than disappearing.
pub fn xml_spans(text: &str) -> Vec<(Range<usize>, Token)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'<' {
            let end = text[at..].find('<').map_or(bytes.len(), |index| at + index);
            push_trimmed(&mut spans, text, at..end, Token::Text);
            at = end;
            continue;
        }
        let rest = &text[at..];
        let standalone = if rest.starts_with("<!--") {
            rest.find("-->").map(|index| at + index + 3)
        } else if rest.starts_with("<![CDATA[") {
            rest.find("]]>").map(|index| at + index + 3)
        } else if rest.starts_with("<?") {
            rest.find("?>").map(|index| at + index + 2)
        } else if rest.starts_with("<!") {
            tag_end(bytes, at)
        } else {
            None
        };
        if let Some(end) = standalone {
            spans.push((at..end, Token::Comment));
            at = end;
            continue;
        }
        let Some(end) = tag_end(bytes, at) else {
            spans.push((at..bytes.len(), Token::Punct));
            break;
        };
        push_tag(&mut spans, text, at..end);
        at = end;
    }
    spans
}

/// The tag `<…>` at `range`, split into its bracket, name, attributes and
/// values.
fn push_tag(spans: &mut Vec<(Range<usize>, Token)>, text: &str, range: Range<usize>) {
    let bytes = text.as_bytes();
    let mut at = range.start + 1;
    if bytes.get(at) == Some(&b'/') {
        at += 1;
    }
    spans.push((range.start..at, Token::Punct));
    let name_end = scan_while(bytes, at, range.end, |byte| {
        !matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/')
    });
    if name_end > at {
        spans.push((at..name_end, Token::Tag));
    }
    at = name_end;
    // An unquoted HTML attribute value (`width=100`) is a value, not another
    // attribute name, so the token after `=` is coloured as one.
    let mut expect_value = false;
    while at < range.end {
        match bytes[at] {
            b' ' | b'\t' | b'\n' | b'\r' => at += 1,
            b'>' | b'/' => {
                spans.push((at..at + 1, Token::Punct));
                expect_value = false;
                at += 1;
            }
            b'=' => {
                spans.push((at..at + 1, Token::Punct));
                expect_value = true;
                at += 1;
            }
            quote @ (b'"' | b'\'') => {
                let mut end = at + 1;
                while end < range.end && bytes[end] != quote {
                    end += 1;
                }
                end = (end + 1).min(range.end);
                push_string(spans, text, at..end, Token::Str);
                expect_value = false;
                at = end;
            }
            _ => {
                let end = scan_while(bytes, at, range.end, |byte| {
                    !matches!(
                        byte,
                        b' ' | b'\t' | b'\n' | b'\r' | b'>' | b'/' | b'=' | b'"' | b'\''
                    )
                });
                spans.push((
                    at..end,
                    if expect_value {
                        Token::Str
                    } else {
                        Token::Attr
                    },
                ));
                expect_value = false;
                at = end;
            }
        }
    }
}

fn scan_while(bytes: &[u8], from: usize, limit: usize, keep: impl Fn(u8) -> bool) -> usize {
    let mut at = from;
    while at < limit && keep(bytes[at]) {
        at += 1;
    }
    at
}

/// `range` without its surrounding whitespace, dropped entirely when there is
/// nothing but whitespace in it — indentation carries no colour.
fn push_trimmed(
    spans: &mut Vec<(Range<usize>, Token)>,
    text: &str,
    range: Range<usize>,
    kind: Token,
) {
    let slice = &text[range.clone()];
    let start = range.start + (slice.len() - slice.trim_start().len());
    let end = range.end - (slice.len() - slice.trim_end().len());
    if start < end {
        push_string(spans, text, start..end, kind);
    }
}

/// How much of each body the Diff panes show. The semantic diff underneath is
/// unbounded; this only caps what is painted.
pub const MAX_DIFF_LINES: usize = 400;

/// One painted line of a Diff pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Same(String),
    Removed(String),
    Added(String),
    /// The pane's cap was reached; `usize` lines were left unpainted.
    Elided(usize),
}

/// A line diff of `base` against `head`, split into the two panes: the base
/// pane carries `Same` and `Removed` lines, the head pane `Same` and `Added`.
/// Plain LCS on lines — bodies are bounded, so quadratic is fine — capped per
/// pane at [`MAX_DIFF_LINES`] with an [`DiffLine::Elided`] tail.
pub fn line_diff(base: &str, head: &str) -> (Vec<DiffLine>, Vec<DiffLine>) {
    let old: Vec<&str> = base.lines().collect();
    let new: Vec<&str> = head.lines().collect();
    let (old, old_elided) = capped(&old);
    let (new, new_elided) = capped(&new);
    let n = old.len();
    let m = new.len();
    let mut table = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if old[i] == new[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut left = Vec::new();
    let mut right = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old[i] == new[j] {
            left.push(DiffLine::Same(old[i].to_string()));
            right.push(DiffLine::Same(new[j].to_string()));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            left.push(DiffLine::Removed(old[i].to_string()));
            i += 1;
        } else {
            right.push(DiffLine::Added(new[j].to_string()));
            j += 1;
        }
    }
    left.extend(
        old[i..]
            .iter()
            .map(|line| DiffLine::Removed(line.to_string())),
    );
    right.extend(
        new[j..]
            .iter()
            .map(|line| DiffLine::Added(line.to_string())),
    );
    if old_elided > 0 {
        left.push(DiffLine::Elided(old_elided));
    }
    if new_elided > 0 {
        right.push(DiffLine::Elided(new_elided));
    }
    (left, right)
}

fn capped<'a>(lines: &'a [&'a str]) -> (&'a [&'a str], usize) {
    if lines.len() > MAX_DIFF_LINES {
        (&lines[..MAX_DIFF_LINES], lines.len() - MAX_DIFF_LINES)
    } else {
        (lines, 0)
    }
}

/// The wall-clock a history row shows: `just now`, `4 min ago`, `2 h ago`,
/// `3 d ago` — relative, like the design's WHEN column.
pub fn relative_time(then_ms: i64, now_ms: i64) -> String {
    let delta = (now_ms - then_ms).max(0) / 1000;
    if delta < 60 {
        "just now".into()
    } else if delta < 3_600 {
        format!("{} min ago", delta / 60)
    } else if delta < 86_400 {
        format!("{} h ago", delta / 3_600)
    } else {
        format!("{} d ago", delta / 86_400)
    }
}

/// `1.2 KB`-style size for the response readout.
pub fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024. * 1024.))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_json_moves_whitespace_and_nothing_else() {
        let formatted = format_json(
            r#"{"name":"{{pet}}","tags":[ "a" ,"b"],"price":1.50,"meta":{},"ids":[],"nested":{"ok":true,"none":null}}"#,
        )
        .unwrap();
        assert_eq!(
            formatted,
            "{\n  \"name\": \"{{pet}}\",\n  \"tags\": [\n    \"a\",\n    \"b\"\n  ],\n  \"price\": 1.50,\n  \"meta\": {},\n  \"ids\": [],\n  \"nested\": {\n    \"ok\": true,\n    \"none\": null\n  }\n}"
        );
        // Already formatted text is a fixed point, and escaped quotes or
        // braces inside strings never open a level.
        assert_eq!(format_json(&formatted).as_deref(), Some(formatted.as_str()));
        assert_eq!(
            format_json(r#"{"quote":"say \"{\"","brace":"}"}"#).as_deref(),
            Some("{\n  \"quote\": \"say \\\"{\\\"\",\n  \"brace\": \"}\"\n}")
        );
        // Not JSON: an unquoted template or a half-typed body stays untouched.
        assert_eq!(format_json(r#"{"id": {{pet_id}}}"#), None);
        assert_eq!(format_json(r#"{"id": "#), None);
        assert_eq!(format_json(""), None);
    }

    fn slices<'a>(spans: &[(Range<usize>, Token)], text: &'a str) -> Vec<(&'a str, Token)> {
        spans
            .iter()
            .map(|(range, kind)| (&text[range.clone()], *kind))
            .collect()
    }

    fn kinds(text: &str) -> Vec<(&str, Token)> {
        json_spans(text)
            .into_iter()
            .map(|(range, kind)| (&text[range], kind))
            .collect()
    }

    #[test]
    fn keys_strings_literals_and_punctuation_are_told_apart() {
        let text =
            "{\n  \"id\": 42,\n  \"name\": \"widget\",\n  \"ok\": true,\n  \"none\": null\n}";
        let spans = kinds(text);
        assert_eq!(spans[0], ("{", Token::Punct));
        assert!(spans.contains(&("\"id\"", Token::Key)));
        assert!(spans.contains(&("42", Token::Literal)));
        assert!(spans.contains(&("\"widget\"", Token::Str)));
        assert!(spans.contains(&("true", Token::Literal)));
        assert!(spans.contains(&("null", Token::Literal)));
        assert!(!spans.contains(&("\"widget\"", Token::Key)));
        // Ranges ascend and never overlap.
        let ranges = json_spans(text);
        for pair in ranges.windows(2) {
            assert!(pair[0].0.end <= pair[1].0.start, "{pair:?}");
        }
    }

    #[test]
    fn variables_inside_strings_get_their_own_span() {
        let text = r#"{"url": "{{service_url}}/items", "k": "{{a}}{{b}}"}"#;
        let spans = kinds(text);
        assert!(spans.contains(&("{{service_url}}", Token::Var)));
        assert!(spans.contains(&("/items\"", Token::Str)));
        assert!(spans.contains(&("\"", Token::Str)));
        assert!(spans.contains(&("{{a}}", Token::Var)));
        assert!(spans.contains(&("{{b}}", Token::Var)));
        assert!(spans.contains(&("\"url\"", Token::Key)));
    }

    #[test]
    fn escapes_and_multibyte_text_stay_on_char_boundaries() {
        let text = "{\"quote\": \"a \\\" b\", \"emoji\": \"héllo 🎉\", \"unclosed\": \"oops";
        for (range, _) in json_spans(text) {
            assert!(text.is_char_boundary(range.start));
            assert!(text.is_char_boundary(range.end));
        }
        assert!(kinds(text).contains(&("\"a \\\" b\"", Token::Str)));
        assert!(kinds(text).contains(&("\"héllo 🎉\"", Token::Str)));
        assert!(kinds(text).contains(&("\"oops", Token::Str)));
    }

    #[test]
    fn non_json_still_tokenizes_without_panicking() {
        assert!(json_spans("").is_empty());
        let spans = kinds("plain text body");
        assert_eq!(spans.len(), 3);
        assert!(spans.iter().all(|(_, kind)| *kind == Token::Literal));
    }

    #[test]
    fn format_xml_indents_by_depth_and_keeps_leaves_on_one_line() {
        let formatted = format_xml(
            r#"<?xml version="1.0"?><catalog count="2"><!-- stock --><item id="7"><name>Widget &amp; co</name><tags><tag>a</tag></tags><empty/></item><item id="8"><name/></item></catalog>"#,
        )
        .unwrap();
        assert_eq!(
            formatted,
            concat!(
                "<?xml version=\"1.0\"?>\n",
                "<catalog count=\"2\">\n",
                "  <!-- stock -->\n",
                "  <item id=\"7\">\n",
                "    <name>Widget &amp; co</name>\n",
                "    <tags>\n",
                "      <tag>a</tag>\n",
                "    </tags>\n",
                "    <empty/>\n",
                "  </item>\n",
                "  <item id=\"8\">\n",
                "    <name/>\n",
                "  </item>\n",
                "</catalog>"
            )
        );
        // Re-indenting is a fixed point, and an entity or a `>` inside an
        // attribute value survives it byte for byte.
        assert_eq!(format_xml(&formatted).as_deref(), Some(formatted.as_str()));
        assert_eq!(
            format_xml(r#"<a t="x > y"><b>1 &lt; 2</b></a>"#).as_deref(),
            Some("<a t=\"x > y\">\n  <b>1 &lt; 2</b>\n</a>")
        );
    }

    #[test]
    fn html_void_elements_raw_text_and_implicit_closes_come_out_at_the_right_depth() {
        // `<br>` and `<meta>` never close, `<script>`'s `a < b` opens no
        // element, and `</ul>` closes both unclosed `<li>`s at once — so the
        // `</ul>` lands back beside its own `<ul>`.
        let formatted = format_xml(
            "<html><head><meta charset=\"utf-8\"><script>if (a < b) { x(); }</script></head>\
             <body><ul><li>one<li>two</ul><br><p>done</p></body></html>",
        )
        .unwrap();
        assert_eq!(
            formatted,
            concat!(
                "<html>\n",
                "  <head>\n",
                "    <meta charset=\"utf-8\">\n",
                "    <script>if (a < b) { x(); }</script>\n",
                "  </head>\n",
                "  <body>\n",
                "    <ul>\n",
                "      <li>\n",
                "        one\n",
                "      <li>\n",
                "        two\n",
                "    </ul>\n",
                "    <br>\n",
                "    <p>done</p>\n",
                "  </body>\n",
                "</html>"
            )
        );
        // The second `<li>` sits beside the first, not inside it: without the
        // implicit close a hundred-item list would indent a hundred levels.
        let rows = format_xml(&format!(
            "<table>{}</table>",
            "<tr><td>a</td><td>b</td>".repeat(3)
        ))
        .unwrap();
        assert_eq!(
            rows.lines()
                .filter(|line| line.trim_start().starts_with("<tr>"))
                .collect::<Vec<_>>(),
            vec!["  <tr>"; 3]
        );
        assert!(
            rows.lines()
                .all(|line| line.len() - line.trim_start().len() <= 4),
            "no row indents past its cells:\n{rows}"
        );
    }

    #[test]
    fn format_xml_refuses_anything_it_cannot_walk() {
        // No element at all, a comparison that only looks like a tag, a tag
        // that never closes, a stray close tag, and an unterminated comment
        // or raw-text element. Each is left to the caller to show verbatim.
        for source in [
            "",
            "plain text",
            "1 < 2 and 3 > 2",
            "<open",
            "<a></b></a>",
            "<a><!-- forever</a>",
            "<a><script>never</a>",
        ] {
            assert_eq!(format_xml(source), None, "{source:?}");
        }
    }

    #[test]
    fn xml_spans_tell_names_attributes_values_and_comments_apart() {
        let text = "<item id=\"7\" open width=100>text &amp; more</item><!-- note -->";
        let spans = slices(&xml_spans(text), text);
        assert!(spans.contains(&("<", Token::Punct)));
        assert!(spans.contains(&("item", Token::Tag)));
        assert!(spans.contains(&("id", Token::Attr)));
        assert!(spans.contains(&("\"7\"", Token::Str)));
        assert!(spans.contains(&("open", Token::Attr)));
        // An unquoted HTML value is a value, not a second attribute name.
        assert!(spans.contains(&("100", Token::Str)));
        assert!(spans.contains(&("text &amp; more", Token::Text)));
        assert!(spans.contains(&("</", Token::Punct)));
        assert!(spans.contains(&("<!-- note -->", Token::Comment)));
        // Indentation carries no colour, and ranges stay ordered, disjoint and
        // on char boundaries even through multibyte text and an unclosed tag.
        let text = "<a>\n  <b t=\"{{host}}/é\">héllo 🎉</b>\n</a>\n<unclosed";
        let spans = xml_spans(text);
        for pair in spans.windows(2) {
            assert!(pair[0].0.end <= pair[1].0.start, "{pair:?}");
        }
        for (range, _) in &spans {
            assert!(text.is_char_boundary(range.start));
            assert!(text.is_char_boundary(range.end));
        }
        let kinds = slices(&spans, text);
        assert!(kinds.contains(&("{{host}}", Token::Var)));
        assert!(kinds.contains(&("héllo 🎉", Token::Text)));
        assert!(kinds.contains(&("<unclosed", Token::Punct)));
        assert!(!kinds.iter().any(|(slice, _)| slice.trim().is_empty()));
    }

    #[test]
    fn present_picks_the_format_from_the_body_not_the_header() {
        // JSON mislabelled `text/plain` still formats, and it keeps the wire
        // spelling of its numbers — a `to_string_pretty` round trip would
        // rewrite `1.50` as `1.5` and `1e2` as `100.0`.
        let (format, text) = present(Some("text/plain"), r#"{"a":1.50,"b":1e2}"#);
        assert_eq!(format, BodyFormat::Json);
        assert_eq!(text, "{\n  \"a\": 1.50,\n  \"b\": 1e2\n}");
        // A header promising JSON does not make an HTML error page parse.
        let (format, text) = present(Some("application/json"), "<html><b>502</b></html>");
        assert_eq!(format, BodyFormat::Xml);
        assert_eq!(text, "<html>\n  <b>502</b>\n</html>");
        // Markup only the header knows about, and a body that is neither.
        assert_eq!(
            present(Some("application/soap+xml"), "<a><b>1</b></a>").0,
            BodyFormat::Xml
        );
        let (format, text) = present(Some("text/csv"), "a,b\n1,2");
        assert_eq!(format, BodyFormat::Text);
        assert_eq!(text, "a,b\n1,2", "an unformattable body is left alone");
        assert_eq!(present(None, "1 < 2").0, BodyFormat::Text);
        assert!(spans("a,b\n1,2", BodyFormat::Text).is_empty());
    }

    #[test]
    fn large_response_preparation_is_virtualized_and_keeps_the_tail() {
        let tail = "THE-END-MUST-SURVIVE";
        let body = format!("{}\n{tail}", "a".repeat(VIRTUALIZED_BODY_BYTES));
        let prepared = prepare(Some("text/plain"), &body, false);

        assert_eq!(prepared.raw.as_ref(), body);
        let BodyPresentation::Virtualized { text, lines, .. } = prepared.presentation else {
            panic!("large response should use the virtualized reader");
        };
        let tail_row = lines.last().expect("tail line");
        assert_eq!(&text[tail_row.range.clone()], tail);
        assert_eq!(tail_row.line_number, Some(2));
        assert!(lines.len() > 2, "the long first line is chunked");
    }

    #[test]
    fn virtualized_lines_preserve_a_trailing_empty_row() {
        let body = format!("{}\nend\n", "x".repeat(VIRTUALIZED_BODY_BYTES));
        let prepared = prepare(None, &body, false);
        let BodyPresentation::Virtualized { text, lines, .. } = prepared.presentation else {
            panic!("large response should use the virtualized reader");
        };
        assert_eq!(&text[lines[lines.len() - 2].range.clone()], "end");
        assert_eq!(lines[lines.len() - 2].line_number, Some(2));
        assert_eq!(&text[lines.last().unwrap().range.clone()], "");
        assert_eq!(lines.last().unwrap().line_number, Some(3));
        assert!(
            lines
                .iter()
                .all(|row| row.range.end - row.range.start <= VIRTUAL_ROW_BYTES)
        );
    }

    #[test]
    fn line_diff_splits_into_two_panes() {
        let (left, right) = line_diff("a\nb\nc", "a\nx\nc\nd");
        assert_eq!(
            left,
            vec![
                DiffLine::Same("a".into()),
                DiffLine::Removed("b".into()),
                DiffLine::Same("c".into()),
            ]
        );
        assert_eq!(
            right,
            vec![
                DiffLine::Same("a".into()),
                DiffLine::Added("x".into()),
                DiffLine::Same("c".into()),
                DiffLine::Added("d".into()),
            ]
        );
        let (left, right) = line_diff("same", "same");
        assert_eq!(left, right);
        assert_eq!(left, vec![DiffLine::Same("same".into())]);
    }

    #[test]
    fn line_diff_caps_each_pane_and_says_how_much_it_left_out() {
        let long = (0..MAX_DIFF_LINES + 25)
            .map(|index| index.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let (left, right) = line_diff(&long, "0");
        assert_eq!(left.len(), MAX_DIFF_LINES + 1);
        assert_eq!(left.last(), Some(&DiffLine::Elided(25)));
        assert_eq!(right, vec![DiffLine::Same("0".into())]);
    }

    #[test]
    fn readouts_format_like_the_design() {
        assert_eq!(relative_time(1_000, 5_000), "just now");
        assert_eq!(relative_time(0, 4 * 60_000), "4 min ago");
        assert_eq!(relative_time(0, 2 * 3_600_000), "2 h ago");
        assert_eq!(relative_time(0, 3 * 86_400_000), "3 d ago");
        assert_eq!(relative_time(9_000, 1_000), "just now");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1_536), "1.5 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }
}
