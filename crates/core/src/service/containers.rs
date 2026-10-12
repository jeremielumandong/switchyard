//! Docker and Kubernetes connections ([`Command::ContainersList`] and friends): the
//! user's own `docker` / `kubectl` run on this computer (`tokio::process`, killed on
//! cancel) or on the connection's Host over its shared SSH session.
//!
//! Every call is registered by request id so [`Command::ContainersCancel`] can stop it;
//! actions are refused on read-only connections, and destructive ones on Production
//! unless the user confirmed them in the app. Nothing here is reachable from the MCP
//! server or `swy`.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::future::{AbortHandle, Abortable};
use switchyard_containers::{
    self as ct, ContainersError, Invocation, LineSplitter, Target, Tool, local,
};
use switchyard_remote::ssh::{ExecChunk, SshConn};
use switchyard_store::{ContainerConnection, Profile, ProfileId};
use switchyard_term::{LocalShell, TermSize, Terminal};
use tokio::io::AsyncBufReadExt as _;
use tracing::{info, warn};

use super::{Service, lock};
use crate::bus::{ContainerForward, Event, ForwardState, RequestId, TermId};
use crate::terminals::SshTerminalSpec;

/// Limits for listings / inspect and for actions (a stop waits up to 10 s per container).
const LIST_TIMEOUT: Duration = Duration::from_secs(60);
const ACTION_TIMEOUT: Duration = Duration::from_secs(180);
/// Output kept from a command on a Host.
const OUTPUT_CAP: usize = 32 * 1024 * 1024;
/// Exit status of a shell that did not find the program.
const NOT_FOUND: u32 = 127;

/// A running (or failed) port forward.
pub(super) struct ForwardSlot {
    info: ContainerForward,
    abort: Option<AbortHandle>,
}

type CtResult<T> = Result<T, ContainersError>;

/// The connection's CLI settings, with the tab's context instead of the saved one.
fn target_of(c: &ContainerConnection, context: Option<String>) -> Target {
    let context = context
        .filter(|s| !s.trim().is_empty())
        .or_else(|| c.context.clone());
    match c.platform {
        switchyard_store::ContainerPlatform::Docker => Target {
            docker_context: context,
            ..Target::default()
        },
        switchyard_store::ContainerPlatform::Kubernetes => Target {
            docker_context: None,
            kubeconfig: c.kubeconfig.clone(),
            kube_context: context,
        },
    }
}

/// The executable the connection names for `tool` (only for its own platform's CLI).
fn configured(c: &ContainerConnection, tool: Tool) -> Option<&str> {
    let own = match c.platform {
        switchyard_store::ContainerPlatform::Docker => Tool::Docker,
        switchyard_store::ContainerPlatform::Kubernetes => Tool::Kubectl,
    };
    c.executable
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty() && tool == own)
}

fn program_word(c: &ContainerConnection, tool: Tool) -> String {
    configured(c, tool).unwrap_or(tool.program()).to_owned()
}

fn io<E: std::fmt::Display>(e: E) -> ContainersError {
    ContainersError::Io(e.to_string())
}

/// Turn a Host command's exit status into a result.
fn remote_result(tool: Tool, status: Option<u32>, stderr: &str, stdout: &str) -> CtResult<()> {
    match status {
        Some(0) => Ok(()),
        Some(NOT_FOUND) => Err(ContainersError::Missing(tool)),
        code => {
            let code = code.and_then(|c| i32::try_from(c).ok());
            Err(ContainersError::Failed {
                tool,
                code,
                message: local::error_text(stderr, stdout, code),
            })
        }
    }
}

impl Service {
    async fn container_conn(&self, id: &ProfileId) -> CtResult<ContainerConnection> {
        let id = id.clone();
        match self.with_store(move |s| s.profile(&id)).await {
            Ok(Some(Profile::Containers(c))) => Ok(c),
            Ok(_) => Err(ContainersError::Io("connection not found".into())),
            Err(e) => Err(io(e)),
        }
    }

