//! A small element tree over `quick-xml` for the storage APIs' list responses.

use quick_xml::events::Event;

use crate::error::{CloudError, Result};

/// One element: local name, its text, and its children.
#[derive(Debug, Default)]
pub(crate) struct Node {
    pub(crate) name: String,
    pub(crate) text: String,
    pub(crate) children: Vec<Node>,
}

impl Node {
    /// The first child named `name`.
    pub(crate) fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    /// Every child named `name`.
    pub(crate) fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// Text of the first child named `name`.
    pub(crate) fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.as_str())
    }

    /// Text at a path of child names.
    pub(crate) fn path(&self, path: &[&str]) -> Option<&str> {
        let mut n = self;
        for p in path {
            n = n.child(p)?;
        }
        Some(n.text.as_str())
    }
}

/// Parse a document into its root element.
pub(crate) fn parse(xml: &[u8]) -> Result<Node> {
    let bad = |e: &dyn std::fmt::Display| CloudError::Network(format!("unreadable XML reply: {e}"));
    let mut reader = quick_xml::Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut stack: Vec<Node> = vec![Node::default()];
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf).map_err(|e| bad(&e))? {
            Event::Start(e) => stack.push(Node {
                name: e.local_name().as_ref().to_owned(),
                ..Node::default()
            }),
            Event::Empty(e) => {
                let node = Node {
                    name: e.local_name().as_ref().to_owned(),
                    ..Node::default()
                };
                if let Some(top) = stack.last_mut() {
                    top.children.push(node);
                }
            }
            Event::End(_) => {
                let Some(node) = stack.pop() else {
                    return Err(bad(&"unbalanced"));
                };
                match stack.last_mut() {
                    Some(top) => top.children.push(node),
                    None => return Err(bad(&"unbalanced")),
                }
            }
            Event::Text(t) => {
                let s = t.xml10_content();
                let s = quick_xml::escape::unescape(&s).map_err(|e| bad(&e))?;
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&s);
                }
            }
            Event::GeneralRef(r) => {
                let name = r.xml10_content();
                let s = match name.as_ref() {
                    "amp" => "&".to_owned(),
                    "lt" => "<".to_owned(),
                    "gt" => ">".to_owned(),
                    "quot" => "\"".to_owned(),
                    "apos" => "'".to_owned(),
                    other => match r.resolve_char_ref().map_err(|e| bad(&e))? {
                        Some(c) => c.to_string(),
                        None => format!("&{other};"),
                    },
                };
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&s);
                }
            }
            Event::CData(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t.into_inner());
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    let mut root = stack.pop().unwrap_or_default();
    if !stack.is_empty() {
        return Err(bad(&"unclosed element"));
    }
    // The document element is the dummy root's only child.
    root.children.pop().ok_or_else(|| bad(&"empty document"))
}

/// Escape text for an XML body.
pub(crate) fn escape(s: &str) -> String {
    quick_xml::escape::escape(s).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree() {
        let doc = br#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>b</Name><IsTruncated>false</IsTruncated>
  <Contents><Key>a &amp; b.txt</Key><Size>5</Size></Contents>
  <Contents><Key>c&#233;.txt</Key><Size>7</Size></Contents>
  <CommonPrefixes><Prefix>dir/</Prefix></CommonPrefixes>
  <Empty/>
</ListBucketResult>"#;
        let root = parse(doc).unwrap();
        assert_eq!(root.name, "ListBucketResult");
        let keys: Vec<_> = root
            .all("Contents")
            .map(|c| c.text_of("Key").unwrap().to_owned())
            .collect();
        assert_eq!(keys, ["a & b.txt", "cé.txt"]);
        assert_eq!(root.path(&["CommonPrefixes", "Prefix"]), Some("dir/"));
        assert!(root.child("Empty").is_some());
        assert_eq!(escape("a<b&c"), "a&lt;b&amp;c");
    }
}
