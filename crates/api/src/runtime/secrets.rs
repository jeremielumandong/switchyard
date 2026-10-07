//! Session-scoped secret values and the text parsers shared by the desktop
//! editors and the headless runner.
//!
//! Secret values entered in a session live only here (or in the vault behind
//! the fallback store); they are never serialized, logged or displayed.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::{
    CompileContext, KeyValueRow, RowId, SecretRef, SecretResolver, SecretStore, SecretValue,
    Variable, VariableValue, WorkspaceId,
};

#[derive(Clone, Default)]
pub struct DraftSecrets {
    values: BTreeMap<String, String>,
    fallback: Option<(Arc<dyn SecretStore>, WorkspaceId)>,
}

impl DraftSecrets {
    pub fn with_store(store: Arc<dyn SecretStore>, workspace: WorkspaceId) -> Self {
        Self {
            values: BTreeMap::new(),
            fallback: Some((store, workspace)),
        }
    }

    pub fn merge(&mut self, other: Self) {
        self.values.extend(other.values);
    }

    /// Remember a value entered in this session for `reference`. Headless
    /// callers use this where the desktop editors fill values from text.
    pub fn insert(&mut self, reference: &SecretRef, value: impl Into<String>) {
        self.values
            .insert(reference.as_str().to_string(), value.into());
    }

    pub fn forget(&mut self, reference: &SecretRef) {
        self.values.remove(reference.as_str());
    }

    /// Whether `reference` was entered in this session, as opposed to
    /// living only in the vault.
    pub fn has_value(&self, reference: &SecretRef) -> bool {
        self.values.contains_key(reference.as_str())
    }

    pub fn persist(&self, store: &dyn SecretStore, workspace: &WorkspaceId) -> Result<(), String> {
        for (reference, value) in &self.values {
            let reference = SecretRef::new(reference.clone())?;
            store
                .set_secret(workspace, &reference, SecretValue::new(value))
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

impl SecretResolver for DraftSecrets {
    fn resolve(&self, reference: &SecretRef) -> Result<String, String> {
        if let Some(value) = self.values.get(reference.as_str()) {
            return Ok(value.clone());
        }
        if let Some((store, workspace)) = &self.fallback {
            return store
                .get_secret(workspace, reference)
                .map(|value| value.expose_secret().to_string())
                .map_err(|error| error.to_string());
        }
        Err(format!(
            "secret {} is not available in this session",
            reference.as_str()
        ))
    }
}

/// Compile with only an environment and a data row, no collection scopes.
pub fn compile_context<'a>(
    environment: &'a [Variable],
    data: &'a [Variable],
    secrets: &'a DraftSecrets,
) -> CompileContext<'a> {
    CompileContext {
        global: &[],
        environment,
        data,
        local: &[],
        secrets,
        environment_base_url: None,
        environment_auth: None,
    }
}

pub fn parse_session_variables(
    input: &str,
    namespace: &str,
) -> Result<(Vec<Variable>, DraftSecrets), String> {
    let mut secrets = DraftSecrets::default();
    let mut variables = Vec::new();
    for row in parse_key_value_rows(input, '=')? {
        let (key, value) = if let Some(key) = row.key.strip_prefix("secret:") {
            let named = crate::vault::parse_vault_expression(&row.value)?;
            let reference = named.clone().map_or_else(
                || SecretRef::new(format!("workbench.{namespace}.variable.{key}")),
                Ok,
            )?;
            // A blank secret row is the masked representation of a restored
            // vault reference. Keep the deterministic reference and let the
            // resolver fail closed if this is actually a new/missing secret.
            if named.is_none() && !row.value.trim().is_empty() {
                secrets.values.insert(reference.as_str().into(), row.value);
            }
            (key.to_string(), VariableValue::Secret(reference))
        } else {
            (row.key, VariableValue::Plain(row.value))
        };
        variables.push(Variable {
            id: row.id,
            key,
            value,
            enabled: row.enabled,
            description: row.description,
        });
    }
    Ok((variables, secrets))
}

pub fn parse_data_rows(input: &str) -> Result<Vec<Vec<Variable>>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(vec![Vec::new()]);
    }
    if input.starts_with('[') {
        let rows: Vec<serde_json::Map<String, serde_json::Value>> =
            serde_json::from_str(input).map_err(|error| format!("invalid JSON data: {error}"))?;
        return Ok(rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|(key, value)| Variable {
                        id: RowId::new(),
                        key,
                        value: VariableValue::Plain(match value {
                            serde_json::Value::String(value) => value,
                            value => value.to_string(),
                        }),
                        enabled: true,
                        description: String::new(),
                    })
                    .collect()
            })
            .collect());
    }
    let mut lines = input.lines().filter(|line| !line.trim().is_empty());
    let headers = parse_csv_line(lines.next().ok_or("CSV needs a header row")?)?;
    if headers.iter().any(|header| header.trim().is_empty()) {
        return Err("CSV headers cannot be empty".into());
    }
    lines
        .enumerate()
        .map(|(index, line)| {
            let values = parse_csv_line(line)?;
            if values.len() != headers.len() {
                return Err(format!(
                    "CSV row {} has {} fields; expected {}",
                    index + 2,
                    values.len(),
                    headers.len()
                ));
            }
            Ok(headers
                .iter()
                .cloned()
                .zip(values)
                .map(|(key, value)| Variable {
                    id: RowId::new(),
                    key,
                    value: VariableValue::Plain(value),
                    enabled: true,
                    description: String::new(),
                })
                .collect())
        })
        .collect()
}

