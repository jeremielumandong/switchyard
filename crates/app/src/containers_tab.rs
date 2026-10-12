//! Docker and Kubernetes explorer: one tab per container connection. Docker lists
//! containers (running or all), images, volumes and networks; Kubernetes lists pods,
//! deployments and services of a namespace (or all), per kubeconfig context, plus the
//! connection's port forwards.
//!
//! Every list, inspect, log stream and action runs the user's own `docker` / `kubectl`
//! on the core runtime ([`Command::ContainersList`] and friends), on this computer or on
//! the connection's Host; the tab only sends commands and draws events. Logs stream in
//! batches into a capped, filtered buffer drawn by a virtualized list. Destructive
//! actions ask first; on a Production connection the user types the object's name, and
//! the core checks that again. "Open shell" asks the workspace for a terminal tab running
//! `docker exec -it` / `kubectl exec -it`.

mod logs;
mod rows;
mod view;

use gpui_kit::component::input::{EditorState, InputEvent, InputState};
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, Subscription, UniformListScrollHandle, Window,
};
use switchyard_core::containers::kube::{Context as KubeContext, Contexts};
use switchyard_core::containers::{
    Action, ContainersError, DockerKind, ForwardKind, Inspect, KubeKind, Listing, LogOptions,
    LogSource, PortForward, Query, ShellTarget, docker, kube,
};
use switchyard_core::store::{ContainerConnection, ContainerPlatform};
use switchyard_core::{Command, ContainerForward, ForwardState, RequestId, RuntimeHandle};

use crate::app_state::next_id;
use crate::workspace::{Tab, Workspace};

use logs::LogBuffer;
use rows::{Row, Tone, View};

/// Tail choices of the log pane (`None`: everything).
const TAILS: [Option<u32>; 4] = [Some(100), Some(500), Some(2000), None];

/// What the tab asks the workspace for.
pub enum ContainersTabEvent {
    /// Open a terminal tab with a shell in this container.
    OpenShell(ShellTarget),
}

/// The bottom pane.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Detail {
    /// Nothing open.
    None,
    /// The selected object's logs.
    Logs,
    /// Inspect JSON, describe or YAML; the title.
    Text(String),
}

/// A form under the detail header.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Form {
    None,
    /// Scale a deployment.
    Scale {
        namespace: String,
        name: String,
    },
    /// Port-forward to a pod or service; declared ports as suggestions.
    Forward {
        namespace: String,
        kind: ForwardKind,
        name: String,
        ports: Vec<u16>,
    },
}

