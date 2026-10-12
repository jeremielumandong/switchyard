//! Runbooks: saved, parameterized, step-by-step procedures for on-call engineers.
//!
//! A runbook is stored as one JSON document in the `runbooks` table. Secret parameter
//! values never appear in it: they live in the keychain or vault under
//! [`Runbook::secret_ref`], and [`Runbook::sanitized`] clears any value a caller left in
//! a secret parameter's `default` before the runbook is stored or exported.

use serde::{Deserialize, Serialize};

use crate::model::{ProfileId, SecretRef, ValidationError};

/// Version of the JSON written by [`RunbookExport`].
pub const EXPORT_VERSION: u32 = 1;

/// Default time limit of a shell step, in seconds.
pub const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 300;

/// Longest shell step time limit, in seconds.
pub const MAX_SHELL_TIMEOUT_SECS: u64 = 3600;

/// A saved runbook.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Runbook {
    /// Id; empty before it is stored.
    #[serde(default)]
    pub id: String,
    /// Title.
    pub name: String,
    /// What it is for and when to use it (Markdown).
    #[serde(default)]
    pub description: String,
    /// Parameters, substituted as `{{name}}` in step text.
    #[serde(default)]
    pub params: Vec<RunbookParam>,
    /// Steps, in order.
    #[serde(default)]
    pub steps: Vec<RunbookStep>,
    /// Creation time, ms since epoch.
    #[serde(default)]
    pub created_at: i64,
    /// Last change, ms since epoch.
    #[serde(default)]
    pub updated_at: i64,
}

/// A runbook parameter.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunbookParam {
    /// Name used in `{{name}}` (letters, digits, `_`).
    pub name: String,
    /// Label shown in the run form; the name when empty.
    #[serde(default)]
    pub label: String,
    /// Default value. Always empty for a secret parameter: its default lives in the
    /// keychain or vault ([`Runbook::secret_ref`]).
    #[serde(default)]
    pub default: String,
    /// The value is a secret: masked in the UI, never stored in plain text, never in
    /// exports or history.
    #[serde(default)]
    pub secret: bool,
}

impl RunbookParam {
    /// A plain parameter.
    pub fn new(name: &str, default: &str) -> Self {
        Self {
            name: name.to_owned(),
            label: String::new(),
            default: default.to_owned(),
            secret: false,
        }
    }

    /// A secret parameter (its value is set separately).
    pub fn secret(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            label: String::new(),
            default: String::new(),
            secret: true,
        }
    }

    /// The label, or the name when no label is set.
    pub fn display_label(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.name
        } else {
            &self.label
        }
    }
}

/// One step of a runbook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunbookStep {
    /// Stable id within the runbook (UI identity across reorders).
    #[serde(default)]
    pub id: String,
    /// Short title; the kind's name when empty.
    #[serde(default)]
    pub title: String,
    /// What the step does.
    #[serde(flatten)]
    pub kind: StepKind,
}

impl RunbookStep {
    /// A new step with a fresh id.
    pub fn new(title: &str, kind: StepKind) -> Self {
        Self {
            id: crate::random::random_hex(6),
            title: title.to_owned(),
            kind,
        }
    }

    /// The title, or the kind's name when no title is set.
    pub fn display_title(&self) -> &str {
        if self.title.trim().is_empty() {
            self.kind.label()
        } else {
            &self.title
        }
    }
}

/// What a step does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepKind {
    /// Markdown text for the reader; nothing runs.
    Note {
        /// Markdown.
        text: String,
    },
    /// SQL against a saved database connection.
    Sql {
        /// Connection; `None` until chosen.
        connection: Option<ProfileId>,
        /// Statements (split with the connection's dialect).
        sql: String,
    },
    /// An HTTP request: inline, or a saved API workspace request.
    Http(HttpStep),
    /// A shell command, local or on a Host. Runs only after the user clicks Run on it.
    Shell {
        /// Where it runs.
        target: ShellTarget,
        /// Command line (`sh -c` locally on Unix, `cmd /C` on Windows; the login shell
        /// on a Host).
        command: String,
        /// Time limit in seconds.
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },
    /// A manual checkpoint: the run waits until the user continues.
    Confirm {
        /// What to check before continuing.
        text: String,
    },
}

fn default_timeout() -> u64 {
    DEFAULT_SHELL_TIMEOUT_SECS
}

