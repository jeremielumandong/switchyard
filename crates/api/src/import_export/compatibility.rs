//! Advisory source checks. This never executes imported scripts or claims parity.
use super::*;

pub(super) fn review(imported: &mut ImportResult) {
    let mut findings = Vec::new();
    review_scripts(
        "Collection",
        &imported.collection.name,
        &imported.collection.scripts,
        &mut findings,
    );
    for folder in &imported.folders {
        review_scripts("Folder", &folder.name, &folder.scripts, &mut findings);
    }
    for request in &imported.requests {
        review_scripts("Request", &request.name, &request.scripts, &mut findings);
    }
    imported.warnings.extend(findings);
}

fn review_scripts(kind: &str, name: &str, scripts: &Scripts, findings: &mut Vec<String>) {
    const UNSUPPORTED: &[(&str, &str)] = &[
        (
            "pm.execution.skipRequest",
            "Skipping the current request is unavailable; setNextRequest supports next-request and stop commands.",
        ),
        (
            "setInterval",
            "Repeating script timers are unavailable; use bounded collection branching.",
        ),
        (
            "require('fs')",
            "Filesystem modules are unavailable in the sandbox.",
        ),
        (
            "require(\"fs\")",
            "Filesystem modules are unavailable in the sandbox.",
        ),
    ];
    for (phase, source) in [
        ("Pre-request", &scripts.pre_request),
        ("Post-response", &scripts.tests),
    ] {
        if source.trim().is_empty() {
            continue;
        }
        findings.push(format!("{kind} {name:?} · {phase}: imported script requires review. Workbench implements a bounded Postman API subset; static checks cannot verify full compatibility."));
        for (api, help) in UNSUPPORTED {
            if let Some((line, _)) = source
                .lines()
                .enumerate()
                .find(|(_, line)| line.contains(api))
            {
                findings.push(format!(
                    "{kind} {name:?} · {phase}, line {}: {api} may require adaptation. {help}",
                    line + 1
                ));
            }
        }
        if source.contains("jsonSchema") {
            findings.push(format!("{kind} {name:?} · {phase}: JSON Schema validation runs in the sandbox; remote schema loading is unavailable."));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn import_warns_with_owner_phase_and_line_without_copying_script_values() {
        let result = import(&WorkspaceId::new("compatibility-test").unwrap(), br#"{
          "info":{"name":"Migration","schema":"postman"},
          "item":[{"name":"Login","request":{"method":"POST","url":"https://example.test"},
          "event":[{"listen":"prerequest","script":{"exec":["const secret = 'private-value';","require('fs');"]}}]}]
        }"#).unwrap();
        let warnings = result.warnings.join("\n");
        assert!(warnings.contains("Login"));
        assert!(warnings.contains("Pre-request, line 2"));
        assert!(warnings.contains("require(\'fs\')"));
        assert!(!warnings.contains("private-value"));
    }
}
