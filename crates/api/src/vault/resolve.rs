//! Resolve references at credential consumption boundaries, never when saving settings.

use super::vault_secret_reference;

pub fn contains_reference(value: &str) -> bool {
    value.contains("{{vault.")
}

/// Expands only the original template. Values containing reference-like text are
/// literal secrets and are never recursively interpreted. Resolver errors are
/// deliberately replaced so a backend cannot expose credential material.
pub fn resolve_template_with(
    value: &str,
    mut resolver: impl FnMut(&str) -> Result<String, String>,
) -> Result<String, String> {
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some(start) = remaining.find("{{vault.") {
        output.push_str(&remaining[..start]);
        let reference = &remaining[start + "{{vault.".len()..];
        let end = reference
            .find("}}")
            .ok_or("Vault references use {{vault.name}}")?;
        let name = reference[..end].trim();
        vault_secret_reference(name)?;
        let secret = resolver(name)
            .map_err(|_| format!("Vault secret '{name}' is missing or unavailable"))?;
        output.push_str(&secret);
        remaining = &reference[end + 2..];
    }
    output.push_str(remaining);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_do_not_access_the_vault() {
        let value = "plain {{environment.token}} password";
        assert!(!contains_reference(value));
        assert_eq!(
            resolve_template_with(value, |_| panic!("unexpected vault read")).unwrap(),
            value
        );
    }

    #[test]
    fn expands_embedded_and_multiple_credentials_without_recursion() {
        let template = "Bearer {{vault.api-key}}; login={{vault.user}}";
        let resolved = resolve_template_with(template, |name| match name {
            "api-key" => Ok("{{vault.literal-secret}}".into()),
            "user" => Ok("alice".into()),
            _ => panic!("unexpected reference"),
        })
        .unwrap();
        assert_eq!(resolved, "Bearer {{vault.literal-secret}}; login=alice");
        assert_eq!(template, "Bearer {{vault.api-key}}; login={{vault.user}}");
    }

    #[test]
    fn malformed_and_unavailable_references_fail_without_secret_disclosure() {
        for template in [
            "{{vault.}}",
            "{{vault.missing",
            "{{vault.invalid name}}",
            "{{vault.a{{vault.b}}",
        ] {
            assert!(
                resolve_template_with(template, |_| panic!("invalid name must not be read"))
                    .is_err()
            );
        }
        let error = resolve_template_with("{{vault.missing}}", |_| {
            Err("backend includes sensitive value".into())
        })
        .unwrap_err();
        assert_eq!(error, "Vault secret 'missing' is missing or unavailable");
    }
}
