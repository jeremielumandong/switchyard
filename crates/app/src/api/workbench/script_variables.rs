use std::collections::{BTreeMap, BTreeSet};

use switchyard_api::{Variable, VariableValue};

use super::{entries::KvGrid, format_variables};

fn editor_key(line: &str) -> Option<String> {
    KvGrid::parse(line, '=')
        .into_iter()
        .next()
        .map(|(_, key, _)| key.strip_prefix("secret:").unwrap_or(&key).to_string())
}

fn editor_rows(source: &str) -> BTreeMap<String, Vec<&str>> {
    let mut rows = BTreeMap::new();
    for line in source.lines() {
        if let Some(key) = editor_key(line) {
            rows.entry(key).or_insert_with(Vec::new).push(line);
        }
    }
    rows
}

fn saved_rows(variables: &[Variable]) -> BTreeMap<&str, Vec<(&VariableValue, bool)>> {
    let mut rows = BTreeMap::new();
    for variable in variables {
        rows.entry(variable.key.as_str())
            .or_insert_with(Vec::new)
            .push((&variable.value, variable.enabled));
    }
    rows
}

/// Apply script changes without overwriting rows edited while the request ran.
/// Unchanged rows retain their original text, including whitespace and blank lines.
pub(super) fn merge_editor(
    initial_source: &str,
    current_source: &str,
    saved_before: &[Variable],
    saved_after: &[Variable],
) -> String {
    let initial = editor_rows(initial_source);
    let current = editor_rows(current_source);
    let before = saved_rows(saved_before);
    let after = saved_rows(saved_after);
    let changed: BTreeSet<&str> = before
        .keys()
        .chain(after.keys())
        .copied()
        .filter(|key| before.get(key) != after.get(key))
        .filter(|key| initial.get(*key) == current.get(*key))
        .collect();
    if changed.is_empty() {
        return current_source.to_string();
    }

    let mut replacements = BTreeMap::<&str, Vec<Variable>>::new();
    for variable in saved_after {
        if changed.contains(variable.key.as_str()) {
            replacements
                .entry(variable.key.as_str())
                .or_default()
                .push(variable.clone());
        }
    }
    let mut output = String::new();
    let mut applied = BTreeSet::new();
    for line in current_source.split_inclusive('\n') {
        let key = editor_key(line);
        if let Some(key) = key.as_deref().filter(|key| changed.contains(*key)) {
            if applied.insert(key.to_string())
                && let Some(variables) = replacements.get(key)
            {
                output.push_str(&format_variables(variables));
                if line.ends_with("\r\n") {
                    output.push_str("\r\n");
                } else if line.ends_with('\n') {
                    output.push('\n');
                }
            }
        } else {
            output.push_str(line);
        }
    }
    // Preserve the persisted order for newly added keys.
    for variable in saved_after {
        let key = variable.key.as_str();
        if changed.contains(key) && applied.insert(key.to_string()) {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&format_variables(&replacements[key]));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_api::{RowId, SecretRef};

    fn plain(key: &str, value: &str) -> Variable {
        Variable {
            id: RowId::new(),
            key: key.into(),
            value: VariableValue::Plain(value.into()),
            enabled: true,
            description: String::new(),
        }
    }

    #[test]
    fn updates_token_and_preserves_unrelated_text() {
        let initial = "  token = old  \n\n#  other = unchanged \n";
        let current = "  token = old  \n\n#  other = edited \nnew=user";
        assert_eq!(
            merge_editor(
                initial,
                current,
                &[plain("token", "old")],
                &[plain("token", "new")]
            ),
            "token=new\n\n#  other = edited \nnew=user"
        );
    }

    #[test]
    fn preserves_same_key_edits_additions_and_deletions() {
        for current in ["token=typed", "", "token=old\ntoken=typed", " token=old"] {
            assert_eq!(
                merge_editor(
                    "token=old",
                    current,
                    &[plain("token", "old")],
                    &[plain("token", "new")]
                ),
                current
            );
        }
        assert_eq!(
            merge_editor("", "token=typed", &[], &[plain("token", "new")]),
            "token=typed"
        );
    }

    #[test]
    fn adds_in_saved_order_and_deletes_changed_keys() {
        assert_eq!(
            merge_editor(
                "gone=old\nkeep=yes",
                "gone=old\nkeep=yes",
                &[plain("gone", "old")],
                &[plain("z", "1"), plain("a", "2")]
            ),
            "keep=yes\nz=1\na=2"
        );
    }

    #[test]
    fn ignores_ids_and_descriptions() {
        let mut after = plain("token", "old");
        after.description = "new description".into();
        assert_eq!(
            merge_editor(
                " token = old \n",
                " token = old \n",
                &[plain("token", "old")],
                &[after]
            ),
            " token = old \n"
        );
    }

    #[test]
    fn updates_enabled_state_and_duplicate_rows() {
        let mut after = plain("token", "new");
        after.enabled = false;
        assert_eq!(
            merge_editor(
                "token=old\ntoken=duplicate\nkeep=yes",
                "token=old\ntoken=duplicate\nkeep=yes",
                &[plain("token", "old"), plain("token", "duplicate")],
                &[after]
            ),
            "# token=new\nkeep=yes"
        );
    }

    #[test]
    fn matches_secret_keys_and_never_exposes_secret_references() {
        let mut secret = plain("token", "");
        secret.value = VariableValue::Secret(SecretRef::new("vault-token").unwrap());
        assert_eq!(
            merge_editor(
                "token=old",
                "token=old",
                &[plain("token", "old")],
                &[secret.clone()]
            ),
            "secret:token="
        );
        assert_eq!(
            merge_editor(
                "secret:token=",
                "secret:token=",
                &[secret.clone()],
                &[plain("token", "plain")]
            ),
            "token=plain"
        );
        assert_eq!(
            merge_editor(
                " secret:token= \n",
                " secret:token= \n",
                &[secret.clone()],
                &[secret]
            ),
            " secret:token= \n"
        );
    }

    #[test]
    fn preserves_edited_secret_and_line_endings() {
        let mut secret = plain("token", "");
        secret.value = VariableValue::Secret(SecretRef::new("vault-token").unwrap());
        assert_eq!(
            merge_editor(
                "secret:token=",
                "secret:token=user-secret",
                &[secret],
                &[plain("token", "new")]
            ),
            "secret:token=user-secret"
        );
        assert_eq!(
            merge_editor(
                "token=old\r\nkeep=yes\r\n",
                "token=old\r\nkeep=yes\r\n",
                &[plain("token", "old")],
                &[plain("token", "new")]
            ),
            "token=new\r\nkeep=yes\r\n"
        );
    }
}
