//! Contract drift: learn the shape of a request's JSON responses from the
//! ones already seen (saved examples and History) and report how a new
//! response differs: fields that disappeared, fields that changed type, and
//! fields that are new. No schema has to be written first; the contract is
//! whatever the API has actually returned, and it can be exported as JSON
//! Schema.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

/// Array items checked per array (the rest are assumed to look alike).
const MAX_ITEMS_CHECKED: usize = 50;
/// Drifts reported per response.
const MAX_DRIFTS: usize = 200;
/// Nesting followed before a value is treated as opaque.
const MAX_DEPTH: usize = 32;

/// Every JSON type a position held across the samples.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shape {
    pub null: bool,
    pub boolean: bool,
    pub integer: bool,
    /// A number with a fractional part or exponent.
    pub number: bool,
    pub string: bool,
    /// The merged shape of every array item; an empty `Shape` when only empty
    /// arrays were seen.
    pub array: Option<Box<Shape>>,
    pub object: Option<ObjectShape>,
}

/// The fields objects at one position had.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ObjectShape {
    /// Objects seen at this position.
    pub samples: u32,
    /// Each field's shape and how many of those objects had it.
    pub fields: BTreeMap<String, (Shape, u32)>,
}

impl Shape {
    /// The shape of one value.
    pub fn of(value: &Value) -> Self {
        let mut shape = Self::default();
        shape.add(value, 0);
        shape
    }

