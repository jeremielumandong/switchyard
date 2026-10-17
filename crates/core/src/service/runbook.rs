//! Runbooks in the service: storage commands and step execution.
//!
//! SQL steps resolve their text for the session's dialect (secrets bound, never inlined)
//! and then run through [`Service::execute`], so Production confirmation, streaming,
//! cancel and history behave as in a SQL tab. HTTP steps go through the API workspace's
//! sender; shell steps run locally (own process group, killed with the group on cancel)
//! or over the Host's shared SSH session. Every executed step is recorded in history
//! with the runbook's tags; secrets are masked in everything shown or stored.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use secrecy::SecretString;
use switchyard_api::http::{ApiWorkbenchRequest, send_api_workbench_request};
use switchyard_api::{PreparedBody, PreparedRequest};
use switchyard_db::dialect_for;
use switchyard_remote::ssh::{CommandExit, OutputStream};
use switchyard_store::{
    ApiRequestRef, HistoryEntry, HistoryStatus, HttpStep, ProfileId, Runbook, ShellTarget,
    StepKind, now_ms,
};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tracing::warn;

use super::{Service, lock};
use crate::bus::{Event, FetchLimit, QueryId, RequestId, SessionId, StatementRequest};
use crate::runbook::{
    OUTPUT_CAP, ParamValue, ParamValues, Reveal, SQL_ROW_CAP, names_in, redact, render_sql,
    render_text,
};

/// Running HTTP and shell steps, to cancel.
pub(super) type RunbookRuns = Mutex<HashMap<QueryId, oneshot::Sender<()>>>;

/// Time limit of an HTTP step.
const HTTP_TIMEOUT_MS: u64 = 60_000;
/// Largest HTTP response body kept.
const HTTP_RESPONSE_LIMIT: usize = 1024 * 1024;

/// A step that could not start, or failed.
struct StepError(String);

impl<T: std::fmt::Display> From<T> for StepError {
    fn from(e: T) -> Self {
        StepError(e.to_string())
    }
}

/// Text of a step that may hold `{{param}}` placeholders.
fn step_texts(kind: &StepKind) -> Vec<&str> {
    match kind {
        StepKind::Note { text } | StepKind::Confirm { text } => vec![text],
        StepKind::Sql { sql, .. } => vec![sql],
        StepKind::Shell { command, .. } => vec![command],
        StepKind::Http(h) => {
            let mut v = vec![h.method.as_str(), h.url.as_str(), h.body.as_str()];
            for (k, val) in &h.headers {
                v.push(k);
                v.push(val);
            }
            v
        }
    }
}

impl Service {
    /// Emit the runbooks ([`Event::Runbooks`]).
    pub(super) async fn emit_runbooks(&self) {
        match self.with_store(|s| s.runbooks()).await {
            Ok(list) => self.emit(Event::Runbooks(list)),
            Err(e) => self.error("Runbooks", e),
        }
    }

    /// [`crate::Command::SaveRunbook`].
    pub(super) async fn save_runbook(
        &self,
        request: RequestId,
        runbook: Runbook,
        secrets: Vec<(String, SecretString)>,
    ) {
        let result = self.save_runbook_inner(runbook, secrets).await;
        self.emit(Event::RunbookSaved {
            request,
            result: result.map_err(|e| e.0),
        });
        self.emit_runbooks().await;
    }

    async fn save_runbook_inner(
        &self,
        runbook: Runbook,
        secrets: Vec<(String, SecretString)>,
    ) -> Result<Runbook, StepError> {
        let id = runbook.id.clone();
        let old = if id.is_empty() {
            None
        } else {
            self.with_store(move |s| s.runbook(&id)).await?
        };
        let stored = self.with_store(move |s| s.save_runbook(&runbook)).await?;
        // Stored values of parameters that are gone or no longer secret.
        if let Some(old) = old {
            for p in old.params.iter().filter(|p| p.secret) {
                if !stored.params.iter().any(|q| q.secret && q.name == p.name) {
                    let key = old.secret_ref(&p.name);
                    self.with_secrets(move |s| s.delete(&key)).await?;
                }
            }
        }
        for (name, value) in secrets {
            if !stored.params.iter().any(|p| p.secret && p.name == name) {
                continue;
            }
            let key = stored.secret_ref(&name);
            self.with_secrets(move |s| s.set(&key, &value)).await?;
        }
        Ok(stored)
    }