/// The vault reference a masked auth field of `scope` is stored under —
/// the same one `parse_auth` assigns, so callers can name it in messages.
pub fn auth_secret_reference(scope: &str, name: &str) -> String {
    format!("workbench.{scope}.auth.{name}")
}

/// One `key<separator>value` row per line; blank lines are skipped.
pub fn parse_key_value_rows(input: &str, separator: char) -> Result<Vec<KeyValueRow>, String> {
    input
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let (enabled, trimmed) = trimmed
                .strip_prefix('#')
                .map_or((true, trimmed), |line| (false, line.trim()));
            Some(
                trimmed
                    .split_once(separator)
                    .ok_or_else(|| format!("line {} needs name{separator}value", index + 1))
                    .and_then(|(key, value)| {
                        if key.trim().is_empty() {
                            Err(format!("line {} needs a name", index + 1))
                        } else {
                            let mut row = KeyValueRow::enabled(key.trim(), value.trim());
                            row.enabled = enabled;
                            Ok(row)
                        }
                    }),
            )
        })
        .collect()
}

pub fn parse_cookie_pairs(input: &str) -> Result<Vec<(String, String)>, String> {
    let normalized = input.replace(';', "\n");
    Ok(parse_key_value_rows(&normalized, '=')?
        .into_iter()
        .filter(|row| row.enabled)
        .map(|row| (row.key, row.value))
        .collect())
}

fn parse_csv_line(line: &str) -> Result<Vec<String>, String> {
    let mut values = Vec::new();
    let mut value = String::new();
    let mut chars = line.chars().peekable();
    let mut quoted = false;
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => {
                value.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => values.push(std::mem::take(&mut value)),
            _ => value.push(ch),
        }
    }
    if quoted {
        return Err("CSV row has an unterminated quoted field".into());
    }
    values.push(value);
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_rows_parse_json_and_quoted_csv() {
        let json = parse_data_rows(r#"[{"id":1},{"id":2}]"#).unwrap();
        assert_eq!(json.len(), 2);
        let csv = parse_data_rows("name,note\none,\"hello, world\"").unwrap();
        assert!(matches!(&csv[0][1].value, VariableValue::Plain(value) if value == "hello, world"));
    }
}
