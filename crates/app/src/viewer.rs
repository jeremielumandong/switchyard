//! Value viewer helpers: pretty XML with token kinds, image detection, hex rows.

use gpui_kit::ImageFormat;
use quick_xml::events::Event;
use switchyard_core::db::Value;

/// What a piece of formatted XML is, for colouring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tok {
    /// `<`, `>`, `/>`, `=`, quotes.
    Punct,
    /// Element name.
    Tag,
    /// Attribute name.
    Attr,
    /// Attribute value.
    Value,
    /// Text content.
    Text,
    /// Comments, declarations, processing instructions.
    Meta,
}

/// One displayed line: indented pieces.
pub type Line = Vec<(String, Tok)>;

const INDENT: &str = "  ";

fn lossy(s: &str) -> String {
    s.to_owned()
}

/// `src` indented one element per line (short text stays on its element's line), at most
/// `max_lines` lines. Errors when `src` isn't well-formed XML.
pub fn xml_lines(src: &str, max_lines: usize) -> Result<(Vec<Line>, bool), String> {
    let mut reader = quick_xml::Reader::from_str(src);
    // Whitespace-only text is skipped below; trimming here would also eat the spaces
    // around entity references (`Tea &amp; cake`).
    reader.config_mut().trim_text(false);
    let mut out: Vec<Line> = Vec::new();
    let mut depth = 0usize;
    // An open element whose closing tag may join its line (`<a>text</a>`).
    let mut open_line: Option<usize> = None;
    let mut saw_element = false;
    let indent = |d: usize| (INDENT.repeat(d), Tok::Punct);
    loop {
        if out.len() >= max_lines {
            return Ok((out, true));
        }
        let ev = reader
            .read_event()
            .map_err(|e| format!("not XML at byte {}: {e}", reader.buffer_position()))?;
        match ev {
            Event::Start(e) => {
                saw_element = true;
                let mut line = vec![indent(depth), ("<".into(), Tok::Punct)];
                line.push((lossy(e.name().0), Tok::Tag));
                push_attrs(&mut line, &e);
                line.push((">".into(), Tok::Punct));
                out.push(line);
                open_line = Some(out.len() - 1);
                depth += 1;
            }
            Event::Empty(e) => {
                saw_element = true;
                let mut line = vec![indent(depth), ("<".into(), Tok::Punct)];
                line.push((lossy(e.name().0), Tok::Tag));
                push_attrs(&mut line, &e);
                line.push(("/>".into(), Tok::Punct));
                out.push(line);
                open_line = None;
            }
            Event::End(e) => {
                depth = depth.saturating_sub(1);
                let close = [
                    ("</".to_owned(), Tok::Punct),
                    (lossy(e.name().0), Tok::Tag),
                    (">".to_owned(), Tok::Punct),
                ];
                match open_line.take() {
                    // `<a>text</a>` or `<a></a>`: close on the same line.
                    Some(i) if i + 2 >= out.len() && joinable(&out, i) => {
                        if i + 1 < out.len() {
                            let mut text = out.remove(i + 1);
                            trim_run_end(&mut text);
                            out[i].extend(text.into_iter().skip(1));
                        }
                        out[i].extend(close);
                    }
                    _ => {
                        if let Some(l) = out.last_mut() {
                            trim_run_end(l);
                        }
                        let mut line = vec![indent(depth)];
                        line.extend(close);
                        out.push(line);
                    }
                }
            }
            Event::Text(t) => {
                let s = lossy(&t);
                if s.trim().is_empty() {
                    continue;
                }
                match out.last_mut() {
                    // Text after an entity reference continues the same run.
                    Some(l) if l.last().is_some_and(|(_, t)| *t == Tok::Text) => {
                        l.push((s, Tok::Text))
                    }
                    _ => out.push(vec![indent(depth), (s.trim_start().to_owned(), Tok::Text)]),
                }
            }
            Event::GeneralRef(r) => {
                let s = format!("&{};", lossy(&r));
                match out.last_mut() {
                    Some(l) if l.last().is_some_and(|(_, t)| *t == Tok::Text) => {
                        l.push((s, Tok::Text))
                    }
                    _ => out.push(vec![indent(depth), (s, Tok::Text)]),
                }
            }
            Event::CData(c) => out.push(vec![
                indent(depth),
                ("<![CDATA[".into(), Tok::Meta),
                (lossy(&c), Tok::Text),
                ("]]>".into(), Tok::Meta),
            ]),
            Event::Comment(c) => out.push(vec![
                indent(depth),
                (format!("<!--{}-->", lossy(&c)), Tok::Meta),
            ]),
            Event::Decl(d) => out.push(vec![(format!("<?{}?>", lossy(&d)), Tok::Meta)]),
            Event::PI(p) => out.push(vec![
                indent(depth),
                (format!("<?{}?>", lossy(&p)), Tok::Meta),
            ]),
            Event::DocType(d) => out.push(vec![(format!("<!DOCTYPE {}>", lossy(&d)), Tok::Meta)]),
            Event::Eof => break,
        }
    }
    if !saw_element {
        return Err("no XML elements".into());
    }
    if depth != 0 {
        return Err("unclosed element".into());
    }
    Ok((out, false))
}

/// Drop trailing whitespace from a line that ends in text.
fn trim_run_end(line: &mut Line) {
    if let Some((s, Tok::Text)) = line.last_mut() {
        let t = s.trim_end().len();
        s.truncate(t);
    }
}

