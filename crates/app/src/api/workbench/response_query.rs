//! Response inspection only: queries never change the stored response.

pub(super) fn filter(body: &str, query: &str) -> Result<(String, usize), String> {
    if body.len() > 4 * 1024 * 1024 {
        return Err("Response filtering supports documents up to 4 MiB. Save this response for external inspection.".into());
    }
    let query = query.trim();
    if query.len() > 4096 {
        return Err("Filter expressions must be at most 4096 bytes.".into());
    }
    if query.is_empty() {
        return Err("Enter a JSONPath or XPath expression.".into());
    }
    if body.trim_start().starts_with('<') {
        let package =
            sxd_document::parser::parse(body).map_err(|error| format!("Invalid XML: {error}"))?;
        let document = package.as_document();
        let value = sxd_xpath::evaluate_xpath(&document, query)
            .map_err(|error| format!("Invalid XPath: {error}"))?;
        match value {
            sxd_xpath::Value::Nodeset(nodes) => {
                if nodes.size() > 10_000 {
                    return Err("More than 10,000 matches. Use a narrower filter.".into());
                }
                let values: Vec<_> = nodes
                    .document_order()
                    .into_iter()
                    .map(|node| node.string_value())
                    .collect();
                let count = values.len();
                if count > 10_000 {
                    return Err("More than 10,000 matches. Use a narrower filter.".into());
                }
                Ok((bounded_json(&values)?, count))
            }
            value => Ok((value.string(), 1)),
        }
    } else {
        let value: serde_json::Value =
            serde_json::from_str(body).map_err(|error| format!("Invalid JSON: {error}"))?;
        let path = serde_json_path::JsonPath::parse(query)
            .map_err(|error| format!("Invalid JSONPath: {error}"))?;
        let nodes = path.query(&value).all();
        let count = nodes.len();
        if count > 10_000 {
            return Err("More than 10,000 matches. Use a narrower filter.".into());
        }
        Ok((bounded_json(&nodes)?, count))
    }
}

fn bounded_json(value: &impl serde::Serialize) -> Result<String, String> {
    struct Output(Vec<u8>);
    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0.len().saturating_add(bytes.len()) > 4 * 1024 * 1024 {
                return Err(std::io::Error::other(
                    "Filtered output exceeds 4 MiB. Use a narrower filter.",
                ));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Output(Vec::new());
    serde_json::to_writer_pretty(&mut output, value).map_err(|e| e.to_string())?;
    String::from_utf8(output.0).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonpath_supports_predicates_and_recursive_descent() {
        let body = r#"{"items":[{"name":"a","price":3},{"name":"b","price":12}]}"#;
        let (text, count) = filter(body, "$.items[?@.price < 10].name").unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            serde_json::json!(["a"])
        );
        assert_eq!(filter(body, "$..price").unwrap().1, 2);
        assert_eq!(filter(body, "$.missing").unwrap().1, 0);
        assert!(filter(body, "$[").is_err());
    }

    #[test]
    fn xpath_supports_attributes_predicates_and_scalars() {
        let body = r#"<items><item id="a">First</item><item id="b">Second</item></items>"#;
        assert_eq!(
            filter(body, "//item[@id='b']/text()").unwrap(),
            ("[\n  \"Second\"\n]".into(), 1)
        );
        assert_eq!(filter(body, "count(//item)").unwrap(), ("2".into(), 1));
        assert_eq!(filter(body, "//missing").unwrap().1, 0);
        assert!(filter(body, "//[").is_err());
        assert!(filter("<broken>", "//broken").is_err());
    }
}