    /// The merged shape of several values.
    pub fn learn<'a>(values: impl IntoIterator<Item = &'a Value>) -> Self {
        let mut shape = Self::default();
        for value in values {
            shape.add(value, 0);
        }
        shape
    }

    fn add(&mut self, value: &Value, depth: usize) {
        match value {
            Value::Null => self.null = true,
            Value::Bool(_) => self.boolean = true,
            Value::Number(number) if number.is_i64() || number.is_u64() => self.integer = true,
            Value::Number(_) => self.number = true,
            Value::String(_) => self.string = true,
            Value::Array(items) => {
                let item = self.array.get_or_insert_with(Box::default);
                if depth < MAX_DEPTH {
                    for value in items {
                        item.add(value, depth + 1);
                    }
                }
            }
            Value::Object(fields) => {
                let object = self.object.get_or_insert_with(ObjectShape::default);
                object.samples += 1;
                if depth < MAX_DEPTH {
                    for (name, value) in fields {
                        let entry = object.fields.entry(name.clone()).or_default();
                        entry.0.add(value, depth + 1);
                        entry.1 += 1;
                    }
                }
            }
        }
    }

    /// No value was ever seen here.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `string`, `integer | null`, `object`, … for people.
    pub fn describe(&self) -> String {
        let mut kinds = Vec::new();
        if self.object.is_some() {
            kinds.push("object");
        }
        if self.array.is_some() {
            kinds.push("array");
        }
        if self.string {
            kinds.push("string");
        }
        if self.number {
            kinds.push("number");
        } else if self.integer {
            kinds.push("integer");
        }
        if self.boolean {
            kinds.push("boolean");
        }
        if self.null {
            kinds.push("null");
        }
        if kinds.is_empty() {
            "nothing".into()
        } else {
            kinds.join(" | ")
        }
    }

    /// Whether `value`'s own type was seen here (children are checked separately).
    fn admits(&self, value: &Value) -> bool {
        match value {
            Value::Null => self.null,
            Value::Bool(_) => self.boolean,
            Value::Number(number) if number.is_i64() || number.is_u64() => {
                self.integer || self.number
            }
            Value::Number(_) => self.number,
            Value::String(_) => self.string,
            Value::Array(_) => self.array.is_some(),
            Value::Object(_) => self.object.is_some(),
        }
    }

    /// This shape as a JSON Schema (draft 2020-12). A field every sample had
    /// is required.
    pub fn json_schema(&self) -> Value {
        let mut schema = self.schema_body();
        if let Value::Object(map) = &mut schema {
            let mut root = Map::new();
            root.insert(
                "$schema".into(),
                json!("https://json-schema.org/draft/2020-12/schema"),
            );
            root.extend(std::mem::take(map));
            return Value::Object(root);
        }
        schema
    }

    fn schema_body(&self) -> Value {
        let mut types = Vec::new();
        let mut schema = Map::new();
        if let Some(object) = &self.object {
            types.push("object");
            let properties: Map<String, Value> = object
                .fields
                .iter()
                .map(|(name, (shape, _))| (name.clone(), shape.schema_body()))
                .collect();
            let required: Vec<&String> = object
                .fields
                .iter()
                .filter(|(_, (_, present))| *present == object.samples)
                .map(|(name, _)| name)
                .collect();
            schema.insert("properties".into(), Value::Object(properties));
            if !required.is_empty() {
                schema.insert("required".into(), json!(required));
            }
        }
        if let Some(item) = &self.array {
            types.push("array");
            if !item.is_empty() {
                schema.insert("items".into(), item.schema_body());
            }
        }
        if self.string {
            types.push("string");
        }
        if self.number {
            types.push("number");
        } else if self.integer {
            types.push("integer");
        }
        if self.boolean {
            types.push("boolean");
        }
        if self.null {
            types.push("null");
        }
        match types.as_slice() {
            [] => {}
            [one] => {
                schema.insert("type".into(), json!(one));
            }
            many => {
                schema.insert("type".into(), json!(many));
            }
        }
        Value::Object(schema)
    }

    /// Every position below this one, `$.items[].price`, with its types and
    /// whether every sample had it.
    pub fn fields(&self) -> Vec<FieldSummary> {
        let mut out = Vec::new();
        self.collect_fields("$", true, &mut out);
        out
    }

    fn collect_fields(&self, path: &str, required: bool, out: &mut Vec<FieldSummary>) {
        out.push(FieldSummary {
            path: path.to_string(),
            types: self.describe(),
            required,
        });
        if let Some(object) = &self.object {
            for (name, (shape, present)) in &object.fields {
                shape.collect_fields(&child_path(path, name), *present == object.samples, out);
            }
        }
        if let Some(item) = &self.array
            && !item.is_empty()
        {
            item.collect_fields(&format!("{path}[]"), true, out);
        }
    }

    /// How `value` departs from this shape.
    pub fn check(&self, value: &Value) -> Vec<Drift> {
        let mut drifts = Vec::new();
        self.check_at("$", value, 0, &mut drifts);
        drifts.sort_by(|a, b| {
            b.kind
                .is_breaking()
                .cmp(&a.kind.is_breaking())
                .then_with(|| a.path.cmp(&b.path))
        });
        drifts.dedup();
        drifts.truncate(MAX_DRIFTS);
        drifts
    }

    fn check_at(&self, path: &str, value: &Value, depth: usize, out: &mut Vec<Drift>) {
        if out.len() >= MAX_DRIFTS * 4 || depth > MAX_DEPTH {
            return;
        }
        if !self.admits(value) {
            out.push(Drift {
                path: path.to_string(),
                kind: DriftKind::TypeChanged {
                    was: self.describe(),
                    now: Shape::of(value).describe(),
                },
            });
            return;
        }
        match value {
            Value::Object(fields) => {
                let Some(object) = &self.object else { return };
                for (name, (shape, present)) in &object.fields {
                    let child = child_path(path, name);
                    match fields.get(name) {
                        Some(value) => shape.check_at(&child, value, depth + 1, out),
                        None if *present == object.samples => out.push(Drift {
                            path: child,
                            kind: DriftKind::Removed {
                                was: shape.describe(),
                            },
                        }),
                        None => {}
                    }
                }
                for (name, value) in fields {
                    if !object.fields.contains_key(name) {
                        out.push(Drift {
                            path: child_path(path, name),
                            kind: DriftKind::Added {
                                now: Shape::of(value).describe(),
                            },
                        });
                    }
                }
            }
            Value::Array(items) => {
                let Some(item) = self.array.as_deref().filter(|item| !item.is_empty()) else {
                    return;
                };
                let path = format!("{path}[]");
                for value in items.iter().take(MAX_ITEMS_CHECKED) {
                    item.check_at(&path, value, depth + 1, out);
                }
            }
            _ => {}
        }
    }
}

