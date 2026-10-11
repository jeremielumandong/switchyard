//! Settings to and from files: flat or nested JSON (`{"app:db:host": "x"}`), `.env`
//! (`APP__DB__HOST=x`, the .NET environment variable form of `app:db:host`) and App
//! Configuration's own `kvset` JSON, which keeps labels, content types and tags.

use std::collections::BTreeMap;

use serde_json::{Map, Value as Json, json};

use crate::error::{CloudError, Result};
use crate::kv::{KvItem, KvWrite};

/// A file format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvFormat {
    /// `{"key": "value"}`; nested objects are read as `parent:child` keys.
    Json,
    /// `KEY=value` lines; `:` in keys is written as `__`.
    Env,
    /// `{"items": [{"key", "label", "value", "content_type", "tags"}]}`.
    KvSet,
}

impl KvFormat {
    /// Every format, for menus.
    pub const ALL: [KvFormat; 3] = [KvFormat::Json, KvFormat::Env, KvFormat::KvSet];

    /// Menu title.
    pub fn title(self) -> &'static str {
        match self {
            KvFormat::Json => "JSON (key: value)",
            KvFormat::Env => ".env",
            KvFormat::KvSet => "JSON with labels, content types and tags",
        }
    }

    /// Default file name.
    pub fn file_name(self) -> &'static str {
        match self {
            KvFormat::Json => "settings.json",
            KvFormat::Env => "settings.env",
            KvFormat::KvSet => "settings.kvset.json",
        }
    }

    /// The format of a file, from its name and contents.
    pub fn detect(name: &str, text: &str) -> KvFormat {
        let lower = name.to_ascii_lowercase();
        let t = text.trim_start();
        if !t.starts_with('{') && !t.starts_with('[') {
            return if lower.ends_with(".json") {
                KvFormat::Json
            } else {
                KvFormat::Env
            };
        }
        match serde_json::from_str::<Json>(text) {
            Ok(j) if kvset_items(&j).is_some() => KvFormat::KvSet,
            _ => KvFormat::Json,
        }
    }
}

/// The `items` of a kvset document (also a bare array of settings).
fn kvset_items(j: &Json) -> Option<&Vec<Json>> {
    let a = match j {
        Json::Array(a) => a,
        Json::Object(o) => o.get("items")?.as_array()?,
        _ => return None,
    };
    a.iter()
        .all(|i| i.get("key").is_some_and(Json::is_string))
        .then_some(a)
}

/// `app:db:host` as an environment variable name (`app__db__host`).
fn env_name(key: &str) -> String {
    key.replace(':', "__")
}