    async fn container_ssh(&self, host: &ProfileId) -> CtResult<Arc<SshConn>> {
        let target = self.ssh_target(host).await.map_err(io)?;
        self.ssh.session(&target).await.map_err(io)
    }

    /// Run a call to completion; its standard output.
    pub(super) async fn container_run(
        &self,
        c: &ContainerConnection,
        inv: &Invocation,
        timeout: Duration,
    ) -> CtResult<String> {
        match &c.host_id {
            None => {
                let program = local::find_program(inv.tool, configured(c, inv.tool))
                    .ok_or(ContainersError::Missing(inv.tool))?;
                local::run(&program, inv, timeout).await
            }
            Some(host) => {
                let conn = self.container_ssh(host).await?;
                let line = inv.shell_line(&program_word(c, inv.tool));
                let out = conn
                    .run_command(&line, OUTPUT_CAP, timeout)
                    .await
                    .map_err(io)?;
                if out.timed_out {
                    return Err(ContainersError::TimedOut {
                        tool: inv.tool,
                        secs: timeout.as_secs(),
                    });
                }
                let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&out.stderr);
                remote_result(inv.tool, out.exit_status, &stderr, &stdout)?;
                if out.truncated {
                    return Err(ContainersError::Failed {
                        tool: inv.tool,
                        code: None,
                        message: "The output was too large to read".into(),
                    });
                }
                Ok(stdout)
            }
        }
    }

    /// The connection's target, with a Rancher cluster's managed kubeconfig (generated
    /// when missing, or again with `refresh`).
    pub(super) async fn container_target(
        &self,
        c: &ContainerConnection,
        context: Option<String>,
        refresh: bool,
    ) -> CtResult<Target> {
        let mut target = target_of(c, context);
        if c.rancher.is_some() {
            if c.host_id.is_some() {
                return Err(ContainersError::Unsupported(
                    "Rancher clusters run kubectl on this computer".into(),
                ));
            }
            let path = self.rancher_kubeconfig(c, None, refresh).await?;
            target.kubeconfig = Some(path.to_string_lossy().into_owned());
        }
        Ok(target)
    }

    /// Run the call `make` builds for `c`; a kubectl sign-in failure becomes
    /// [`ContainersError::SignIn`] with a hint, and a Rancher cluster gets a fresh
    /// kubeconfig and one more try.
    async fn kube_call(
        &self,
        c: &ContainerConnection,
        context: Option<String>,
        make: impl Fn(&Target) -> Invocation,
        timeout: Duration,
    ) -> CtResult<String> {
        let target = self.container_target(c, context.clone(), false).await?;
        match self.container_run(c, &make(&target), timeout).await {
            Err(e) => match classify(c, e) {
                ContainersError::SignIn { .. } if c.rancher.is_some() => {
                    let target = self.container_target(c, context, true).await?;
                    self.container_run(c, &make(&target), timeout)
                        .await
                        .map_err(|e| classify(c, e))
                }
                e => Err(e),
            },
            ok => ok,
        }
    }

    /// Run `fut` as request `request`, stoppable with [`Command::ContainersCancel`].
    ///
    /// [`Command::ContainersCancel`]: crate::Command::ContainersCancel
    async fn container_op<T>(
        &self,
        request: RequestId,
        fut: impl Future<Output = CtResult<T>>,
    ) -> CtResult<T> {
        let (handle, registration) = AbortHandle::new_pair();
        lock(&self.container_ops).insert(request, handle);
        let r = Abortable::new(fut, registration).await;
        lock(&self.container_ops).remove(&request);
        r.unwrap_or(Err(ContainersError::Cancelled))
    }

    /// [`Command::ContainersCancel`](crate::Command::ContainersCancel).
    pub(super) fn containers_cancel(&self, request: RequestId) {
        if let Some(h) = lock(&self.container_ops).remove(&request) {
            h.abort();
        }
    }

    /// [`Command::ContainersList`](crate::Command::ContainersList).
    pub(super) async fn containers_list(
        &self,
        request: RequestId,
        connection: ProfileId,
        context: Option<String>,
        query: ct::Query,
    ) {
        let result = self
            .container_op(request, async {
                let c = self.container_conn(&connection).await?;
                let out = self
                    .kube_call(&c, context, |t| query.invocation(t), LIST_TIMEOUT)
                    .await?;
                let mut listing = query.parse(&out)?;
                if let ct::Listing::Contexts(list) = &mut listing
                    && c.host_id.is_none()
                {
                    mark_missing_plugins(list);
                }
                Ok(listing)
            })
            .await;
        self.emit(Event::ContainersListed { request, result });
    }

    /// [`Command::ContainersInspect`](crate::Command::ContainersInspect).
    pub(super) async fn containers_inspect(
        &self,
        request: RequestId,
        connection: ProfileId,
        context: Option<String>,
        inspect: ct::Inspect,
    ) {
        let result = self
            .container_op(request, async {
                let c = self.container_conn(&connection).await?;
                self.kube_call(&c, context, |t| inspect.invocation(t), LIST_TIMEOUT)
                    .await
            })
            .await;
        self.emit(Event::ContainersInspected { request, result });
    }

    /// [`Command::ContainersAction`](crate::Command::ContainersAction).
    pub(super) async fn containers_action(
        &self,
        request: RequestId,
        connection: ProfileId,
        context: Option<String>,
        action: ct::Action,
        confirmed: bool,
    ) {
        let result = self
            .container_op(request, async {
                let c = self.container_conn(&connection).await?;
                check_action(&c, &action, confirmed)?;
                info!(
                    connection = %c.name,
                    action = action.label(),
                    subject = action.subject(),
                    "container action"
                );
                self.kube_call(&c, context, |t| action.invocation(t), ACTION_TIMEOUT)
                    .await?;
                Ok(action.done_message())
            })
            .await;
        if let Err(e) = &result {
            warn!(error = %e, "container action failed");
        }
        self.emit(Event::ContainersActionDone { request, result });
    }

    /// [`Command::ContainersLogs`](crate::Command::ContainersLogs).
    pub(super) async fn containers_logs(
        &self,
        request: RequestId,
        connection: ProfileId,
        context: Option<String>,
        source: ct::LogSource,
        options: ct::LogOptions,
    ) {
        let events = self.events.clone();
        let deliver = move |lines: Vec<String>| {
            events.emit(Event::ContainersLog { request, lines });
        };
        let result = self
            .container_op(request, async {
                let c = self.container_conn(&connection).await?;
                let inv =
                    source.invocation(options, &self.container_target(&c, context, false).await?);
                let r = match &c.host_id {
                    None => {
                        let program = local::find_program(inv.tool, configured(&c, inv.tool))
                            .ok_or(ContainersError::Missing(inv.tool))?;
                        local::stream(&program, &inv, deliver).await
                    }
                    Some(host) => {
                        let conn = self.container_ssh(host).await?;
                        let line = inv.shell_line(&program_word(&c, inv.tool));
                        // A followed log gets a PTY so closing the channel stops it.
                        stream_remote(&conn, &line, inv.tool, options.follow, deliver).await
                    }
                };
                r.map_err(|e| classify(&c, e))
            })
            .await;
        let result = match result {
            Err(ContainersError::Cancelled) => Ok(()),
            r => r,
        };
        self.emit(Event::ContainersLogEnded { request, result });
    }

    /// Emit [`Event::ContainerForwards`].
    pub(super) fn emit_container_forwards(&self) {
        let mut list: Vec<ContainerForward> = lock(&self.container_forwards)
            .values()
            .map(|s| s.info.clone())
            .collect();
        list.sort_by_key(|f| f.id);
        self.emit(Event::ContainerForwards(list));
    }

    fn set_forward_state(&self, id: u64, state: ForwardState) {
        if let Some(slot) = lock(&self.container_forwards).get_mut(&id) {
            slot.info.state = state;
            if !matches!(
                slot.info.state,
                ForwardState::Starting | ForwardState::Running(_)
            ) {
                slot.abort = None;
            }
        }
        self.emit_container_forwards();
    }

    /// [`Command::ContainersStopForward`](crate::Command::ContainersStopForward).
    pub(super) fn containers_stop_forward(&self, id: u64) {
        if let Some(slot) = lock(&self.container_forwards).remove(&id)
            && let Some(a) = slot.abort
        {
            a.abort();
        }
        self.emit_container_forwards();
    }

    /// [`Command::ContainersForward`](crate::Command::ContainersForward): runs until the
    /// forward stops (this task holds the `kubectl` child).
    pub(super) async fn containers_forward(
        &self,
        id: u64,
        connection: ProfileId,
        context: Option<String>,
        forward: ct::PortForward,
    ) {
        let (handle, registration) = AbortHandle::new_pair();
        lock(&self.container_forwards).insert(
            id,
            ForwardSlot {
                info: ContainerForward {
                    id,
                    connection: connection.clone(),
                    forward: forward.clone(),
                    state: ForwardState::Starting,
                },
                abort: Some(handle),
            },
        );
        self.emit_container_forwards();
        let run = async {
            let c = self.container_conn(&connection).await?;
            if c.host_id.is_some() {
                return Err(ContainersError::Unsupported(
                    "Port forwards run kubectl on this computer; this connection runs it on \
                     a Host. Open a shell there, or add a local connection for the cluster."
                        .into(),
                ));
            }
            let inv = forward.invocation(&self.container_target(&c, context, false).await?);
            let program = local::find_program(inv.tool, configured(&c, inv.tool))
                .ok_or(ContainersError::Missing(inv.tool))?;
            let mut child = local::spawn(&program, &inv)?;
            let (Some(out), Some(err)) = (child.stdout.take(), child.stderr.take()) else {
                return Err(ContainersError::Io("no output pipe".into()));
            };
            let mut out = tokio::io::BufReader::new(out).lines();
            let mut err = tokio::io::BufReader::new(err).lines();
            let mut err_tail: Vec<String> = Vec::new();
            let (mut out_open, mut err_open) = (true, true);
            let mut running = false;
            while out_open || err_open {
                tokio::select! {
                    l = out.next_line(), if out_open => match l {
                        Ok(Some(l)) => {
                            if !running && let Some(port) = ct::forwarding_port(&l) {
                                running = true;
                                self.set_forward_state(id, ForwardState::Running(port));
                            }
                        }
                        _ => out_open = false,
                    },
                    l = err.next_line(), if err_open => match l {
                        Ok(Some(l)) => {
                            err_tail.push(l);
                            let keep = err_tail.len().saturating_sub(10);
                            err_tail.drain(..keep);
                        }
                        _ => err_open = false,
                    },
                }
            }
            let status = child.wait().await.map_err(io)?;
            Err(classify(
                &c,
                ContainersError::Failed {
                    tool: Tool::Kubectl,
                    code: status.code(),
                    message: local::error_text(&err_tail.join("\n"), "", status.code()),
                },
            ))
        };
        match Abortable::new(run, registration).await {
            // Stopped by the user: the slot is gone already.
            Err(_) => {}
            Ok(Ok(())) => self.set_forward_state(id, ForwardState::Failed("Stopped".into())),
            Ok(Err(e)) => {
                warn!(error = %e, "port forward ended");
                self.set_forward_state(id, ForwardState::Failed(e.to_string()));
            }
        }
    }

    /// A terminal running `docker exec -it` / `kubectl exec -it` (for
    /// [`TermTarget::Container`](crate::TermTarget::Container)).
    pub(super) async fn container_terminal(
        &self,
        term: TermId,
        connection: ProfileId,
        context: Option<String>,
        shell: ct::ShellTarget,
        size: TermSize,
    ) -> Result<(Terminal, String), String> {
        let c = self
            .container_conn(&connection)
            .await
            .map_err(|e| e.to_string())?;
        let target = self
            .container_target(&c, context, false)
            .await
            .map_err(|e| e.to_string())?;
        let inv = shell.invocation(&target);
        let description = shell.title();
        match &c.host_id {
            None => {
                let program = local::find_program(inv.tool, configured(&c, inv.tool))
                    .ok_or_else(|| ContainersError::Missing(inv.tool).to_string())?;
                let local_shell = LocalShell {
                    program: Some(program.to_string_lossy().into_owned()),
                    args: inv.args.iter().map(|a| local::expand_home(a)).collect(),
                    cwd: None,
                    env: Vec::new(),
                };
                self.terminals
                    .open_local(
                        term,
                        local_shell,
                        size,
                        switchyard_term::DEFAULT_SCROLLBACK,
                        self.events.clone(),
                        Vec::new(),
                    )
                    .map(|t| (t, description))
                    .map_err(|e| e.to_string())
            }
            Some(host) => {
                let target = self.ssh_target(host).await.map_err(|e| e.to_string())?;
                let line = inv.shell_line(&program_word(&c, inv.tool));
                let terminal = self.terminals.open_ssh(
                    SshTerminalSpec {
                        term,
                        host_id: host.clone(),
                        target,
                        size,
                        startup: Vec::new(),
                        env: Vec::new(),
                        command: Some(line),
                    },
                    self.ssh.clone(),
                    self.events.clone(),
                );
                Ok((terminal, description))
            }
        }
    }
}