    /// [`crate::Command::DeleteRunbook`].
    pub(super) async fn delete_runbook(&self, id: String) {
        match self.with_store(move |s| s.delete_runbook(&id)).await {
            Ok(Some(rb)) => {
                for p in rb.params.iter().filter(|p| p.secret) {
                    let key = rb.secret_ref(&p.name);
                    if let Err(e) = self.with_secrets(move |s| s.delete(&key)).await {
                        warn!(error = %e, "runbook secret not removed");
                    }
                }
                self.emit(Event::Toast(format!("Deleted {}", rb.name)));
            }
            Ok(None) => {}
            Err(e) => self.error("Runbooks", e),
        }
        self.emit_runbooks().await;
    }

    /// [`crate::Command::ImportRunbook`].
    pub(super) async fn import_runbook(&self, request: RequestId, path: PathBuf) {
        let result = async {
            let text = tokio::fs::read_to_string(&path).await?;
            let rb = Runbook::import_json(&text)?;
            Ok::<_, StepError>(self.with_store(move |s| s.save_runbook(&rb)).await?)
        }
        .await;
        self.emit(Event::RunbookSaved {
            request,
            result: result.map_err(|e| e.0),
        });
        self.emit_runbooks().await;
    }

    /// [`crate::Command::ExportRunbook`].
    pub(super) async fn export_runbook(&self, runbook: Runbook, path: PathBuf) {
        let result = async {
            let json = runbook.export_json()?;
            tokio::fs::write(&path, json).await?;
            Ok::<_, StepError>(())
        }
        .await;
        match result {
            Ok(()) => self.emit(Event::Toast(format!("Exported to {}", path.display()))),
            Err(e) => self.error("Export runbook", e.0),
        }
    }

    /// [`crate::Command::ListApiRequests`].
    pub(super) async fn list_api_requests(&self, request: RequestId) {
        let dir = self.data_dir.join("api");
        let result = tokio::task::spawn_blocking(move || list_saved_requests(dir))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
        self.emit(Event::ApiRequests { request, result });
    }

    /// Cancel a running HTTP or shell step. Returns whether it was one.
    pub(super) fn cancel_runbook_step(&self, query: QueryId) -> bool {
        match lock(&self.runbook_runs).remove(&query) {
            Some(tx) => {
                let _ = tx.send(());
                true
            }
            None => false,
        }
    }

    /// The values of a run: entered values, defaults, then stored secrets for secret
    /// parameters the step uses. A secret parameter with no value is an error.
    async fn run_values(
        &self,
        rb: &Runbook,
        kind: &StepKind,
        entered: ParamValues,
    ) -> Result<ParamValues, StepError> {
        let mut values = ParamValues::new();
        for p in &rb.params {
            match entered.get(&p.name) {
                // A secret parameter's value is a secret however the UI sent it.
                Some(ParamValue::Plain(v)) if p.secret && !v.is_empty() => {
                    values.insert(&p.name, ParamValue::Secret(SecretString::from(v.clone())))
                }
                Some(ParamValue::Plain(v)) if p.secret && v.is_empty() => {}
                Some(v) => values.insert(&p.name, v.clone()),
                None => {}
            }
        }
        let mut values = values.with_defaults(rb);
        let used: Vec<String> = step_texts(kind).into_iter().flat_map(names_in).collect();
        for p in rb.params.iter().filter(|p| p.secret) {
            if values.contains(&p.name) || !used.contains(&p.name) {
                continue;
            }
            let key = rb.secret_ref(&p.name);
            let stored = if rb.id.is_empty() {
                None
            } else {
                self.with_secrets(move |s| s.get(&key)).await?
            };
            match stored {
                Some(v) => values.insert(&p.name, ParamValue::Secret(v)),
                None => {
                    return Err(StepError(format!(
                        "the secret parameter {{{{{}}}}} has no value: enter it before running",
                        p.name
                    )));
                }
            }
        }
        Ok(values)
    }