fn env_quote(v: &str) -> String {
    let plain = !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@,+%".contains(c));
    if plain {
        return v.to_owned();
    }
    let mut out = String::from("\"");
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '$' => out.push_str("\\$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Items written in `format`. Items without a loaded value are written empty.
pub fn export(items: &[KvItem], format: KvFormat) -> String {
    match format {
        KvFormat::Json => {
            let map: Map<String, Json> = items
                .iter()
                .map(|i| (i.key.clone(), json!(i.value.clone().unwrap_or_default())))
                .collect();
            serde_json::to_string_pretty(&Json::Object(map)).unwrap_or_default() + "\n"
        }
        KvFormat::Env => items
            .iter()
            .map(|i| {
                format!(
                    "{}={}\n",
                    env_name(&i.key),
                    env_quote(i.value.as_deref().unwrap_or_default())
                )
            })
            .collect(),
        KvFormat::KvSet => {
            let list: Vec<Json> = items
                .iter()
                .map(|i| {
                    json!({
                        "key": i.key,
                        "label": i.label,
                        "value": i.value.clone().unwrap_or_default(),
                        "content_type": i.content_type.clone().unwrap_or_default(),
                        "tags": i.tags,
                    })
                })
                .collect();
            serde_json::to_string_pretty(&json!({ "items": list })).unwrap_or_default() + "\n"
        }
    }
}

fn flatten(prefix: &str, j: &Json, out: &mut BTreeMap<String, String>) {
    let join = |k: &str| {
        if prefix.is_empty() {
            k.to_owned()
        } else {
            format!("{prefix}:{k}")
        }
    };
    match j {
        Json::Object(o) if !o.is_empty() => {
            for (k, v) in o {
                flatten(&join(k), v, out);
            }
        }
        Json::Array(a) if !a.is_empty() => {
            for (i, v) in a.iter().enumerate() {
                flatten(&join(&i.to_string()), v, out);
            }
        }
        Json::String(s) => {
            out.insert(prefix.to_owned(), s.clone());
        }
        Json::Null => {
            out.insert(prefix.to_owned(), String::new());
        }
        other => {
            out.insert(prefix.to_owned(), other.to_string());
        }
    }
}

fn env_unquote(v: &str, line: usize) -> Result<String> {
    let v = v.trim();
    if let Some(inner) = v.strip_prefix('\'') {
        return inner
            .strip_suffix('\'')
            .map(str::to_owned)
            .ok_or_else(|| CloudError::Invalid(format!("line {line}: unclosed quote")));
    }
    let Some(inner) = v.strip_prefix('"') else {
        // A trailing ` # comment` is not part of an unquoted value.
        let v = match v.find(" #") {
            Some(ix) => v[..ix].trim_end(),
            None => v,
        };
        return Ok(v.to_owned());
    };
    let mut out = String::new();
    let mut chars = inner.chars();
    loop {
        match chars.next() {
            None => return Err(CloudError::Invalid(format!("line {line}: unclosed quote"))),
            Some('"') => return Ok(out),
            Some('\\') => match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some(c) => out.push(c),
                None => return Err(CloudError::Invalid(format!("line {line}: unclosed quote"))),
            },
            Some(c) => out.push(c),
        }
    }
}

/// Settings read from a file, each written with `label` unless the file names its own
/// (kvset). Writes overwrite existing settings.
pub fn import(text: &str, format: KvFormat, label: Option<&str>) -> Result<Vec<KvWrite>> {
    let label = label
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned);
    let plain = |key: String, value: String| KvWrite {
        key,
        label: label.clone(),
        value,
        ..KvWrite::default()
    };
    let writes: Vec<KvWrite> = match format {
        KvFormat::Json => {
            let j: Json = serde_json::from_str(text)
                .map_err(|e| CloudError::Invalid(format!("not JSON: {e}")))?;
            if !j.is_object() {
                return Err(CloudError::Invalid("expected a JSON object".into()));
            }
            let mut flat = BTreeMap::new();
            flatten("", &j, &mut flat);
            flat.into_iter()
                .filter(|(k, _)| !k.is_empty())
                .map(|(k, v)| plain(k, v))
                .collect()
        }
        KvFormat::Env => {
            let mut out = Vec::new();
            for (n, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let line = line.strip_prefix("export ").unwrap_or(line);
                let (k, v) = line.split_once('=').ok_or_else(|| {
                    CloudError::Invalid(format!("line {}: expected NAME=value", n + 1))
                })?;
                let k = k.trim();
                if k.is_empty() {
                    return Err(CloudError::Invalid(format!("line {}: no name", n + 1)));
                }
                out.push(plain(k.replace("__", ":"), env_unquote(v, n + 1)?));
            }
            out
        }
        KvFormat::KvSet => {
            let j: Json = serde_json::from_str(text)
                .map_err(|e| CloudError::Invalid(format!("not JSON: {e}")))?;
            let items = kvset_items(&j).ok_or_else(|| {
                CloudError::Invalid("expected {\"items\": [{\"key\": …}]}".into())
            })?;
            items
                .iter()
                .map(|i| {
                    let s = |k: &str| i.get(k).and_then(Json::as_str).map(str::to_owned);
                    let value = match i.get("value") {
                        Some(Json::String(s)) => s.clone(),
                        None | Some(Json::Null) => String::new(),
                        Some(other) => other.to_string(),
                    };
                    KvWrite {
                        key: s("key").unwrap_or_default(),
                        label: match i.get("label") {
                            Some(Json::String(l)) if !l.is_empty() => Some(l.clone()),
                            Some(_) => None,
                            None => label.clone(),
                        },
                        value,
                        content_type: s("content_type").filter(|c| !c.is_empty()),
                        tags: i
                            .get("tags")
                            .and_then(Json::as_object)
                            .map(|o| {
                                o.iter()
                                    .map(|(k, v)| {
                                        (k.clone(), v.as_str().unwrap_or_default().to_owned())
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        ..KvWrite::default()
                    }
                })
                .collect()
        }
    };
    if let Some(w) = writes.iter().find(|w| w.key.trim().is_empty()) {
        return Err(CloudError::Invalid(format!(
            "a setting has no key (value \"{}\")",
            w.value.chars().take(40).collect::<String>()
        )));
    }
    Ok(writes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, value: &str) -> KvItem {
        KvItem {
            key: key.into(),
            value: Some(value.into()),
            ..KvItem::default()
        }
    }

    #[test]
    fn json_round_trip_and_nesting() {
        let items = [item("app:db:host", "db.internal"), item("app:port", "5432")];
        let text = export(&items, KvFormat::Json);
        assert_eq!(KvFormat::detect("x.json", &text), KvFormat::Json);
        let back = import(&text, KvFormat::Json, Some("dev")).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].key, "app:db:host");
        assert_eq!(back[0].label.as_deref(), Some("dev"));
        let nested =
            r#"{"app": {"db": {"host": "h", "port": 5432}, "hosts": ["a", "b"], "on": true}}"#;
        let w = import(nested, KvFormat::Json, None).unwrap();
        let keys: Vec<_> = w
            .iter()
            .map(|w| (w.key.as_str(), w.value.as_str()))
            .collect();
        assert_eq!(
            keys,
            [
                ("app:db:host", "h"),
                ("app:db:port", "5432"),
                ("app:hosts:0", "a"),
                ("app:hosts:1", "b"),
                ("app:on", "true"),
            ]
        );
        assert!(import("[1]", KvFormat::Json, None).is_err());
    }

    #[test]
    fn env_round_trip() {
        let items = [
            item("App:Db:Host", "db.internal"),
            item("Greeting", "hello \"you\"\nbye $HOME"),
            item("Empty", ""),
        ];
        let text = export(&items, KvFormat::Env);
        assert!(text.starts_with("App__Db__Host=db.internal\n"), "{text}");
        assert_eq!(KvFormat::detect("settings.env", &text), KvFormat::Env);
        let back = import(&text, KvFormat::Env, None).unwrap();
        assert_eq!(back[0].key, "App:Db:Host");
        assert_eq!(back[1].value, "hello \"you\"\nbye $HOME");
        assert_eq!(back[2].value, "");
        let w = import(
            "# c\nexport A=1 # note\nB='x y'\n\nC = \"q\"",
            KvFormat::Env,
            None,
        )
        .unwrap();
        assert_eq!(
            w.iter().map(|w| w.value.as_str()).collect::<Vec<_>>(),
            ["1", "x y", "q"]
        );
        assert!(import("NOEQUALS", KvFormat::Env, None).is_err());
        assert!(import("A=\"open", KvFormat::Env, None).is_err());
    }

    #[test]
    fn kvset_keeps_labels_and_types() {
        let mut i = item("Conn", r#"{"uri":"https://v.vault.azure.net/secrets/c"}"#);
        i.label = Some("prod".into());
        i.content_type = Some("application/vnd.microsoft.appconfig.keyvaultref+json".into());
        i.tags.insert("team".into(), "web".into());
        let plain = item("Plain", "1");
        let text = export(&[i, plain], KvFormat::KvSet);
        assert_eq!(KvFormat::detect("a.json", &text), KvFormat::KvSet);
        let back = import(&text, KvFormat::KvSet, Some("dev")).unwrap();
        assert_eq!(back[0].label.as_deref(), Some("prod"));
        assert!(
            back[0]
                .content_type
                .as_deref()
                .unwrap()
                .contains("keyvaultref")
        );
        assert_eq!(back[0].tags["team"], "web");
        // An explicit null label stays the null label.
        assert_eq!(back[1].label, None);
        let bare = import(
            r#"[{"key": "k", "value": "v"}]"#,
            KvFormat::KvSet,
            Some("dev"),
        )
        .unwrap();
        assert_eq!(bare[0].label.as_deref(), Some("dev"));
    }
}