/// The provider a connection was added as.
fn provider_of(c: &ContainerConnection) -> ct::clusters::ClusterProvider {
    if c.rancher.is_some() {
        return ct::clusters::ClusterProvider::Rancher;
    }
    c.provider
        .as_deref()
        .and_then(ct::clusters::ClusterProvider::from_key)
        .unwrap_or_default()
}

/// A kubectl failure that is about signing in, as [`ContainersError::SignIn`].
pub(super) fn classify(c: &ContainerConnection, e: ContainersError) -> ContainersError {
    match e {
        ContainersError::Failed {
            tool: Tool::Kubectl,
            message,
            code,
        } => match ct::clusters::sign_in_hint(&message, provider_of(c)) {
            Some(hint) => ContainersError::SignIn { message, hint },
            None => ContainersError::Failed {
                tool: Tool::Kubectl,
                code,
                message,
            },
        },
        e => e,
    }
}

/// Mark contexts whose exec plugin is not installed on this computer.
pub(super) fn mark_missing_plugins(list: &mut ct::kube::Contexts) {
    for c in &mut list.contexts {
        c.plugin_missing = c
            .exec
            .as_deref()
            .is_some_and(|p| local::find_named(p).is_none());
    }
}

/// Whether `action` may run on `c` now.
fn check_action(c: &ContainerConnection, action: &ct::Action, confirmed: bool) -> CtResult<()> {
    if c.read_only {
        return Err(ContainersError::ReadOnly);
    }
    let needs =
        action.always_confirm() || (action.is_destructive() && c.environment.is_production());
    if needs && !confirmed {
        return Err(ContainersError::NeedsConfirmation);
    }
    Ok(())
}