impl StepKind {
    /// The kind's display name.
    pub fn label(&self) -> &'static str {
        match self {
            StepKind::Note { .. } => "Note",
            StepKind::Sql { .. } => "SQL",
            StepKind::Http(_) => "HTTP",
            StepKind::Shell { .. } => "Shell",
            StepKind::Confirm { .. } => "Confirm",
        }
    }

    /// Short badge (`SQL`, `SH`).
    pub fn badge(&self) -> &'static str {
        match self {
            StepKind::Note { .. } => "MD",
            StepKind::Sql { .. } => "SQL",
            StepKind::Http(_) => "API",
            StepKind::Shell { .. } => "SH",
            StepKind::Confirm { .. } => "OK?",
        }
    }

    /// An empty step of the same kind as `badge` (`MD`, `SQL`, `API`, `SH`, `OK?`).
    pub fn empty(badge: &str) -> Option<Self> {
        Some(match badge {
            "MD" => StepKind::Note {
                text: String::new(),
            },
            "SQL" => StepKind::Sql {
                connection: None,
                sql: String::new(),
            },
            "API" => StepKind::Http(HttpStep::default()),
            "SH" => StepKind::Shell {
                target: ShellTarget::Local,
                command: String::new(),
                timeout_secs: DEFAULT_SHELL_TIMEOUT_SECS,
            },
            "OK?" => StepKind::Confirm {
                text: String::new(),
            },
            _ => return None,
        })
    }

    /// Profiles the step uses (to check they still exist).
    pub fn profile(&self) -> Option<&ProfileId> {
        match self {
            StepKind::Sql { connection, .. } => connection.as_ref(),
            StepKind::Shell {
                target: ShellTarget::Host(h),
                ..
            } => Some(h),
            _ => None,
        }
    }
}

/// Where a shell step runs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellTarget {
    /// This machine.
    #[default]
    Local,
    /// A saved Host, through its shared SSH session.
    Host(ProfileId),
}

/// An HTTP step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpStep {
    /// A saved API workspace request to send instead of the inline fields. Runbook
    /// parameters reach it as `{{name}}` variables of the request's local scope.
    #[serde(default)]
    pub request: Option<ApiRequestRef>,
    /// Method (inline request).
    #[serde(default = "default_method")]
    pub method: String,
    /// URL (inline request).
    #[serde(default)]
    pub url: String,
    /// Headers, one `Name: value` per row (inline request).
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Body (inline request); empty for none.
    #[serde(default)]
    pub body: String,
}

fn default_method() -> String {
    "GET".into()
}

impl Default for HttpStep {
    fn default() -> Self {
        Self {
            request: None,
            method: default_method(),
            url: String::new(),
            headers: Vec::new(),
            body: String::new(),
        }
    }
}

/// A saved request of the API workspace, by id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiRequestRef {
    /// API workspace id.
    pub workspace: String,
    /// Collection id.
    pub collection: String,
    /// Request id.
    pub request: String,
    /// `Collection / Request` name when picked (display only).
    #[serde(default)]
    pub name: String,
    /// Method when picked (display only).
    #[serde(default)]
    pub method: String,
}

/// Whether `name` is a valid parameter name.
pub fn valid_param_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit())
}

impl Runbook {
    /// A new, empty runbook.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ..Self::default()
        }
    }

    /// Keychain / vault key of a secret parameter's stored value.
    pub fn secret_ref(&self, param: &str) -> SecretRef {
        SecretRef(format!("runbook:{}:{param}", self.id))
    }

    /// The parameter called `name`.
    pub fn param(&self, name: &str) -> Option<&RunbookParam> {
        self.params.iter().find(|p| p.name == name)
    }

    /// History tags of a step run from this runbook: `runbook` and `runbook:<name>`
    /// (commas, which separate stored tags, become spaces).
    pub fn history_tags(&self) -> Vec<String> {
        vec![
            "runbook".to_owned(),
            format!("runbook:{}", self.name.trim().replace(',', " ")),
        ]
    }

    /// A copy without anything secret: secret parameters' defaults are cleared. Also
    /// gives steps without an id a fresh one.
    pub fn sanitized(&self) -> Self {
        let mut r = self.clone();
        for p in &mut r.params {
            if p.secret {
                p.default.clear();
            }
        }
        for s in &mut r.steps {
            if s.id.trim().is_empty() {
                s.id = crate::random::random_hex(6);
            }
        }
        r
    }

    /// Check the runbook before storing it.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let invalid = |field: &'static str, message: String| ValidationError { field, message };
        if self.name.trim().is_empty() {
            return Err(invalid("name", "a runbook needs a title".into()));
        }
        for (i, p) in self.params.iter().enumerate() {
            if !valid_param_name(&p.name) {
                return Err(invalid(
                    "params",
                    format!(
                        "parameter {}: use letters, digits and '_' (not starting with a digit)",
                        i + 1
                    ),
                ));
            }
            if self.params[..i].iter().any(|q| q.name == p.name) {
                return Err(invalid(
                    "params",
                    format!("parameter {:?} is defined twice", p.name),
                ));
            }
        }
        for (i, s) in self.steps.iter().enumerate() {
            if let StepKind::Shell { timeout_secs, .. } = &s.kind
                && !(1..=MAX_SHELL_TIMEOUT_SECS).contains(timeout_secs)
            {
                return Err(invalid(
                    "steps",
                    format!("step {}: the time limit must be 1 to 3600 seconds", i + 1),
                ));
            }
            if self.steps[..i]
                .iter()
                .any(|t| !s.id.is_empty() && t.id == s.id)
            {
                return Err(invalid(
                    "steps",
                    format!("step {} has a duplicate id", i + 1),
                ));
            }
        }
        Ok(())
    }

    /// The runbook as export JSON (never contains secrets).
    pub fn export_json(&self) -> Result<String, serde_json::Error> {
        let mut r = self.sanitized();
        r.id.clear();
        r.created_at = 0;
        r.updated_at = 0;
        serde_json::to_string_pretty(&RunbookExport {
            version: EXPORT_VERSION,
            runbook: r,
        })
    }

    /// Parse export JSON. The result is new (empty id) and sanitized; a newer version
    /// is refused.
    pub fn import_json(json: &str) -> Result<Self, ValidationError> {
        let invalid = |message: String| ValidationError {
            field: "file",
            message,
        };
        let export: RunbookExport =
            serde_json::from_str(json).map_err(|e| invalid(format!("not a runbook file: {e}")))?;
        if export.version > EXPORT_VERSION {
            return Err(invalid(format!(
                "runbook file version {} is newer than this app supports",
                export.version
            )));
        }
        let mut r = export.runbook.sanitized();
        r.id.clear();
        r.created_at = 0;
        r.updated_at = 0;
        r.validate()?;
        Ok(r)
    }
}