fn child_path(parent: &str, name: &str) -> String {
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if plain {
        format!("{parent}.{name}")
    } else {
        format!("{parent}[{}]", Value::String(name.to_string()))
    }
}

/// One position of a learned contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldSummary {
    pub path: String,
    pub types: String,
    /// Every sample that reached this position had it.
    pub required: bool,
}

/// How one position changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriftKind {
    /// A field every earlier response had is missing.
    Removed { was: String },
    /// The value has a type no earlier response had here.
    TypeChanged { was: String, now: String },
    /// A field no earlier response had.
    Added { now: String },
}

impl DriftKind {
    /// Removed fields and type changes break clients; additions usually do not.
    pub fn is_breaking(&self) -> bool {
        !matches!(self, Self::Added { .. })
    }
}

/// A difference between a response and the learned contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drift {
    pub path: String,
    pub kind: DriftKind,
}

impl Drift {
    /// `$.user.email: removed (was string)`.
    pub fn describe(&self) -> String {
        match &self.kind {
            DriftKind::Removed { was } => format!("{}: removed (was {was})", self.path),
            DriftKind::TypeChanged { was, now } => {
                format!("{}: type changed from {was} to {now}", self.path)
            }
            DriftKind::Added { now } => format!("{}: new field ({now})", self.path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn learned() -> Shape {
        let samples = [
            json!({"id": 1, "name": "Lamp", "price": 12.5, "tags": ["a"], "owner": {"id": 7}}),
            json!({"id": 2, "name": "Desk", "price": 99, "tags": [], "owner": null, "note": "x"}),
        ];
        Shape::learn(samples.iter())
    }

    #[test]
    fn learning_merges_types_and_tracks_optional_fields() {
        let shape = learned();
        let fields = shape.fields();
        let field = |path: &str| fields.iter().find(|f| f.path == path).cloned().unwrap();
        assert_eq!(field("$.id").types, "integer");
        assert_eq!(field("$.price").types, "number", "integer and float merge");
        assert_eq!(field("$.owner").types, "object | null");
        assert!(field("$.name").required);
        assert!(!field("$.note").required, "only one sample had it");
        assert_eq!(field("$.tags[]").types, "string");
        assert_eq!(field("$.owner.id").types, "integer");
    }

    #[test]
    fn a_matching_response_has_no_drift() {
        let value = json!({"id": 3, "name": "Chair", "price": 5, "tags": ["b"], "owner": null});
        assert_eq!(learned().check(&value), Vec::new());
    }

    #[test]
    fn removals_and_type_changes_are_breaking_additions_are_not() {
        let value = json!({
            "id": "3",
            "price": 5,
            "tags": [1],
            "owner": {"id": 7, "email": "a@b.c"},
            "weird key": true
        });
        let drifts = learned().check(&value);
        let described: Vec<String> = drifts.iter().map(Drift::describe).collect();
        assert_eq!(
            described,
            vec![
                "$.id: type changed from integer to string",
                "$.name: removed (was string)",
                "$.tags[]: type changed from string to integer",
                "$.owner.email: new field (string)",
                "$[\"weird key\"]: new field (boolean)",
            ]
        );
        assert!(drifts[0].kind.is_breaking());
        assert!(!drifts[4].kind.is_breaking());
    }

    #[test]
    fn json_schema_marks_required_fields_and_unions() {
        let schema = learned().json_schema();
        assert_eq!(
            schema["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        assert_eq!(schema["type"], "object");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, ["id", "name", "owner", "price", "tags"]);
        assert_eq!(
            schema["properties"]["owner"]["type"],
            json!(["object", "null"])
        );
        assert_eq!(schema["properties"]["tags"]["items"]["type"], "string");
    }

    #[test]
    fn top_level_arrays_check_their_items() {
        let shape = Shape::learn([json!([{"id": 1}, {"id": 2}])].iter());
        let drifts = shape.check(&json!([{"id": 1}, {}]));
        assert_eq!(drifts.len(), 1);
        assert_eq!(drifts[0].describe(), "$[].id: removed (was integer)");
    }
}