    /// [`crate::Command::RunbookStep`].
    pub(super) async fn runbook_step(
        &self,
        query: QueryId,
        session: Option<SessionId>,
        runbook: Runbook,
        step: usize,
        entered: ParamValues,
        confirmed_destructive: bool,
    ) {
        let started = Instant::now();
        let fail = |msg: String| Event::RunbookStepDone {
            query,
            result: Err(msg),
            elapsed: started.elapsed(),
            cancelled: false,
        };
        let Some(kind) = runbook.steps.get(step).map(|s| s.kind.clone()) else {
            return self.emit(fail("the step no longer exists".into()));
        };
        let values = match self.run_values(&runbook, &kind, entered).await {
            Ok(v) => v,
            Err(e) => return self.emit(fail(e.0)),
        };
        let mut tags = runbook.history_tags();
        match kind {
            StepKind::Sql { .. } => {
                let Some(session) = session else {
                    return self.emit(fail("the step's connection is not open".into()));
                };
                if let Err(e) = self
                    .runbook_sql(query, session, &kind, &values, tags, confirmed_destructive)
                    .await
                {
                    self.emit(fail(e.0));
                }
            }
            StepKind::Http(h) => {
                tags.push("http".into());
                self.runbook_http(query, h, values, tags, started).await
            }
            StepKind::Shell {
                target,
                command,
                timeout_secs,
            } => {
                tags.push("shell".into());
                self.runbook_shell(
                    query,
                    target,
                    command,
                    Duration::from_secs(timeout_secs.max(1)),
                    values,
                    tags,
                    started,
                )
                .await
            }
            StepKind::Note { .. } | StepKind::Confirm { .. } => self.emit(Event::RunbookStepDone {
                query,
                result: Ok(String::new()),
                elapsed: started.elapsed(),
                cancelled: false,
            }),
        }
    }

    async fn runbook_sql(
        &self,
        query: QueryId,
        session: SessionId,
        kind: &StepKind,
        values: &ParamValues,
        tags: Vec<String>,
        confirmed_destructive: bool,
    ) -> Result<(), StepError> {
        let StepKind::Sql { sql, .. } = kind else {
            return Ok(());
        };
        let Some(slot) = self.slot(session) else {
            return Err(StepError("the step's connection is not open".into()));
        };
        let dialect = dialect_for(slot.connection.engine);
        let resolved = render_sql(dialect, sql, values)?;
        if resolved.statements.is_empty() {
            return Err(StepError("the step has no SQL".into()));
        }
        let mut text = resolved.display.clone();
        if resolved.secrets_bound > 0 {
            text.push_str(&format!(
                "\n-- {} secret parameter{} bound",
                resolved.secrets_bound,
                if resolved.secrets_bound == 1 { "" } else { "s" }
            ));
        }
        self.emit(Event::RunbookResolved { query, text });
        let statements = resolved
            .statements
            .into_iter()
            .map(|s| StatementRequest {
                sql: s.sql,
                params: s.params,
                offset: 0,
            })
            .collect();
        self.execute(
            session,
            query,
            statements,
            tags,
            confirmed_destructive,
            FetchLimit::Rows(SQL_ROW_CAP),
            false,
        )
        .await;
        Ok(())
    }