/// The JSON file a runbook is exported as.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunbookExport {
    /// Format version ([`EXPORT_VERSION`]).
    pub version: u32,
    /// The runbook (no id, no secrets).
    pub runbook: Runbook,
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    pub(crate) fn sample() -> Runbook {
        let mut r = Runbook::new("Fail over replica");
        r.description = "When the primary is down.".into();
        r.params = vec![
            RunbookParam::new("region", "eu-west-1"),
            RunbookParam {
                default: "hunter2".into(),
                ..RunbookParam::secret("token")
            },
        ];
        r.steps = vec![
            RunbookStep::new(
                "Read me",
                StepKind::Note {
                    text: "# Check".into(),
                },
            ),
            RunbookStep::new(
                "",
                StepKind::Sql {
                    connection: Some(ProfileId("db1".into())),
                    sql: "SELECT * FROM t WHERE region = {{region}}".into(),
                },
            ),
            RunbookStep::new(
                "Health",
                StepKind::Http(HttpStep {
                    url: "https://example.com/{{region}}".into(),
                    headers: vec![("Authorization".into(), "Bearer {{token}}".into())],
                    ..HttpStep::default()
                }),
            ),
            RunbookStep::new(
                "Restart",
                StepKind::Shell {
                    target: ShellTarget::Host(ProfileId("h1".into())),
                    command: "systemctl restart app".into(),
                    timeout_secs: 60,
                },
            ),
            RunbookStep::new(
                "",
                StepKind::Confirm {
                    text: "Traffic back?".into(),
                },
            ),
        ];
        r
    }

    #[test]
    fn export_has_no_secrets_and_round_trips() {
        let r = sample();
        let json = r.export_json().unwrap();
        assert!(!json.contains("hunter2"));
        assert!(json.contains("\"kind\": \"shell\""));
        let back = Runbook::import_json(&json).unwrap();
        assert_eq!(back.id, "");
        assert_eq!(back.params[1].default, "");
        assert!(back.params[1].secret);
        assert_eq!(back.steps, r.steps);
        assert_eq!(back.name, r.name);
    }

    #[test]
    fn import_refuses_bad_files() {
        assert!(Runbook::import_json("{}").is_err());
        assert!(Runbook::import_json("nope").is_err());
        let newer = r#"{"version": 99, "runbook": {"name": "x"}}"#;
        assert!(Runbook::import_json(newer).is_err());
        let ok = r#"{"version": 1, "runbook": {"name": "x", "steps": [
            {"kind": "note", "text": "hi"}]}}"#;
        let r = Runbook::import_json(ok).unwrap();
        assert_eq!(r.steps.len(), 1);
        assert!(!r.steps[0].id.is_empty(), "steps get ids");
    }

    #[test]
    fn validation() {
        assert!(sample().validate().is_ok());
        assert!(Runbook::new(" ").validate().is_err());
        let mut r = sample();
        r.params.push(RunbookParam::new("region", ""));
        assert!(r.validate().is_err());
        let mut r = sample();
        r.params[0].name = "1x".into();
        assert!(r.validate().is_err());
        let mut r = sample();
        r.params[0].name = "a-b".into();
        assert!(r.validate().is_err());
        let mut r = sample();
        if let StepKind::Shell { timeout_secs, .. } = &mut r.steps[3].kind {
            *timeout_secs = 0;
        }
        assert!(r.validate().is_err());
    }

    #[test]
    fn tags_and_secret_keys() {
        let mut r = sample();
        r.name = "Fail over, eu".into();
        r.id = "abc".into();
        assert_eq!(r.history_tags(), ["runbook", "runbook:Fail over  eu"]);
        assert_eq!(r.secret_ref("token").0, "runbook:abc:token");
        assert_eq!(r.steps[1].display_title(), "SQL");
        assert_eq!(StepKind::empty("SH").unwrap().label(), "Shell");
        assert!(StepKind::empty("??").is_none());
    }
}