/// Stream a command's output lines from a Host.
async fn stream_remote(
    conn: &SshConn,
    line: &str,
    tool: Tool,
    pty: bool,
    deliver: impl FnMut(Vec<String>),
) -> CtResult<()> {
    let mut stream = conn.exec_stream(line, pty).await.map_err(io)?;
    let mut batch = local::Batcher::new(deliver);
    let (mut so, mut se) = (LineSplitter::new(), LineSplitter::new());
    let mut err_tail: Vec<String> = Vec::new();
    let mut status = None;
    loop {
        let wait = batch.deadline().unwrap_or(Duration::from_secs(3600));
        tokio::select! {
            chunk = stream.next() => match chunk {
                Some(ExecChunk::Stdout(b)) => {
                    let start = batch.pending().len();
                    so.feed(&b, batch.pending());
                    // With a PTY, errors arrive on standard output: keep its last lines.
                    if pty {
                        err_tail.extend(batch.pending()[start..].iter().cloned());
                    }
                }
                Some(ExecChunk::Stderr(b)) => {
                    let start = batch.pending().len();
                    se.feed(&b, batch.pending());
                    err_tail.extend(batch.pending()[start..].iter().cloned());
                }
                Some(ExecChunk::Exit(code)) => status = Some(code),
                None => break,
            },
            () = tokio::time::sleep(wait) => {}
        }
        let keep = err_tail.len().saturating_sub(if pty { 5 } else { 20 });
        err_tail.drain(..keep);
        batch.maybe_flush();
    }
    so.flush(batch.pending());
    se.flush(batch.pending());
    batch.flush();
    remote_result(tool, status, &err_tail.join("\n"), "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_store::{ContainerPlatform, EnvironmentLabel};

    fn conn(env: EnvironmentLabel) -> ContainerConnection {
        let mut c = ContainerConnection::new("k", ContainerPlatform::Kubernetes);
        c.environment = env;
        c
    }

    #[test]
    fn production_and_read_only_rules() {
        let stop = ct::Action::Stop {
            id: "1".into(),
            name: "web".into(),
        };
        let start = ct::Action::Start {
            id: "1".into(),
            name: "web".into(),
        };
        let scale = ct::Action::Scale {
            namespace: "n".into(),
            name: "web".into(),
            replicas: 1,
        };
        let dev = conn(EnvironmentLabel::Development);
        assert_eq!(check_action(&dev, &stop, false), Ok(()));
        assert_eq!(
            check_action(&dev, &scale, false),
            Err(ContainersError::NeedsConfirmation)
        );
        let prod = conn(EnvironmentLabel::Production);
        assert_eq!(
            check_action(&prod, &stop, false),
            Err(ContainersError::NeedsConfirmation)
        );
        assert_eq!(check_action(&prod, &stop, true), Ok(()));
        assert_eq!(check_action(&prod, &start, false), Ok(()));
        let mut ro = conn(EnvironmentLabel::Development);
        ro.read_only = true;
        assert_eq!(
            check_action(&ro, &start, true),
            Err(ContainersError::ReadOnly)
        );
    }

    #[test]
    fn targets_and_executables() {
        let mut k = conn(EnvironmentLabel::Development);
        k.context = Some("saved".into());
        k.kubeconfig = Some("~/.kube/x".into());
        k.executable = Some("/opt/bin/kubectl".into());
        let t = target_of(&k, None);
        assert_eq!(t.kube_context.as_deref(), Some("saved"));
        assert_eq!(t.kubeconfig.as_deref(), Some("~/.kube/x"));
        assert_eq!(
            target_of(&k, Some("other".into())).kube_context.as_deref(),
            Some("other")
        );
        assert_eq!(
            target_of(&k, Some(" ".into())).kube_context.as_deref(),
            Some("saved")
        );
        assert_eq!(program_word(&k, Tool::Kubectl), "/opt/bin/kubectl");
        assert_eq!(program_word(&k, Tool::Docker), "docker");
        let mut d = ContainerConnection::new("d", ContainerPlatform::Docker);
        d.context = Some("colima".into());
        let t = target_of(&d, None);
        assert_eq!(t.docker_context.as_deref(), Some("colima"));
        assert_eq!(t.kube_context, None);
    }

    #[test]
    fn host_exit_codes() {
        assert_eq!(remote_result(Tool::Docker, Some(0), "", ""), Ok(()));
        assert_eq!(
            remote_result(Tool::Docker, Some(127), "sh: docker: not found", ""),
            Err(ContainersError::Missing(Tool::Docker))
        );
        assert_eq!(
            remote_result(Tool::Kubectl, Some(1), "Forbidden", ""),
            Err(ContainersError::Failed {
                tool: Tool::Kubectl,
                code: Some(1),
                message: "Forbidden".into()
            })
        );
    }
}
