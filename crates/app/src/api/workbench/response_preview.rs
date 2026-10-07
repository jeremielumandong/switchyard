//! Static response previews. Only structural HTML is retained; no resources or actions.
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};

pub(super) fn sanitize(source: &str, markdown: bool) -> Result<String, String> {
    if source.len() > 1024 * 1024 {
        return Err(
            "Preview supports responses up to 1 MiB. Use Pretty or Raw for larger bodies.".into(),
        );
    }
    let html = if markdown {
        markdown::to_html(source)
    } else {
        source.to_string()
    };
    let dom = html5ever::parse_document(RcDom::default(), Default::default()).one(html);
    let mut output = String::new();
    append(&dom.document, &mut output, 0)?;
    Ok(output)
}

fn append(node: &Handle, output: &mut String, depth: usize) -> Result<(), String> {
    if depth > 128 {
        return Err("This document is too deeply nested to preview. Use Raw.".into());
    }
    let mut tag = None;
    match &node.data {
        NodeData::Text { contents } => {
            for c in contents.borrow().chars() {
                match c {
                    '&' => output.push_str("&amp;"),
                    '<' => output.push_str("&lt;"),
                    '>' => output.push_str("&gt;"),
                    _ => output.push(c),
                }
            }
        }
        NodeData::Element { name, .. } => {
            let name = name.local.as_ref();
            if matches!(
                name,
                "script"
                    | "style"
                    | "iframe"
                    | "object"
                    | "embed"
                    | "img"
                    | "video"
                    | "audio"
                    | "source"
                    | "link"
                    | "meta"
                    | "svg"
                    | "math"
                    | "head"
                    | "input"
                    | "button"
                    | "textarea"
                    | "select"
            ) {
                return Ok(());
            }
            if matches!(
                name,
                "p" | "div"
                    | "span"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "ul"
                    | "ol"
                    | "li"
                    | "blockquote"
                    | "pre"
                    | "code"
                    | "strong"
                    | "b"
                    | "em"
                    | "i"
                    | "u"
                    | "s"
                    | "table"
                    | "thead"
                    | "tbody"
                    | "tr"
                    | "th"
                    | "td"
                    | "br"
                    | "hr"
            ) {
                output.push('<');
                output.push_str(name);
                output.push('>');
                if !matches!(name, "br" | "hr") {
                    tag = Some(name);
                }
            }
        }
        _ => {}
    }
    for child in node.children.borrow().iter() {
        append(child, output, depth + 1)?;
    }
    if let Some(tag) = tag {
        output.push_str("</");
        output.push_str(tag);
        output.push('>');
    }
    if output.len() > 4 * 1024 * 1024 {
        return Err("Preview output is too large. Use Raw.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn html_preview_drops_resources_scripts_attributes_and_actions() {
        let output = sanitize(r#"<h1 onclick="bad()">Hello</h1><script>attack()</script><img src="https://remote.test"><a href="file:///secret">read</a><p style="background:url(x)">A &amp; B</p><svg><image href="x"/></svg>"#, false).unwrap();
        assert_eq!(output, "<h1>Hello</h1>read<p>A &amp; B</p>");
    }
    #[test]
    fn markdown_preview_preserves_formatting_without_fetching_images() {
        let output = sanitize("# Title\n\n**Strong** ![remote](https://remote.test/image) [Link](https://remote.test)", true).unwrap();
        assert!(output.contains("<h1>Title</h1>"));
        assert!(output.contains("<strong>Strong</strong>"));
        assert!(!output.contains("https://"));
        assert!(!output.contains("<img"));
    }
}