    /// Register a cancellable step; the returned future completes on cancel.
    fn runbook_cancel(&self, query: QueryId) -> impl Future<Output = ()> + use<> {
        let (tx, rx) = oneshot::channel();
        lock(&self.runbook_runs).insert(query, tx);
        async move {
            let _ = rx.await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn runbook_done(
        &self,
        query: QueryId,
        started: Instant,
        result: Result<String, String>,
        cancelled: bool,
        history: (Option<ProfileId>, String, String),
        tags: Vec<String>,
    ) {
        lock(&self.runbook_runs).remove(&query);
        let elapsed = started.elapsed();
        let (connection_id, connection_name, text) = history;
        let entry = HistoryEntry {
            id: 0,
            connection_id,
            connection_name,
            sql: text,
            started_at: now_ms() - elapsed.as_millis() as i64,
            duration_ms: elapsed.as_millis() as i64,
            rows: None,
            affected: None,
            status: if cancelled {
                HistoryStatus::Cancelled
            } else if result.is_ok() {
                HistoryStatus::Ok
            } else {
                HistoryStatus::Error
            },
            error: result.as_ref().err().cloned(),
            tags,
            has_plan: false,
        };
        if let Err(e) = self.with_store(move |s| s.add_history(&entry)).await {
            warn!(error = %e, "runbook history write failed");
        }
        self.emit(Event::RunbookStepDone {
            query,
            result,
            elapsed,
            cancelled,
        });
    }

    async fn runbook_http(
        &self,
        query: QueryId,
        h: HttpStep,
        values: ParamValues,
        tags: Vec<String>,
        started: Instant,
    ) {
        let secrets = values.secret_texts();
        let prepared = match &h.request {
            Some(r) => {
                let dir = self.data_dir.join("api");
                let backend = self.secrets.clone();
                let (r, v) = (r.clone(), values.clone());
                tokio::task::spawn_blocking(move || compile_saved(dir, backend, &r, &v))
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r)
            }
            None => inline_request(&h, &values),
        };
        let (request, display) = match prepared {
            Ok((req, extra_redactions)) => {
                let mut hide = secrets.clone();
                hide.extend(extra_redactions);
                let display = describe_request(&req, &hide);
                (req, display)
            }
            Err(e) => {
                return self.emit(Event::RunbookStepDone {
                    query,
                    result: Err(e),
                    elapsed: started.elapsed(),
                    cancelled: false,
                });
            }
        };
        self.emit(Event::RunbookResolved {
            query,
            text: display.clone(),
        });
        let cancel = self.runbook_cancel(query);
        let (result, cancelled) = tokio::select! {
            r = send_api_workbench_request(request) => (r, false),
            () = cancel => (Err("cancelled".to_owned()), true),
        };
        let result = match result {
            Ok(resp) => {
                let mut text = format!("HTTP {} {}\n", resp.status, resp.reason);
                for (k, v) in &resp.headers {
                    text.push_str(&format!("{k}: {v}\n"));
                }
                text.push('\n');
                text.push_str(&String::from_utf8_lossy(&resp.body));
                if resp.truncated {
                    text.push_str("\n[response truncated]");
                }
                let mut shown = String::new();
                crate::runbook::append_capped(&mut shown, &redact(&text, &secrets));
                self.emit(Event::RunbookOutput { query, text: shown });
                let summary = format!("{} {}", resp.status, resp.reason).trim().to_owned();
                if (200..400).contains(&resp.status) {
                    Ok(summary)
                } else {
                    Err(format!("HTTP {summary}"))
                }
            }
            Err(e) => Err(redact(&e, &secrets)),
        };
        let first_line = display.lines().next().unwrap_or_default().to_owned();
        self.runbook_done(
            query,
            started,
            result,
            cancelled,
            (None, "HTTP".into(), first_line),
            tags,
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn runbook_shell(
        &self,
        query: QueryId,
        target: ShellTarget,
        command: String,
        timeout: Duration,
        values: ParamValues,
        tags: Vec<String>,
        started: Instant,
    ) {
        let secrets = values.secret_texts();
        let (run, shown) = match (
            render_text(&command, &values, Reveal::Secrets),
            render_text(&command, &values, Reveal::Masked),
        ) {
            (Ok(r), Ok(s)) => (r, s),
            (Err(e), _) | (_, Err(e)) => {
                return self.emit(Event::RunbookStepDone {
                    query,
                    result: Err(e.to_string()),
                    elapsed: started.elapsed(),
                    cancelled: false,
                });
            }
        };
        if run.trim().is_empty() {
            return self.emit(Event::RunbookStepDone {
                query,
                result: Err("the command is empty".into()),
                elapsed: started.elapsed(),
                cancelled: false,
            });
        }
        self.emit(Event::RunbookResolved {
            query,
            text: shown.clone(),
        });
        let cancel = self.runbook_cancel(query);
        let mut sent = 0usize;
        let emit = |_stream: OutputStream, bytes: &[u8]| {
            if sent >= OUTPUT_CAP {
                return;
            }
            let text = redact(&String::from_utf8_lossy(bytes), &secrets);
            let mut chunk = String::new();
            let all = crate::runbook::append_limited(&mut chunk, &text, OUTPUT_CAP - sent);
            sent += chunk.len();
            if !all {
                chunk.push_str("\n[output truncated]");
                sent = OUTPUT_CAP;
            }
            self.emit(Event::RunbookOutput { query, text: chunk });
        };
        let (history_conn, history_name, outcome) = match &target {
            ShellTarget::Local => (
                None,
                "Local shell".to_owned(),
                run_local(&run, timeout, cancel, emit)
                    .await
                    .map_err(|e| e.to_string()),
            ),
            ShellTarget::Host(id) => {
                let name = self
                    .host(id)
                    .await
                    .map(|h| h.name)
                    .unwrap_or_else(|_| id.0.clone());
                let outcome = async {
                    let target = self.ssh_target(id).await.map_err(|e| e.to_string())?;
                    let conn = self.ssh.session(&target).await.map_err(|e| e.to_string())?;
                    conn.run_command_streaming(&run, timeout, cancel, emit)
                        .await
                        .map_err(|e| e.to_string())
                }
                .await;
                (Some(id.clone()), name, outcome)
            }
        };
        let cancelled = matches!(&outcome, Ok(x) if x.cancelled);
        let result = match outcome {
            Ok(x) if x.cancelled => Err("cancelled".to_owned()),
            Ok(x) if x.timed_out => Err(format!("stopped after {} s", timeout.as_secs())),
            Ok(CommandExit {
                exit_status: Some(0),
                ..
            }) => Ok("exit 0".to_owned()),
            Ok(CommandExit {
                exit_status: Some(c),
                ..
            }) => Err(format!("exit status {c}")),
            Ok(_) => Err("ended without an exit status".to_owned()),
            Err(e) => Err(redact(&e, &secrets)),
        };
        self.runbook_done(
            query,
            started,
            result,
            cancelled,
            (history_conn, history_name, shown),
            tags,
        )
        .await;
    }
}

/// `METHOD URL`, headers and body of a request, with `hide` masked.
fn describe_request(req: &ApiWorkbenchRequest, hide: &[String]) -> String {
    let mut s = format!("{} {}", req.method, req.url);
    for (k, v) in &req.headers {
        s.push_str(&format!("\n{k}: {v}"));
    }
    if let Some(body) = &req.body
        && !body.is_empty()
    {
        s.push_str("\n\n");
        s.push_str(&String::from_utf8_lossy(body));
    }
    redact(&s, hide)
}

/// An inline HTTP step with its parameters substituted.
fn inline_request(
    h: &HttpStep,
    values: &ParamValues,
) -> Result<(ApiWorkbenchRequest, Vec<String>), String> {
    let r = |t: &str| render_text(t, values, Reveal::Secrets).map_err(|e| e.to_string());
    let method = r(&h.method)?.trim().to_ascii_uppercase();
    let url = r(&h.url)?.trim().to_owned();
    let mut headers = Vec::with_capacity(h.headers.len());
    for (k, v) in &h.headers {
        headers.push((r(k)?, r(v)?));
    }
    let body = r(&h.body)?;
    Ok((
        ApiWorkbenchRequest {
            method: if method.is_empty() {
                "GET".into()
            } else {
                method
            },
            url,
            headers,
            body: (!body.is_empty()).then(|| body.into_bytes()),
            content_type: None,
            allow_private_network: true,
            timeout_ms: HTTP_TIMEOUT_MS,
            max_redirects: 5,
            response_limit_bytes: HTTP_RESPONSE_LIMIT,
            aws_sigv4: None,
        },
        Vec::new(),
    ))
}

fn api_store(dir: &std::path::Path) -> Result<Option<switchyard_api::WorkbenchStore>, String> {
    if !dir.join("workbench.db").exists() {
        return Ok(None);
    }
    switchyard_api::WorkbenchStore::open(dir)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// Every saved request of the API workspace.
fn list_saved_requests(dir: PathBuf) -> Result<Vec<ApiRequestRef>, String> {
    let Some(store) = api_store(&dir)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for ws in store.list_workspaces().map_err(|e| e.to_string())? {
        for c in store.list_collections(&ws.id).map_err(|e| e.to_string())? {
            for r in store.list_requests(&c.id).map_err(|e| e.to_string())? {
                out.push(ApiRequestRef {
                    workspace: ws.id.as_str().to_owned(),
                    collection: c.id.as_str().to_owned(),
                    request: r.id.as_str().to_owned(),
                    name: format!("{} / {} / {}", ws.name, c.name, r.name),
                    method: r.method.as_str().to_owned(),
                });
            }
        }
    }
    Ok(out)
}

/// A saved API request compiled with its collection, folders, active environment and
/// globals; runbook parameters are its local variables. Returns the request and values
/// to mask (the compiler's redactions).
fn compile_saved(
    dir: PathBuf,
    backend: std::sync::Arc<dyn switchyard_store::SecretStore>,
    r: &ApiRequestRef,
    values: &ParamValues,
) -> Result<(ApiWorkbenchRequest, Vec<String>), String> {
    use switchyard_api::{
        CollectionId, CompileContext, RequestId as ApiRequestId, RowId, Variable, VariableValue,
        WorkspaceId, WorkspaceSecretResolver, compile_request_with_folder_chain,
    };
    let missing = || format!("the saved request {:?} no longer exists", r.name);
    let store = api_store(&dir)?.ok_or_else(missing)?;
    let ws = WorkspaceId::new(r.workspace.clone())?;
    let cid = CollectionId::parse(r.collection.clone())?;
    let rid = ApiRequestId::parse(r.request.clone())?;
    let saved = store
        .request(&cid, &rid)
        .map_err(|e| e.to_string())?
        .ok_or_else(missing)?;
    let collection = store.collection(&ws, &cid).map_err(|e| e.to_string())?;
    let folders = store.list_folders(&cid).map_err(|e| e.to_string())?;
    let chain = switchyard_api::runtime::folder_chain(&folders, &saved);
    let env = store
        .list_environments(&ws)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|e| e.active);
    let globals = store.global_variables(&ws).map_err(|e| e.to_string())?;
    let local: Vec<Variable> = values
        .names()
        .into_iter()
        .filter_map(|n| {
            let v = values.expose(&n)?;
            Some(Variable {
                id: RowId::new(),
                key: n,
                value: VariableValue::Plain(v.to_owned()),
                enabled: true,
                description: String::new(),
            })
        })
        .collect();
    let secrets = crate::api_secrets::ApiSecrets::new(backend);
    let resolver = WorkspaceSecretResolver::new(&ws, &secrets);
    let empty: Vec<Variable> = Vec::new();
    let ctx = CompileContext {
        global: &globals,
        environment: env.as_ref().map_or(&empty, |e| &e.variables),
        data: &empty,
        local: &local,
        secrets: &resolver,
        environment_base_url: env.as_ref().map(|e| e.base_url.as_str()),
        environment_auth: env.as_ref().map(|e| &e.auth),
    };
    let (prepared, _) =
        compile_request_with_folder_chain(&saved, collection.as_ref(), &chain, &ctx)
            .map_err(|e| e.to_string())?;
    to_workbench_request(prepared)
}

fn to_workbench_request(p: PreparedRequest) -> Result<(ApiWorkbenchRequest, Vec<String>), String> {
    let (body, content_type) = match p.body {
        PreparedBody::None => (None, None),
        PreparedBody::Bytes {
            content_type,
            bytes,
        } => (Some(bytes), Some(content_type).filter(|c| !c.is_empty())),
        PreparedBody::Multipart(_) | PreparedBody::File(_) => {
            return Err(
                "multipart and file bodies are not supported in runbook steps yet".to_owned(),
            );
        }
    };
    Ok((
        ApiWorkbenchRequest {
            method: p.method.as_str().to_owned(),
            url: p.url,
            headers: p.headers,
            body,
            content_type,
            allow_private_network: p.settings.allow_private_network,
            timeout_ms: p.settings.timeout_ms.clamp(100, 300_000),
            max_redirects: if p.settings.follow_redirects {
                p.settings.max_redirects.min(10)
            } else {
                0
            },
            response_limit_bytes: HTTP_RESPONSE_LIMIT,
            aws_sigv4: p
                .aws_sigv4
                .map(|s| switchyard_api::http::ApiWorkbenchAwsSigV4 {
                    access_key: s.access_key,
                    secret_key: s.secret_key,
                    region: s.region,
                    service: s.service,
                    session_token: s.session_token,
                }),
        },
        p.redactions,
    ))
}

/// The platform shell running `command`, in its own process group.
fn shell_command(command: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(command);
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        c.process_group(0);
        c
    }
}

/// Stop a local command and everything it started.
async fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(unix)]
    {
        // procps `kill` needs the `--` before a negative pid (a group), or it signals
        // nothing.
        let _ = tokio::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    #[cfg(windows)]
    {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .creation_flags(0x0800_0000)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
}

async fn read_some<R: AsyncRead + Unpin>(
    r: &mut Option<R>,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    match r {
        Some(r) => r.read(buf).await,
        None => std::future::pending().await,
    }
}

/// Run `command` locally, streaming its output, until it exits, `timeout` passes or
/// `cancel` completes (then its process group is killed).
pub(crate) async fn run_local(
    command: &str,
    timeout: Duration,
    cancel: impl Future<Output = ()>,
    mut on_output: impl FnMut(OutputStream, &[u8]),
) -> std::io::Result<CommandExit> {
    let mut child = shell_command(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let pid = child.id();
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let (mut b1, mut b2) = (vec![0u8; 8192], vec![0u8; 8192]);
    let deadline = tokio::time::Instant::now() + timeout;
    tokio::pin!(cancel);
    let mut exit = CommandExit::default();
    loop {
        if out.is_none() && err.is_none() {
            tokio::select! {
                s = child.wait() => {
                    exit.exit_status = s?.code().map(|c| c as u32);
                    return Ok(exit);
                }
                () = &mut cancel => exit.cancelled = true,
                () = tokio::time::sleep_until(deadline) => exit.timed_out = true,
            }
            break;
        }
        tokio::select! {
            () = &mut cancel => {
                exit.cancelled = true;
                break;
            }
            () = tokio::time::sleep_until(deadline) => {
                exit.timed_out = true;
                break;
            }
            r = read_some(&mut out, &mut b1) => match r {
                Ok(n) if n > 0 => on_output(OutputStream::Stdout, &b1[..n]),
                _ => out = None,
            },
            r = read_some(&mut err, &mut b2) => match r {
                Ok(n) if n > 0 => on_output(OutputStream::Stderr, &b2[..n]),
                _ => err = None,
            },
        }
    }
    kill_tree(pid).await;
    let _ = child.kill().await;
    exit.exit_status = None;
    Ok(exit)
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn local_commands_stream_and_report_status() {
        let mut got = String::new();
        let x = run_local(
            "echo one; echo two >&2; exit 3",
            Duration::from_secs(10),
            std::future::pending(),
            |_, b| got.push_str(&String::from_utf8_lossy(b)),
        )
        .await
        .unwrap();
        assert_eq!(x.exit_status, Some(3));
        assert!(got.contains("one") && got.contains("two"));
    }

    #[tokio::test]
    async fn local_commands_cancel_and_time_out() {
        let started = Instant::now();
        let x = run_local(
            "sleep 30 & sleep 30",
            Duration::from_secs(30),
            tokio::time::sleep(Duration::from_millis(200)),
            |_, _| {},
        )
        .await
        .unwrap();
        assert!(x.cancelled);
        assert!(started.elapsed() < Duration::from_secs(10));
        let x = run_local(
            "sleep 30",
            Duration::from_millis(200),
            std::future::pending(),
            |_, _| {},
        )
        .await
        .unwrap();
        assert!(x.timed_out);
    }

    #[test]
    fn inline_requests_substitute_and_mask() {
        let h = HttpStep {
            method: "post".into(),
            url: "https://example.com/{{region}}".into(),
            headers: vec![("Authorization".into(), "Bearer {{token}}".into())],
            body: "{\"r\": \"{{region}}\"}".into(),
            request: None,
        };
        let v = ParamValues::new()
            .plain("region", "eu")
            .secret("token", SecretString::from("tok123"));
        let (req, _) = inline_request(&h, &v).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://example.com/eu");
        assert_eq!(req.headers[0].1, "Bearer tok123");
        let shown = describe_request(&req, &v.secret_texts());
        assert!(!shown.contains("tok123"));
        assert!(shown.starts_with("POST https://example.com/eu\nAuthorization: Bearer ••••••"));
        assert!(shown.ends_with("{\"r\": \"eu\"}"));
    }

    #[test]
    fn texts_of_steps() {
        let k = StepKind::Http(HttpStep {
            headers: vec![("A".into(), "{{x}}".into())],
            ..HttpStep::default()
        });
        assert!(step_texts(&k).contains(&"{{x}}"));
    }
}