/// The element on line `i` can take its text and closing tag on the same line.
fn joinable(out: &[Line], i: usize) -> bool {
    match out.get(i + 1) {
        None => true,
        Some(text) => {
            text.iter().skip(1).all(|(_, t)| *t == Tok::Text)
                && text.iter().map(|(s, _)| s.len()).sum::<usize>() <= 80
        }
    }
}

fn push_attrs(line: &mut Line, e: &quick_xml::events::BytesStart<'_>) {
    for a in e.attributes().with_checks(false).flatten() {
        line.push((" ".into(), Tok::Punct));
        line.push((lossy(a.key.0), Tok::Attr));
        line.push(("=\"".into(), Tok::Punct));
        line.push((lossy(&a.value), Tok::Value));
        line.push(("\"".into(), Tok::Punct));
    }
}

/// The bytes a value holds: binary as is, anything else as its UTF-8 text.
pub fn value_bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::Null => Vec::new(),
        Value::Bytes(b) => b.clone(),
        Value::Text(s) | Value::Json(s) | Value::Numeric(s) | Value::Other(s) => {
            s.as_bytes().to_vec()
        }
        other => other.to_display().into_bytes(),
    }
}

/// The image format of `bytes`, from their signature.
pub fn image_format(bytes: &[u8]) -> Option<ImageFormat> {
    let starts = |sig: &[u8]| bytes.starts_with(sig);
    if starts(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if starts(b"\xff\xd8\xff") {
        Some(ImageFormat::Jpeg)
    } else if starts(b"GIF87a") || starts(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if bytes.len() >= 12 && starts(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(ImageFormat::Webp)
    } else if starts(b"BM") && bytes.len() > 14 {
        Some(ImageFormat::Bmp)
    } else if starts(b"II*\0") || starts(b"MM\0*") {
        Some(ImageFormat::Tiff)
    } else {
        // SVG is text: an <svg element near the start.
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).to_lowercase();
        let t = head.trim_start();
        ((t.starts_with("<?xml") || t.starts_with("<svg") || t.starts_with("<!--"))
            && head.contains("<svg"))
        .then_some(ImageFormat::Svg)
    }
}

/// Short name of a format for the footer.
pub fn format_name(f: ImageFormat) -> &'static str {
    match f {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        ImageFormat::Gif => "GIF",
        ImageFormat::Webp => "WebP",
        ImageFormat::Bmp => "BMP",
        ImageFormat::Tiff => "TIFF",
        ImageFormat::Svg => "SVG",
        _ => "image",
    }
}

/// `(offset, hex, ascii)` rows of 8 bytes, at most `max_rows`.
pub fn hex_rows(bytes: &[u8], max_rows: usize) -> Vec<(String, String, String)> {
    bytes
        .chunks(8)
        .take(max_rows)
        .enumerate()
        .map(|(i, chunk)| {
            let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
            let ascii: String = chunk
                .iter()
                .map(|b| {
                    if (32..127).contains(b) {
                        *b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            (format!("{:04x}", i * 8), hex.join(" "), ascii)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.iter().map(|(s, _)| s.as_str()).collect())
            .collect()
    }

    #[test]
    fn xml_is_indented_with_short_text_inline() {
        let (lines, cut) = xml_lines(
            r#"<?xml version="1.0"?><order id="7"><item sku="a&amp;b">Tea &amp; cake</item><note/><!-- x --></order>"#,
            100,
        )
        .unwrap();
        assert!(!cut);
        assert_eq!(
            text(&lines),
            [
                r#"<?xml version="1.0"?>"#,
                r#"<order id="7">"#,
                r#"  <item sku="a&amp;b">Tea &amp; cake</item>"#,
                "  <note/>",
                "  <!-- x -->",
                "</order>",
            ]
        );
        let kinds: Vec<Tok> = lines[2].iter().map(|(_, t)| *t).collect();
        assert!(kinds.contains(&Tok::Attr) && kinds.contains(&Tok::Value));
    }

    #[test]
    fn not_xml_and_long_documents() {
        assert!(xml_lines("hello", 10).is_err());
        assert!(xml_lines("<a><b></a>", 10).is_err());
        let big = format!("<r>{}</r>", "<i/>".repeat(50));
        let (lines, cut) = xml_lines(&big, 10).unwrap();
        assert!(cut);
        assert_eq!(lines.len(), 10);
    }

    #[test]
    fn images_by_signature() {
        assert_eq!(
            image_format(b"\x89PNG\r\n\x1a\n...."),
            Some(ImageFormat::Png)
        );
        assert_eq!(image_format(b"\xff\xd8\xff\xe0"), Some(ImageFormat::Jpeg));
        assert_eq!(
            image_format(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(ImageFormat::Webp)
        );
        assert_eq!(
            image_format(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"),
            Some(ImageFormat::Svg)
        );
        assert_eq!(image_format(b"hello"), None);
        assert_eq!(image_format(b""), None);
    }

    #[test]
    fn hex() {
        let rows = hex_rows(b"Hi\x00there!!", 10);
        assert_eq!(
            rows[0],
            (
                "0000".into(),
                "48 69 00 74 68 65 72 65".into(),
                "Hi.there".into()
            )
        );
        assert_eq!(rows[1].0, "0008");
        assert_eq!(hex_rows(&[0; 100], 3).len(), 3);
    }
}