/// The selected object of the list.
enum Selected<'a> {
    Container(&'a docker::Container),
    Image(&'a docker::Image),
    Volume(&'a docker::Volume),
    Network(&'a docker::Network),
    Pod(&'a kube::Pod),
    Deployment(&'a kube::Deployment),
    Service(&'a kube::Service),
    Forward(&'a ContainerForward),
}

/// The explorer tab of one Docker or Kubernetes connection.
pub struct ContainersTab {
    core: RuntimeHandle,
    /// The connection.
    pub connection: ContainerConnection,
    view: View,
    /// Context chosen in the tab (else the connection's).
    context: Option<String>,
    contexts: Option<Contexts>,
    ctx_request: Option<RequestId>,
    namespaces: Vec<String>,
    ns_request: Option<RequestId>,
    /// Namespace listed; `None`: all namespaces.
    namespace: Option<String>,
    /// The user picked a namespace (the context's default no longer applies).
    namespace_chosen: bool,
    /// Docker: stopped containers too.
    show_all: bool,
    /// The last listing and the view it belongs to.
    listing: Option<(View, Listing)>,
    list_request: Option<(RequestId, View)>,
    list_error: Option<ContainersError>,
    /// Rows of the current view, filtered.
    rows: Vec<Row>,
    filter: Entity<InputState>,
    /// Key of the selected row.
    selected: Option<String>,
    detail: Detail,
    inspect_request: Option<RequestId>,
    text: Entity<EditorState>,
    text_error: Option<String>,
    logs: LogBuffer,
    log_request: Option<RequestId>,
    log_source: Option<LogSource>,
    log_ended: Option<Result<(), ContainersError>>,
    log_follow: bool,
    log_tail: Option<u32>,
    log_filter: Entity<InputState>,
    log_scroll: UniformListScrollHandle,
    /// Action waiting for confirmation.
    pending: Option<Action>,
    confirm_input: Entity<InputState>,
    action_request: Option<RequestId>,
    /// Outcome of the last action.
    status: Option<(bool, String)>,
    form: Form,
    scale_input: Entity<InputState>,
    local_port: Entity<InputState>,
    remote_port: Entity<InputState>,
    /// This connection's port forwards.
    forwards: Vec<ContainerForward>,
    list_scroll: UniformListScrollHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ContainersTabEvent> for ContainersTab {}

/// Whether the typed confirmation matches the object's name (surrounding spaces ignored).
pub fn typed_name_ok(typed: &str, action: &Action) -> bool {
    typed.trim() == action.subject()
}

/// Whether `action` needs the typed name on a connection labelled Production.
fn needs_typed_name(action: &Action, production: bool) -> bool {
    production && action.is_destructive()
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

impl ContainersTab {
    /// A tab on `connection`; lists right away.
    pub fn new(
        core: RuntimeHandle,
        connection: ContainerConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let filter = input("Filter", window, cx);
        let log_filter = input("Filter lines", window, cx);
        let confirm_input = input("Type the name to confirm", window, cx);
        let scale_input = input("Replicas", window, cx);
        let local_port = input("Local port (any)", window, cx);
        let remote_port = input("Remote port", window, cx);
        let text = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("text")
                .line_number(true)
                .indent_guides(false)
                .soft_wrap(false)
        });
        let subs = vec![
            cx.subscribe_in(&filter, window, |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.rebuild_rows(cx);
                    cx.notify();
                }
            }),
            cx.subscribe_in(&log_filter, window, |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    let f = this.log_filter.read(cx).value().to_string();
                    this.logs.set_filter(&f);
                    cx.notify();
                }
            }),
            cx.subscribe_in(
                &confirm_input,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter { .. } => this.confirm(window, cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &scale_input,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. }) {
                        this.submit_scale(window, cx);
                    }
                },
            ),
            cx.subscribe_in(&remote_port, window, |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.submit_forward(cx);
                }
            }),
        ];
        let views = View::of(connection.platform);
        let namespace = Some(
            connection
                .namespace
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "default".into()),
        );
        let namespace_chosen = connection.namespace.as_ref().is_some_and(|n| !n.is_empty());
        let mut t = Self {
            core,
            view: views[0],
            context: None,
            contexts: None,
            ctx_request: None,
            namespaces: Vec::new(),
            ns_request: None,
            namespace,
            namespace_chosen,
            show_all: true,
            listing: None,
            list_request: None,
            list_error: None,
            rows: Vec::new(),
            filter,
            selected: None,
            detail: Detail::None,
            inspect_request: None,
            text,
            text_error: None,
            logs: LogBuffer::default(),
            log_request: None,
            log_source: None,
            log_ended: None,
            log_follow: true,
            log_tail: Some(500),
            log_filter,
            log_scroll: UniformListScrollHandle::new(),
            pending: None,
            confirm_input,
            action_request: None,
            status: None,
            form: Form::None,
            scale_input,
            local_port,
            remote_port,
            forwards: Vec::new(),
            list_scroll: UniformListScrollHandle::new(),
            connection,
            _subs: subs,
        };
        if t.is_kube() {
            t.load_contexts();
            t.load_namespaces();
            t.core.send(Command::ContainersListForwards);
        }
        t.send_list();
        t
    }

    fn is_kube(&self) -> bool {
        self.connection.platform == ContainerPlatform::Kubernetes
    }

    fn production(&self) -> bool {
        self.connection.environment.is_production()
    }

    /// The context override sent with every command.
    fn context_arg(&self) -> Option<String> {
        self.context.clone()
    }

    /// The context in use: chosen, the connection's, or the kubeconfig's current one.
    fn active_context(&self) -> Option<&KubeContext> {
        let contexts = self.contexts.as_ref()?;
        let name = self
            .context
            .clone()
            .or_else(|| self.connection.context.clone().filter(|c| !c.is_empty()))
            .or_else(|| contexts.current.clone())?;
        contexts.contexts.iter().find(|c| c.name == name)
    }

    /// Whether `request` is one of this tab's.
    pub fn owns(&self, request: RequestId) -> bool {
        self.ctx_request == Some(request)
            || self.ns_request == Some(request)
            || self
                .list_request
                .as_ref()
                .is_some_and(|(r, _)| *r == request)
            || self.inspect_request == Some(request)
            || self.log_request == Some(request)
            || self.action_request == Some(request)
    }

    /// Stop the log stream and pending listings (the tab is closing).
    pub fn shutdown(&mut self) {
        for request in [
            self.log_request.take(),
            self.list_request.take().map(|(r, _)| r),
            self.inspect_request.take(),
            self.ctx_request.take(),
            self.ns_request.take(),
        ]
        .into_iter()
        .flatten()
        {
            self.core.send(Command::ContainersCancel { request });
        }
    }

    fn load_contexts(&mut self) {
        let request = next_id();
        self.ctx_request = Some(request);
        self.core.send(Command::ContainersList {
            request,
            connection: self.connection.id.clone(),
            context: None,
            query: Query::Contexts,
        });
    }

    fn load_namespaces(&mut self) {
        let request = next_id();
        self.ns_request = Some(request);
        self.core.send(Command::ContainersList {
            request,
            connection: self.connection.id.clone(),
            context: self.context_arg(),
            query: Query::Namespaces,
        });
    }

    fn send_list(&mut self) {
        if let Some((old, _)) = self.list_request.take() {
            self.core.send(Command::ContainersCancel { request: old });
        }
        let Some(query) = self.view.query(self.show_all, self.namespace.as_deref()) else {
            self.core.send(Command::ContainersListForwards);
            return;
        };
        let request = next_id();
        self.list_request = Some((request, self.view));
        self.core.send(Command::ContainersList {
            request,
            connection: self.connection.id.clone(),
            context: self.context_arg(),
            query,
        });
    }

    /// List again.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.send_list();
        cx.notify();
    }

    fn all_namespaces(&self) -> bool {
        self.is_kube() && self.namespace.is_none()
    }

    fn rebuild_rows(&mut self, cx: &mut Context<Self>) {
        let filter = self.filter.read(cx).value().to_string();
        self.rows = if self.view == View::Forwards {
            self.forward_rows(&filter)
        } else {
            match &self.listing {
                Some((v, listing)) if *v == self.view => rows::rows(
                    listing,
                    self.view,
                    self.all_namespaces(),
                    &filter,
                    now_secs(),
                ),
                _ => Vec::new(),
            }
        };
    }

    fn forward_rows(&self, filter: &str) -> Vec<Row> {
        let words: Vec<String> = filter.split_whitespace().map(str::to_lowercase).collect();
        self.forwards
            .iter()
            .map(|f| {
                let (local, state, tone) = match &f.state {
                    ForwardState::Starting => {
                        ("…".to_owned(), "Starting".to_owned(), Tone::Pending)
                    }
                    ForwardState::Running(port) => (
                        format!("127.0.0.1:{port}"),
                        "Running".to_owned(),
                        Tone::Good,
                    ),
                    ForwardState::Failed(m) => ("".to_owned(), format!("Failed: {m}"), Tone::Bad),
                };
                Row {
                    key: f.id.to_string(),
                    cells: vec![f.forward.label(), local, state],
                    tone,
                }
            })
            .filter(|r| {
                let hay = r.cells.join(" ").to_lowercase();
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .collect()
    }

    // ------------------------------------------------------------ choices

    fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        if self.view == view {
            return;
        }
        self.view = view;
        self.selected = None;
        self.form = Form::None;
        self.pending = None;
        self.close_detail();
        self.list_error = None;
        self.rebuild_rows(cx);
        self.send_list();
        cx.notify();
    }

    fn set_context(&mut self, name: String, cx: &mut Context<Self>) {
        self.context = Some(name);
        self.selected = None;
        self.close_detail();
        self.listing = None;
        self.rows.clear();
        if !self.namespace_chosen {
            self.namespace = Some(
                self.active_context()
                    .and_then(|c| c.namespace.clone())
                    .unwrap_or_else(|| "default".into()),
            );
        }
        self.load_namespaces();
        self.send_list();
        cx.notify();
    }

    fn set_namespace(&mut self, namespace: Option<String>, cx: &mut Context<Self>) {
        self.namespace_chosen = true;
        if self.namespace == namespace {
            return;
        }
        self.namespace = namespace;
        self.selected = None;
        self.close_detail();
        self.send_list();
        cx.notify();
    }

    fn toggle_show_all(&mut self, cx: &mut Context<Self>) {
        self.show_all = !self.show_all;
        self.send_list();
        cx.notify();
    }

    fn select(&mut self, key: String, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(key.as_str()) {
            return;
        }
        self.selected = Some(key);
        self.form = Form::None;
        self.pending = None;
        self.close_detail();
        cx.notify();
    }

    fn selected(&self) -> Option<Selected<'_>> {
        let key = self.selected.as_deref()?;
        if self.view == View::Forwards {
            return self
                .forwards
                .iter()
                .find(|f| f.id.to_string() == key)
                .map(Selected::Forward);
        }
        let (view, listing) = self.listing.as_ref()?;
        if *view != self.view {
            return None;
        }
        let nsname = |ns: &str, name: &str| key == format!("{ns}/{name}");
        match listing {
            Listing::Containers(l) => l.iter().find(|c| c.id == key).map(Selected::Container),
            Listing::Images(l) => l
                .iter()
                .find(|i| key == format!("{}|{}:{}", i.id, i.repository, i.tag))
                .map(Selected::Image),
            Listing::Volumes(l) => l.iter().find(|v| v.name == key).map(Selected::Volume),
            Listing::Networks(l) => l.iter().find(|n| n.id == key).map(Selected::Network),
            Listing::Pods(l) => l
                .iter()
                .find(|p| nsname(&p.namespace, &p.name))
                .map(Selected::Pod),
            Listing::Deployments(l) => l
                .iter()
                .find(|d| nsname(&d.namespace, &d.name))
                .map(Selected::Deployment),
            Listing::Services(l) => l
                .iter()
                .find(|s| nsname(&s.namespace, &s.name))
                .map(Selected::Service),
            Listing::Contexts(_) | Listing::Namespaces(_) => None,
        }
    }

    // ------------------------------------------------------------ detail

    fn close_detail(&mut self) {
        self.stop_logs();
        if let Some(request) = self.inspect_request.take() {
            self.core.send(Command::ContainersCancel { request });
        }
        self.detail = Detail::None;
        self.text_error = None;
    }

    fn stop_logs(&mut self) {
        if let Some(request) = self.log_request.take() {
            self.core.send(Command::ContainersCancel { request });
        }
    }

    fn inspect(
        &mut self,
        inspect: Inspect,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_detail();
        let request = next_id();
        self.inspect_request = Some(request);
        self.detail = Detail::Text(title);
        self.text.update(cx, |e, cx| e.set_value("", window, cx));
        self.core.send(Command::ContainersInspect {
            request,
            connection: self.connection.id.clone(),
            context: self.context_arg(),
            inspect,
        });
        cx.notify();
    }

    /// Inspect / describe / YAML of the selection (`yaml`: the YAML view).
    fn inspect_selected(&mut self, yaml: bool, window: &mut Window, cx: &mut Context<Self>) {
        let kube_inspect = |kind: KubeKind, namespace: &str, name: &str| {
            let (namespace, name) = (namespace.to_owned(), name.to_owned());
            if yaml {
                Inspect::Yaml {
                    kind,
                    namespace,
                    name,
                }
            } else {
                Inspect::Describe {
                    kind,
                    namespace,
                    name,
                }
            }
        };
        let (inspect, title) = match self.selected() {
            Some(Selected::Container(c)) => (
                Inspect::Docker {
                    kind: DockerKind::Container,
                    id: c.id.clone(),
                },
                format!("Inspect · {}", c.name),
            ),
            Some(Selected::Image(i)) => (
                Inspect::Docker {
                    kind: DockerKind::Image,
                    id: i.id.clone(),
                },
                format!("Inspect · {}", i.reference()),
            ),
            Some(Selected::Volume(v)) => (
                Inspect::Docker {
                    kind: DockerKind::Volume,
                    id: v.name.clone(),
                },
                format!("Inspect · {}", v.name),
            ),
            Some(Selected::Network(n)) => (
                Inspect::Docker {
                    kind: DockerKind::Network,
                    id: n.id.clone(),
                },
                format!("Inspect · {}", n.name),
            ),
            Some(Selected::Pod(p)) => (
                kube_inspect(KubeKind::Pod, &p.namespace, &p.name),
                p.name.clone(),
            ),
            Some(Selected::Deployment(d)) => (
                kube_inspect(KubeKind::Deployment, &d.namespace, &d.name),
                d.name.clone(),
            ),
            Some(Selected::Service(s)) => (
                kube_inspect(KubeKind::Service, &s.namespace, &s.name),
                s.name.clone(),
            ),
            Some(Selected::Forward(_)) | None => return,
        };
        let title = match (&inspect, yaml) {
            (Inspect::Docker { .. }, _) => title,
            (_, true) => format!("YAML · {title}"),
            (_, false) => format!("Describe · {title}"),
        };
        self.inspect(inspect, title, window, cx);
    }

    /// Start (or restart) the log stream of the selection.
    fn open_logs(&mut self, container: Option<String>, cx: &mut Context<Self>) {
        let source = match self.selected() {
            Some(Selected::Container(c)) => LogSource::Docker { id: c.id.clone() },
            Some(Selected::Pod(p)) => LogSource::Pod {
                namespace: p.namespace.clone(),
                pod: p.name.clone(),
                container: container.or_else(|| {
                    if p.containers.len() > 1 {
                        p.containers.first().cloned()
                    } else {
                        None
                    }
                }),
            },
            _ => return,
        };
        self.close_detail();
        self.start_logs(source, cx);
    }

    fn start_logs(&mut self, source: LogSource, cx: &mut Context<Self>) {
        self.stop_logs();
        self.logs.clear();
        self.log_ended = None;
        self.detail = Detail::Logs;
        let request = next_id();
        self.log_request = Some(request);
        self.log_source = Some(source.clone());
        self.core.send(Command::ContainersLogs {
            request,
            connection: self.connection.id.clone(),
            context: self.context_arg(),
            source,
            options: LogOptions {
                tail: self.log_tail,
                follow: self.log_follow,
                timestamps: false,
            },
        });
        cx.notify();
    }

    fn restart_logs(&mut self, cx: &mut Context<Self>) {
        if let Some(source) = self.log_source.clone() {
            self.start_logs(source, cx);
        }
    }

    fn set_log_tail(&mut self, tail: Option<u32>, cx: &mut Context<Self>) {
        self.log_tail = tail;
        self.restart_logs(cx);
    }

    fn toggle_follow(&mut self, cx: &mut Context<Self>) {
        self.log_follow = !self.log_follow;
        if self.log_follow || self.log_request.is_none() {
            self.restart_logs(cx);
        } else {
            // Stop reading; keep what arrived.
            self.stop_logs();
            cx.notify();
        }
    }

    fn set_log_container(&mut self, container: String, cx: &mut Context<Self>) {
        if let Some(LogSource::Pod { namespace, pod, .. }) = self.log_source.clone() {
            self.start_logs(
                LogSource::Pod {
                    namespace,
                    pod,
                    container: Some(container),
                },
                cx,
            );
        }
    }

    fn open_shell(&mut self, container: Option<String>, cx: &mut Context<Self>) {
        let shell = match self.selected() {
            Some(Selected::Container(c)) => ShellTarget::Docker {
                id: c.id.clone(),
                name: c.name.clone(),
            },
            Some(Selected::Pod(p)) => ShellTarget::Pod {
                namespace: p.namespace.clone(),
                pod: p.name.clone(),
                container: container.or_else(|| {
                    if p.containers.len() > 1 {
                        p.containers.first().cloned()
                    } else {
                        None
                    }
                }),
            },
            _ => return,
        };
        cx.emit(ContainersTabEvent::OpenShell(shell));
    }

    // ------------------------------------------------------------ actions

    /// Run `action`, asking first when it needs it.
    fn ask(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        if self.connection.read_only {
            self.status = Some((false, "This connection is read-only".into()));
            cx.notify();
            return;
        }
        let production = self.production();
        if !action.always_confirm() && !needs_typed_name(&action, production) {
            self.run_action(action, false, cx);
            return;
        }
        self.pending = Some(action);
        self.confirm_input
            .update(cx, |i, cx| i.set_value("", window, cx));
        if production {
            self.confirm_input.update(cx, |i, cx| i.focus(window, cx));
        }
        cx.notify();
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = self.pending.as_ref() else {
            return;
        };
        if needs_typed_name(action, self.production())
            && !typed_name_ok(&self.confirm_input.read(cx).value(), action)
        {
            return;
        }
        let Some(action) = self.pending.take() else {
            return;
        };
        self.confirm_input
            .update(cx, |i, cx| i.set_value("", window, cx));
        self.form = Form::None;
        self.run_action(action, true, cx);
    }

    fn run_action(&mut self, action: Action, confirmed: bool, cx: &mut Context<Self>) {
        let request = next_id();
        self.action_request = Some(request);
        self.status = None;
        if matches!(action, Action::Remove { .. } | Action::DeletePod { .. }) {
            self.close_detail();
        }
        self.core.send(Command::ContainersAction {
            request,
            connection: self.connection.id.clone(),
            context: self.context_arg(),
            action,
            confirmed,
        });
        cx.notify();
    }

    fn open_scale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Selected::Deployment(d)) = self.selected() else {
            return;
        };
        let (namespace, name, replicas) = (d.namespace.clone(), d.name.clone(), d.replicas);
        self.form = Form::Scale { namespace, name };
        self.scale_input.update(cx, |i, cx| {
            i.set_value(replicas.to_string(), window, cx);
            i.focus(window, cx);
        });
        cx.notify();
    }

    fn submit_scale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Form::Scale { namespace, name } = self.form.clone() else {
            return;
        };
        let Ok(replicas) = self.scale_input.read(cx).value().trim().parse::<u32>() else {
            self.status = Some((false, "Replicas must be a whole number".into()));
            cx.notify();
            return;
        };
        self.ask(
            Action::Scale {
                namespace,
                name,
                replicas,
            },
            window,
            cx,
        );
    }

    fn open_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (namespace, kind, name, ports): (String, ForwardKind, String, Vec<u16>) =
            match self.selected() {
                Some(Selected::Pod(p)) => (
                    p.namespace.clone(),
                    ForwardKind::Pod,
                    p.name.clone(),
                    p.ports.iter().map(|p| p.port).collect(),
                ),
                Some(Selected::Service(s)) => (
                    s.namespace.clone(),
                    ForwardKind::Service,
                    s.name.clone(),
                    s.ports.iter().map(|p| p.port).collect(),
                ),
                _ => return,
            };
        let first = ports.first().map(u16::to_string).unwrap_or_default();
        self.form = Form::Forward {
            namespace,
            kind,
            name,
            ports,
        };
        self.remote_port.update(cx, |i, cx| {
            i.set_value(first, window, cx);
            i.focus(window, cx);
        });
        self.local_port
            .update(cx, |i, cx| i.set_value("", window, cx));
        cx.notify();
    }

    fn set_remote_port(&mut self, port: u16, window: &mut Window, cx: &mut Context<Self>) {
        self.remote_port
            .update(cx, |i, cx| i.set_value(port.to_string(), window, cx));
        cx.notify();
    }

    fn submit_forward(&mut self, cx: &mut Context<Self>) {
        let Form::Forward {
            namespace,
            kind,
            name,
            ..
        } = self.form.clone()
        else {
            return;
        };
        let remote = self.remote_port.read(cx).value().trim().parse::<u16>();
        let local_text = self.local_port.read(cx).value().trim().to_owned();
        let local = if local_text.is_empty() {
            Ok(0)
        } else {
            local_text.parse::<u16>()
        };
        let (Ok(remote_port), Ok(local_port)) = (remote, local) else {
            self.status = Some((false, "Ports are numbers from 1 to 65535".into()));
            cx.notify();
            return;
        };
        if remote_port == 0 {
            self.status = Some((false, "Enter the remote port".into()));
            cx.notify();
            return;
        }
        self.core.send(Command::ContainersForward {
            id: next_id(),
            connection: self.connection.id.clone(),
            context: self.context_arg(),
            forward: PortForward {
                namespace,
                kind,
                name,
                local_port,
                remote_port,
            },
        });
        self.form = Form::None;
        self.status = Some((true, "Port forward started: see Port forwards".into()));
        cx.notify();
    }

    fn stop_forward(&mut self, id: u64, cx: &mut Context<Self>) {
        self.core.send(Command::ContainersStopForward { id });
        cx.notify();
    }

    // ------------------------------------------------------------ events

    /// A listing (or the contexts / namespaces) arrived.
    pub fn on_listed(
        &mut self,
        request: RequestId,
        result: Result<Listing, ContainersError>,
        cx: &mut Context<Self>,
    ) {
        if self.ctx_request == Some(request) {
            self.ctx_request = None;
            if let Ok(Listing::Contexts(c)) = result {
                self.contexts = Some(c);
                if !self.namespace_chosen {
                    let ns = self
                        .active_context()
                        .and_then(|c| c.namespace.clone())
                        .unwrap_or_else(|| "default".into());
                    if self.namespace.as_deref() != Some(ns.as_str()) {
                        self.namespace = Some(ns);
                        self.send_list();
                    }
                }
            }
        } else if self.ns_request == Some(request) {
            self.ns_request = None;
            if let Ok(Listing::Namespaces(list)) = result {
                self.namespaces = list.into_iter().map(|n| n.name).collect();
            }
        } else if let Some((r, view)) = self.list_request
            && r == request
        {
            self.list_request = None;
            match result {
                Ok(listing) => {
                    self.list_error = None;
                    self.listing = Some((view, listing));
                }
                Err(ContainersError::Cancelled) => {}
                Err(e) => {
                    self.list_error = Some(e);
                    self.listing = None;
                }
            }
            self.rebuild_rows(cx);
            if self.selected().is_none() && self.detail != Detail::Logs {
                self.selected = None;
            }
        }
        cx.notify();
    }

    /// Inspect / describe / YAML text arrived.
    pub fn on_inspected(
        &mut self,
        request: RequestId,
        result: Result<String, ContainersError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.inspect_request != Some(request) {
            return;
        }
        self.inspect_request = None;
        match result {
            Ok(text) => {
                self.text_error = None;
                self.text.update(cx, |e, cx| e.set_value(text, window, cx));
            }
            Err(e) => self.text_error = Some(e.to_string()),
        }
        cx.notify();
    }

    /// An action finished.
    pub fn on_action_done(
        &mut self,
        request: RequestId,
        result: Result<String, ContainersError>,
        cx: &mut Context<Self>,
    ) {
        if self.action_request != Some(request) {
            return;
        }
        self.action_request = None;
        self.status = Some(match result {
            Ok(m) => (true, m),
            Err(e) => (false, e.to_string()),
        });
        self.send_list();
        cx.notify();
    }

    /// Log lines arrived.
    pub fn on_log(&mut self, request: RequestId, lines: Vec<String>, cx: &mut Context<Self>) {
        if self.log_request != Some(request) {
            return;
        }
        // Stay at the bottom while following, unless the user scrolled up.
        let at_end = self.log_scroll.is_scrolled_to_end() != Some(false);
        self.logs.push(lines);
        if self.log_follow && at_end {
            self.log_scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    /// The log stream ended.
    pub fn on_log_ended(
        &mut self,
        request: RequestId,
        result: Result<(), ContainersError>,
        cx: &mut Context<Self>,
    ) {
        if self.log_request != Some(request) {
            return;
        }
        self.log_request = None;
        self.log_ended = Some(result);
        cx.notify();
    }

    /// Port forwards changed (all connections'; this tab keeps its own).
    pub fn on_forwards(&mut self, list: &[ContainerForward], cx: &mut Context<Self>) {
        self.forwards = list
            .iter()
            .filter(|f| f.connection == self.connection.id)
            .cloned()
            .collect();
        if self.view == View::Forwards {
            self.rebuild_rows(cx);
        }
        cx.notify();
    }

    /// The saved connection changed (renamed, relabelled, …).
    pub fn set_connection(&mut self, connection: ContainerConnection, cx: &mut Context<Self>) {
        self.connection = connection;
        cx.notify();
    }
}

impl Workspace {
    /// Open (or focus) the explorer tab of a Docker or Kubernetes connection.
    pub(crate) fn open_containers(
        &mut self,
        connection: ContainerConnection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self.tabs.iter().position(
            |t| matches!(t, Tab::Containers(c) if c.read(cx).connection.id == connection.id),
        ) {
            self.activate(ix, cx);
            return;
        }
        let core = self.core.clone();
        let tab = cx.new(|cx| ContainersTab::new(core, connection, window, cx));
        let sub = cx.subscribe_in(
            &tab,
            window,
            |this, tab, ev: &ContainersTabEvent, _, cx| match ev {
                ContainersTabEvent::OpenShell(shell) => {
                    let (connection, env) = {
                        let t = tab.read(cx);
                        (t.connection.id.clone(), t.connection.environment)
                    };
                    let context = tab.read(cx).context_arg();
                    this.open_container_shell(connection, env, context, shell.clone(), cx);
                }
            },
        );
        self._subs.push(sub);
        self.tabs.push(Tab::Containers(tab));
        self.activate(self.tabs.len() - 1, cx);
    }

    /// A terminal tab running a shell in a container.
    fn open_container_shell(
        &mut self,
        connection: switchyard_core::store::ProfileId,
        env: switchyard_core::store::EnvironmentLabel,
        context: Option<String>,
        shell: ShellTarget,
        cx: &mut Context<Self>,
    ) {
        let core = self.core.clone();
        let t = cx.new(|cx| {
            crate::terminal_tab::TerminalTab::new_container(
                core, env, connection, context, shell, cx,
            )
        });
        self.tabs.push(Tab::Terminal(t));
        self.activate(self.tabs.len() - 1, cx);
    }

    fn containers_tab(
        &self,
        request: RequestId,
        cx: &gpui_kit::App,
    ) -> Option<Entity<ContainersTab>> {
        self.tabs.iter().find_map(|t| match t {
            Tab::Containers(c) if c.read(cx).owns(request) => Some(c.clone()),
            _ => None,
        })
    }

    /// Route the `Containers*` events to their tab.
    pub(crate) fn on_containers_event(
        &mut self,
        ev: switchyard_core::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use switchyard_core::Event;
        match ev {
            Event::ContainersListed { request, result } => {
                if let Some(t) = self.containers_tab(request, cx) {
                    t.update(cx, |t, cx| t.on_listed(request, result, cx));
                }
            }
            Event::ContainersInspected { request, result } => {
                if let Some(t) = self.containers_tab(request, cx) {
                    t.update(cx, |t, cx| t.on_inspected(request, result, window, cx));
                }
            }
            Event::ContainersActionDone { request, result } => {
                if let Some(t) = self.containers_tab(request, cx) {
                    t.update(cx, |t, cx| t.on_action_done(request, result, cx));
                }
            }
            Event::ContainersLog { request, lines } => match self.containers_tab(request, cx) {
                Some(t) => t.update(cx, |t, cx| t.on_log(request, lines, cx)),
                // The tab closed or moved on: stop the stream.
                None => self.core.send(Command::ContainersCancel { request }),
            },
            Event::ContainersLogEnded { request, result } => {
                if let Some(t) = self.containers_tab(request, cx) {
                    t.update(cx, |t, cx| t.on_log_ended(request, result, cx));
                }
            }
            Event::ContainerForwards(list) => {
                for t in &self.tabs {
                    if let Tab::Containers(c) = t {
                        c.update(cx, |c, cx| c.on_forwards(&list, cx));
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_name_confirms_destructive_actions_on_production() {
        let rm = Action::Remove {
            id: "abc123".into(),
            name: "web".into(),
        };
        assert!(typed_name_ok(" web ", &rm));
        assert!(!typed_name_ok("abc123", &rm));
        assert!(needs_typed_name(&rm, true));
        assert!(!needs_typed_name(&rm, false));
        let start = Action::Start {
            id: "abc123".into(),
            name: "web".into(),
        };
        assert!(!needs_typed_name(&start, true));
        let scale = Action::Scale {
            namespace: "shop".into(),
            name: "api".into(),
            replicas: 0,
        };
        assert!(typed_name_ok("api", &scale));
        assert!(needs_typed_name(&scale, true));
    }
}
