//! Terraform / OpenTofu state viewer: open a state from a file, an S3 or Azure Blob
//! backend, or HCP Terraform; browse resources by module and type; read attributes with
//! sensitive values masked until revealed; outputs; and compare two states (two stored
//! versions, or another file). Read-only: nothing here writes state.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
    uniform_list,
};
use secrecy::SecretString;
use switchyard_core::cloud::display_ms;
use switchyard_core::cloud::tfstate::{self, MASK, StateDiff, TfState, TreeRow};
use switchyard_core::infra::{InfraOp, InfraUpdate, TfSource, TfVersion};
use switchyard_core::store::{CloudService, Profile, ProfileId};
use switchyard_core::{Command, RequestId, RuntimeHandle};

use super::{field, input_box, pick, plural, status_line, tag};
use crate::app_state::{Profiles, next_id};
use crate::appearance::{rpx, ts};
use crate::theme::{MONO, Palette, SANS, palette};
use crate::ui::{self, Kind};

const ROW_H: f32 = 24.;

/// Where the state is read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SrcKind {
    File,
    S3,
    Blob,
    Cloud,
}

/// What the main area shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Resources,
    Outputs,
    Compare,
}

/// Why a request was sent.
#[derive(Clone, Debug)]
enum Purpose {
    Load,
    Compare(String),
    Versions,
    SaveToken,
    HasToken,
}

/// A row of the comparison list.
#[derive(Clone, Debug)]
enum DiffRow {
    Section(String),
    Added(String),
    Removed(String),
    Instance(String),
    Attr {
        path: String,
        old: Option<String>,
        new: Option<String>,
        sensitive: bool,
    },
}

fn diff_rows(d: &StateDiff) -> Vec<DiffRow> {
    let mut out = Vec::new();
    if d.lineage_differs {
        out.push(DiffRow::Section(
            "Different lineages: these states are not versions of one another".into(),
        ));
    }
    if !d.added.is_empty() {
        out.push(DiffRow::Section(format!("Added ({})", d.added.len())));
        out.extend(d.added.iter().cloned().map(DiffRow::Added));
    }
    if !d.removed.is_empty() {
        out.push(DiffRow::Section(format!("Removed ({})", d.removed.len())));
        out.extend(d.removed.iter().cloned().map(DiffRow::Removed));
    }
    if !d.changed.is_empty() {
        out.push(DiffRow::Section(format!("Changed ({})", d.changed.len())));
        for c in &d.changed {
            out.push(DiffRow::Instance(c.address.clone()));
            out.extend(c.attrs.iter().map(|a| DiffRow::Attr {
                path: a.path.clone(),
                old: a.old.clone(),
                new: a.new.clone(),
                sensitive: a.sensitive,
            }));
        }
    }
    if !d.outputs.is_empty() {
        out.push(DiffRow::Section(format!("Outputs ({})", d.outputs.len())));
        out.extend(d.outputs.iter().map(|a| DiffRow::Attr {
            path: a.path.clone(),
            old: a.old.clone(),
            new: a.new.clone(),
            sensitive: a.sensitive,
        }));
    }
    out
}

/// The state viewer.
pub struct TfTab {
    core: RuntimeHandle,
    profiles: Profiles,
    kind: SrcKind,
    path: Entity<InputState>,
    conn: Option<ProfileId>,
    bucket: Entity<InputState>,
    key: Entity<InputState>,
    host: Entity<InputState>,
    org: Entity<InputState>,
    workspace: Entity<InputState>,
    token: Entity<InputState>,
    has_token: Option<bool>,
    search: Entity<InputState>,
    pending: HashMap<RequestId, Purpose>,
    source: Option<TfSource>,
    state: Option<Arc<TfState>>,
    label: String,
    error: Option<String>,
    versions: Vec<TfVersion>,
    other: Option<(String, Arc<TfState>)>,
    diff: Option<StateDiff>,
    diff_rows: Vec<DiffRow>,
    view: View,
    collapsed: HashSet<String>,
    rows: Vec<TreeRow>,
    selected: Option<tfstate::InstanceIx>,
    revealed: HashSet<String>,
    reveal_all: bool,
    status: Option<(bool, String)>,
    _subs: Vec<Subscription>,
}

impl TfTab {
    /// An empty viewer.
    pub fn new(
        core: RuntimeHandle,
        profiles: Profiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = |ph: &str, masked: bool, window: &mut Window, cx: &mut Context<Self>| {
            let ph = ph.to_owned();
            cx.new(|cx| InputState::new(window, cx).placeholder(ph).masked(masked))
        };
        let path = input("/path/to/terraform.tfstate", false, window, cx);
        let bucket = input("Bucket", false, window, cx);
        let key = input("env/prod/terraform.tfstate", false, window, cx);
        let host = input("app.terraform.io", false, window, cx);
        let org = input("Organization", false, window, cx);
        let workspace = input("Workspace", false, window, cx);
        let token = input("API token (saved in the keychain)", true, window, cx);
        let search = input("Search address, type or value", false, window, cx);
        let enter = |this: &mut Self, ev: &InputEvent, cx: &mut Context<Self>| {
            if let InputEvent::PressEnter { .. } = ev {
                this.load(cx);
            }
        };
        let subs = vec![
            cx.subscribe_in(&path, window, move |t, _, ev, _, cx| enter(t, ev, cx)),
            cx.subscribe_in(&key, window, move |t, _, ev, _, cx| enter(t, ev, cx)),
            cx.subscribe_in(&workspace, window, move |t, _, ev, _, cx| enter(t, ev, cx)),
            cx.subscribe_in(&search, window, |t, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    t.rebuild(cx);
                }
            }),
            cx.subscribe_in(&token, window, |t, _, ev: &InputEvent, _, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    t.save_token(cx);
                }
            }),
        ];
        Self {
            core,
            profiles,
            kind: SrcKind::File,
            path,
            conn: None,
            bucket,
            key,
            host,
            org,
            workspace,
            token,
            has_token: None,
            search,
            pending: HashMap::new(),
            source: None,
            state: None,
            label: String::new(),
            error: None,
            versions: Vec::new(),
            other: None,
            diff: None,
            diff_rows: Vec::new(),
            view: View::Resources,
            collapsed: HashSet::new(),
            rows: Vec::new(),
            selected: None,
            revealed: HashSet::new(),
            reveal_all: false,
            status: None,
            _subs: subs,
        }
    }

    /// Tab title.
    pub fn title(&self) -> String {
        if self.label.is_empty() {
            "Terraform state".into()
        } else {
            let short = self.label.rsplit('/').next().unwrap_or(&self.label);
            format!("TF · {short}")
        }
    }

    /// New profile list.
    pub fn set_profiles(&mut self, profiles: Profiles, cx: &mut Context<Self>) {
        self.profiles = profiles;
        cx.notify();
    }

    /// Whether `request` is this tab's.
    pub fn owns(&self, request: RequestId) -> bool {
        self.pending.contains_key(&request)
    }

    fn send(&mut self, op: InfraOp, purpose: Purpose) -> RequestId {
        let request = next_id();
        self.pending.insert(request, purpose);
        self.core.send(Command::Infra { request, op });
        request
    }

    fn value(input: &Entity<InputState>, cx: &Context<Self>) -> String {
        input.read(cx).value().trim().to_owned()
    }

    /// The source described by the form.
    fn form_source(&self, cx: &Context<Self>) -> Result<TfSource, String> {
        let need = |v: String, what: &str| {
            if v.is_empty() {
                Err(format!("Enter the {what}"))
            } else {
                Ok(v)
            }
        };
        Ok(match self.kind {
            SrcKind::File => {
                TfSource::File(need(Self::value(&self.path, cx), "state file path")?.into())
            }
            SrcKind::S3 => TfSource::S3 {
                conn: self.conn.clone().ok_or("Choose an S3 connection")?,
                bucket: need(Self::value(&self.bucket, cx), "bucket")?,
                key: need(Self::value(&self.key, cx), "key")?,
                version: None,
            },
            SrcKind::Blob => TfSource::Blob {
                conn: self
                    .conn
                    .clone()
                    .ok_or("Choose a Blob Storage connection")?,
                container: need(Self::value(&self.bucket, cx), "container")?,
                blob: need(Self::value(&self.key, cx), "blob name")?,
                version: None,
            },
            SrcKind::Cloud => TfSource::Cloud {
                host: Self::value(&self.host, cx),
                organization: need(Self::value(&self.org, cx), "organization")?,
                workspace: need(Self::value(&self.workspace, cx), "workspace")?,
                version: None,
            },
        })
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        match self.form_source(cx) {
            Ok(src) => self.load_source(src, cx),
            Err(e) => {
                self.status = Some((false, e));
                cx.notify();
            }
        }
    }

    fn load_source(&mut self, src: TfSource, cx: &mut Context<Self>) {
        self.pending.retain(|_, p| !matches!(p, Purpose::Load));
        self.error = None;
        self.status = None;
        self.source = Some(src.clone());
        self.send(InfraOp::TfLoad(src), Purpose::Load);
        cx.notify();
    }

    fn load_versions(&mut self, cx: &mut Context<Self>) {
        let src = match self
            .source
            .clone()
            .map(Ok)
            .unwrap_or_else(|| self.form_source(cx))
        {
            Ok(s) => s,
            Err(e) => {
                self.status = Some((false, e));
                cx.notify();
                return;
            }
        };
        self.send(InfraOp::TfVersions(src.at_version(None)), Purpose::Versions);
        self.status = Some((true, "Listing versions…".into()));
        cx.notify();
    }

    fn compare_with(&mut self, src: TfSource, cx: &mut Context<Self>) {
        let label = src.label();
        self.send(InfraOp::TfLoad(src), Purpose::Compare(label));
        self.status = Some((true, "Loading the state to compare…".into()));
        cx.notify();
    }

    fn compare_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Compare".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.pop() else { return };
            let _ = this.update(cx, |t, cx| t.compare_with(TfSource::File(path), cx));
        })
        .detach();
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open state".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.pop() else { return };
            let shown = path.to_string_lossy().into_owned();
            let _ = this.update_in(cx, |t, window, cx| {
                t.path.update(cx, |i, cx| i.set_value(shown, window, cx));
                t.load_source(TfSource::File(path), cx);
            });
        })
        .detach();
    }

    fn save_token(&mut self, cx: &mut Context<Self>) {
        let token = Self::value(&self.token, cx);
        if token.is_empty() {
            self.status = Some((false, "Paste an API token first".into()));
            cx.notify();
            return;
        }
        let host = Self::value(&self.host, cx);
        self.send(
            InfraOp::TfSaveToken {
                host,
                token: SecretString::from(token),
            },
            Purpose::SaveToken,
        );
        cx.notify();
    }

    fn set_kind(&mut self, kind: SrcKind, cx: &mut Context<Self>) {
        self.kind = kind;
        self.conn = None;
        if kind == SrcKind::Cloud {
            let host = Self::value(&self.host, cx);
            self.send(InfraOp::TfHasToken { host }, Purpose::HasToken);
        }
        cx.notify();
    }

    fn set_conn(&mut self, id: ProfileId, cx: &mut Context<Self>) {
        self.conn = Some(id);
        cx.notify();
    }

    fn set_view(&mut self, v: View, cx: &mut Context<Self>) {
        self.view = v;
        cx.notify();
    }

    /// Rebuild the tree rows from the state, search and collapsed groups.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let Some(s) = &self.state else {
            self.rows.clear();
            cx.notify();
            return;
        };
        let q = Self::value(&self.search, cx);
        let only = (!q.is_empty()).then(|| s.search(&q));
        self.rows = s.tree(only.as_deref(), &self.collapsed);
        cx.notify();
    }

    fn toggle(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.rebuild(cx);
    }

    fn select(&mut self, ix: tfstate::InstanceIx, cx: &mut Context<Self>) {
        self.selected = Some(ix);
        self.revealed.clear();
        cx.notify();
    }

    fn toggle_reveal(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.revealed.remove(&key) {
            self.revealed.insert(key);
        }
        cx.notify();
    }

    fn copy(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.status = Some((true, "Copied".into()));
        cx.notify();
    }

    /// An update for one of this tab's requests.
    pub fn on_update(
        &mut self,
        request: RequestId,
        update: InfraUpdate,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(purpose) = self.pending.remove(&request) else {
            return;
        };
        match (purpose, update) {
            (Purpose::Load, InfraUpdate::TfState { label, result }) => match result {
                Ok(state) => {
                    self.label = label;
                    let keep = self.selected.filter(|ix| state.instance(*ix).is_some());
                    self.selected = keep.or_else(|| state.instances().next().map(|(ix, _)| ix));
                    self.state = Some(state);
                    self.error = None;
                    self.diff = None;
                    self.other = None;
                    self.diff_rows.clear();
                    if self.view == View::Compare {
                        self.view = View::Resources;
                    }
                    self.rebuild(cx);
                }
                Err(e) => self.error = Some(e),
            },
            (Purpose::Compare(label), InfraUpdate::TfState { result, .. }) => match result {
                Ok(other) => {
                    if let Some(cur) = &self.state {
                        // Older first, so "added" means added since.
                        let (a, b, a_label, b_label) = if other.serial <= cur.serial {
                            (
                                other.clone(),
                                cur.clone(),
                                label.clone(),
                                self.label.clone(),
                            )
                        } else {
                            (
                                cur.clone(),
                                other.clone(),
                                self.label.clone(),
                                label.clone(),
                            )
                        };
                        let d = tfstate::diff(&a, &b);
                        self.status = Some((
                            true,
                            format!(
                                "{a_label} (serial {}) → {b_label} (serial {})",
                                a.serial, b.serial
                            ),
                        ));
                        self.diff_rows = diff_rows(&d);
                        self.diff = Some(d);
                        self.other = Some((label, other));
                        self.view = View::Compare;
                    }
                }
                Err(e) => self.status = Some((false, e)),
            },
            (Purpose::Versions, InfraUpdate::TfVersions(r)) => match r {
                Ok(v) => {
                    self.status = Some((
                        true,
                        if v.is_empty() {
                            "No stored versions (versioning is off, or this is a file)".into()
                        } else {
                            format!("{} stored", plural(v.len(), "version"))
                        },
                    ));
                    self.versions = v;
                }
                Err(e) => self.status = Some((false, e)),
            },
            (Purpose::SaveToken, InfraUpdate::Done(r)) => match r {
                Ok(()) => {
                    self.has_token = Some(true);
                    self.status = Some((true, "Token saved in the keychain".into()));
                }
                Err(e) => self.status = Some((false, e)),
            },
            (Purpose::HasToken, InfraUpdate::TfToken(has)) => self.has_token = Some(has),
            _ => {}
        }
        cx.notify();
    }

    // ---------------------------------------------------------------- render

    fn render_source(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let kind_btn = |k: SrcKind, label: &str| -> (SharedString, bool, ui::OnClick) {
            let this = this.clone();
            (
                label.to_owned().into(),
                self.kind == k,
                Box::new(move |_, _, cx| {
                    let _ = this.update(cx, |t, cx| t.set_kind(k, cx));
                }),
            )
        };
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_end()
            .gap(rpx(8.))
            .child(field(
                "Source",
                ui::segmented(
                    "tf-kind",
                    vec![
                        kind_btn(SrcKind::File, "File"),
                        kind_btn(SrcKind::S3, "S3"),
                        kind_btn(SrcKind::Blob, "Azure Blob"),
                        kind_btn(SrcKind::Cloud, "HCP Terraform"),
                    ],
                    22.,
                    p,
                ),
                p,
            ));
        let boxed = |e: &Entity<InputState>, w: f32| {
            input_box(p)
                .w(rpx(w))
                .child(Input::new(e).appearance(false).text_size(ts::BODY))
        };
        match self.kind {
            SrcKind::File => {
                row = row
                    .child(field("State file", boxed(&self.path, 360.), p))
                    .child(
                        ui::button("tf-browse", "Choose…", Kind::Secondary, p)
                            .on_click(cx.listener(|t, _, w, cx| t.browse(w, cx))),
                    );
            }
            SrcKind::S3 | SrcKind::Blob => {
                let service = if self.kind == SrcKind::S3 {
                    CloudService::S3
                } else {
                    CloudService::AzureBlob
                };
                let conns: Vec<(String, bool, ProfileId)> = self
                    .profiles
                    .all
                    .iter()
                    .filter_map(|pr| match pr {
                        Profile::Cloud(c) if c.service == service => Some((
                            c.name.clone(),
                            self.conn.as_ref() == Some(&c.id),
                            c.id.clone(),
                        )),
                        _ => None,
                    })
                    .collect();
                let current = conns.iter().find(|c| c.1).map_or_else(
                    || {
                        if conns.is_empty() {
                            format!("No {} connection saved", service.short_name())
                        } else {
                            "Choose a connection".to_owned()
                        }
                    },
                    |c| c.0.clone(),
                );
                let (a, b) = if self.kind == SrcKind::S3 {
                    ("Bucket", "Key")
                } else {
                    ("Container", "Blob")
                };
                row = row
                    .child(field(
                        "Connection",
                        pick("tf-conn", current, conns, this.clone(), Self::set_conn),
                        p,
                    ))
                    .child(field(a, boxed(&self.bucket, 160.), p))
                    .child(field(b, boxed(&self.key, 260.), p));
            }
            SrcKind::Cloud => {
                row = row
                    .child(field("Host", boxed(&self.host, 150.), p))
                    .child(field("Organization", boxed(&self.org, 140.), p))
                    .child(field("Workspace", boxed(&self.workspace, 160.), p))
                    .child(field(
                        match self.has_token {
                            Some(true) => "API token (saved; paste to replace)",
                            _ => "API token",
                        },
                        boxed(&self.token, 180.),
                        p,
                    ))
                    .child(
                        ui::button("tf-token", "Save token", Kind::Secondary, p)
                            .on_click(cx.listener(|t, _, _, cx| t.save_token(cx))),
                    );
            }
        }
        let loading = self.pending.values().any(|p| matches!(p, Purpose::Load));
        row = row.child(if loading {
            ui::shimmer(60., p)
        } else {
            ui::button("tf-open", "Open", Kind::Primary, p)
                .on_click(cx.listener(|t, _, _, cx| t.load(cx)))
                .into_any_element()
        });
        if self.kind != SrcKind::File {
            row = row.child(
                ui::button("tf-versions", "Versions", Kind::Secondary, p)
                    .on_click(cx.listener(|t, _, _, cx| t.load_versions(cx))),
            );
        }
        div()
            .flex_none()
            .px(rpx(12.))
            .py(rpx(8.))
            .border_b_1()
            .border_color(p.bd)
            .child(row)
            .into_any_element()
    }

    fn render_versions(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.versions.is_empty() {
            return None;
        }
        let base = self.source.clone();
        let mut list = div()
            .id("tf-version-list")
            .flex_none()
            .max_h(rpx(130.))
            .overflow_y_scroll()
            .border_b_1()
            .border_color(p.bd);
        for (i, v) in self.versions.iter().enumerate() {
            let when = v.modified_ms.map(display_ms).unwrap_or_default();
            let mut text = when;
            if let Some(s) = v.serial {
                text.push_str(&format!(" · serial {s}"));
            }
            if let Some(sz) = v.size {
                text.push_str(&format!(" · {}", ui::bytes(sz)));
            }
            let open_src = base.as_ref().map(|b| b.at_version(Some(v.id.clone())));
            let cmp_src = open_src.clone();
            list = list.child(
                div()
                    .h(rpx(24.))
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .px(rpx(12.))
                    .text_size(ts::BODY)
                    .child(div().flex_1().min_w_0().truncate().child(text))
                    .child(
                        div()
                            .font_family(MONO)
                            .text_size(ts::SMALL)
                            .text_color(p.fg3)
                            .child(v.id.chars().take(16).collect::<String>()),
                    )
                    .when(v.latest, |d| d.child(tag("CURRENT", p.dev)))
                    .when_some(open_src, |d, src| {
                        d.child(
                            ui::button(("tf-v-open", i), "Open", Kind::Ghost, p)
                                .h(rpx(20.))
                                .on_click(
                                    cx.listener(move |t, _, _, cx| t.load_source(src.clone(), cx)),
                                ),
                        )
                    })
                    .when_some(cmp_src.filter(|_| self.state.is_some()), |d, src| {
                        d.child(
                            ui::button(("tf-v-cmp", i), "Compare", Kind::Ghost, p)
                                .h(rpx(20.))
                                .on_click(
                                    cx.listener(move |t, _, _, cx| t.compare_with(src.clone(), cx)),
                                ),
                        )
                    }),
            );
        }
        Some(list.into_any_element())
    }

    fn render_info(&self, s: &TfState, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let (managed, data) = s.counts();
        let this = cx.entity().downgrade();
        let view_btn = |v: View, label: String| -> (SharedString, bool, ui::OnClick) {
            let this = this.clone();
            (
                label.into(),
                self.view == v,
                Box::new(move |_, _, cx| {
                    let _ = this.update(cx, |t, cx| t.set_view(v, cx));
                }),
            )
        };
        let mut views = vec![
            view_btn(View::Resources, format!("Resources {}", managed + data)),
            view_btn(View::Outputs, format!("Outputs {}", s.outputs.len())),
        ];
        if self.diff.is_some() {
            views.push(view_btn(View::Compare, "Compare".into()));
        }
        div()
            .flex_none()
            .h(rpx(34.))
            .flex()
            .items_center()
            .gap(rpx(10.))
            .px(rpx(12.))
            .border_b_1()
            .border_color(p.bd)
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(ts::BASE)
                    .child(self.label.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .font_family(MONO)
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child(format!(
                        "v{} · {} · serial {} · lineage {}",
                        s.version,
                        if s.terraform_version.is_empty() {
                            "unknown version".to_owned()
                        } else {
                            format!("Terraform {}", s.terraform_version)
                        },
                        s.serial,
                        s.lineage.chars().take(8).collect::<String>()
                    )),
            )
            .child(ui::segmented("tf-view", views, 22., p))
            .child(div().flex_1())
            .child(
                ui::checkbox("tf-reveal-all", self.reveal_all, "Show sensitive values", p)
                    .on_click(cx.listener(|t, _, _, cx| {
                        t.reveal_all = !t.reveal_all;
                        cx.notify();
                    })),
            )
            .child(
                ui::button("tf-cmp-file", "Compare with file…", Kind::Secondary, p)
                    .on_click(cx.listener(|t, _, w, cx| t.compare_file(w, cx))),
            )
            .into_any_element()
    }

    fn render_tree(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.rows.clone();
        let selected = self.selected;
        let p2 = *p;
        let list = uniform_list(
            "tf-tree",
            rows.len(),
            cx.processor(move |_this, range: std::ops::Range<usize>, _w, cx| {
                let p = &p2;
                range
                    .map(|i| match &rows[i] {
                        TreeRow::Group {
                            key,
                            label,
                            depth,
                            count,
                            open,
                        } => {
                            let key = key.clone();
                            div()
                                .id(("tf-g", i))
                                .h(rpx(ROW_H))
                                .flex()
                                .items_center()
                                .gap(rpx(6.))
                                .pl(rpx(8. + 14. * *depth as f32))
                                .pr(rpx(8.))
                                .cursor_pointer()
                                .hover(|s| s.bg(p.hover))
                                .on_click(cx.listener(move |t, _, _, cx| t.toggle(key.clone(), cx)))
                                .child(
                                    div()
                                        .w(rpx(10.))
                                        .text_size(ts::SMALL)
                                        .text_color(p.fg3)
                                        .child(if *open { "▾" } else { "▸" }),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(MONO)
                                        .text_size(ts::BODY)
                                        .font_weight(if *depth == 0 {
                                            FontWeight::SEMIBOLD
                                        } else {
                                            FontWeight::MEDIUM
                                        })
                                        .child(label.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(ts::SMALL)
                                        .text_color(p.fg3)
                                        .child(count.to_string()),
                                )
                                .into_any_element()
                        }
                        TreeRow::Instance { ix, label, depth } => {
                            let ix = *ix;
                            div()
                                .id(("tf-i", i))
                                .h(rpx(ROW_H))
                                .flex()
                                .items_center()
                                .pl(rpx(8. + 14. * *depth as f32))
                                .pr(rpx(8.))
                                .cursor_pointer()
                                .when(selected == Some(ix), |d| d.bg(p.sel))
                                .hover(|s| s.bg(p.hover))
                                .on_click(cx.listener(move |t, _, _, cx| t.select(ix, cx)))
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(MONO)
                                        .text_size(ts::BODY)
                                        .child(label.clone()),
                                )
                                .into_any_element()
                        }
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1();
        div()
            .w(rpx(320.))
            .flex_none()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(p.bd)
            .child(
                div().flex_none().p(rpx(8.)).child(
                    input_box(p).child(
                        Input::new(&self.search)
                            .appearance(false)
                            .text_size(ts::BODY),
                    ),
                ),
            )
            .child(if self.rows.is_empty() {
                div()
                    .p(rpx(12.))
                    .text_size(ts::BODY)
                    .text_color(p.fg3)
                    .child("Nothing matches.")
                    .into_any_element()
            } else {
                list.into_any_element()
            })
            .into_any_element()
    }

    fn render_detail(&self, s: &TfState, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some((res, inst)) = self.selected.and_then(|ix| s.instance(ix)) else {
            return div()
                .flex_1()
                .p(rpx(16.))
                .text_color(p.fg3)
                .child("Pick a resource on the left.")
                .into_any_element();
        };
        let ix = self.selected.unwrap_or_default();
        let address = inst.address(res);
        let rows = inst.rows();
        let reveal_all = self.reveal_all;
        let revealed: HashSet<String> = self
            .revealed
            .iter()
            .filter_map(|k| {
                k.strip_prefix(&format!("{}:{}:", ix.0, ix.1))
                    .map(str::to_owned)
            })
            .collect();
        let p2 = *p;
        let prefix = format!("{}:{}:", ix.0, ix.1);
        let attrs =
            uniform_list(
                "tf-attrs",
                rows.len(),
                cx.processor(move |_this, range: std::ops::Range<usize>, _w, cx| {
                    let p = &p2;
                    range
                        .map(|i| {
                            let r = &rows[i];
                            let shown = !r.sensitive || reveal_all || revealed.contains(&r.path);
                            let value = if shown {
                                r.value.clone()
                            } else {
                                MASK.to_owned()
                            };
                            let copy = r.value.clone();
                            let key = format!("{prefix}{}", r.path);
                            div()
                                .id(("tf-a", i))
                                .h(rpx(ROW_H))
                                .flex()
                                .items_center()
                                .gap(rpx(8.))
                                .px(rpx(12.))
                                .border_b_1()
                                .border_color(p.line)
                                .hover(|s| s.bg(p.hover))
                                .child(
                                    div()
                                        .w(rpx(240.))
                                        .flex_none()
                                        .truncate()
                                        .font_family(MONO)
                                        .text_size(ts::LABEL)
                                        .text_color(p.fg2)
                                        .child(r.path.clone()),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(MONO)
                                        .text_size(ts::LABEL)
                                        .text_color(if shown { p.fg } else { p.fg3 })
                                        .child(value),
                                )
                                .when(r.sensitive, |d| {
                                    d.child(
                                        div()
                                            .id(("tf-rv", i))
                                            .text_size(ts::SMALL)
                                            .text_color(p.acc)
                                            .cursor_pointer()
                                            .child(if shown { "Hide" } else { "Reveal" })
                                            .on_click(cx.listener(move |t, _, _, cx| {
                                                t.toggle_reveal(key.clone(), cx)
                                            })),
                                    )
                                })
                                .when(shown, |d| {
                                    d.child(
                                        div()
                                            .id(("tf-cp", i))
                                            .text_size(ts::SMALL)
                                            .text_color(p.fg3)
                                            .cursor_pointer()
                                            .hover(|s| s.text_color(p.acc))
                                            .child("Copy")
                                            .on_click(cx.listener(move |t, _, _, cx| {
                                                t.copy(copy.clone(), cx)
                                            })),
                                    )
                                })
                                .into_any_element()
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1();
        let deps = inst.dependencies.clone();
        let dependents = s.dependents(&res.address());
        let dep_line = |label: &str, items: &[String]| {
            div()
                .flex()
                .gap(rpx(8.))
                .text_size(ts::BODY)
                .child(
                    div()
                        .w(rpx(100.))
                        .flex_none()
                        .text_color(p.fg3)
                        .child(label.to_owned()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_family(MONO)
                        .text_size(ts::LABEL)
                        .child(if items.is_empty() {
                            "none".to_owned()
                        } else {
                            items.join(", ")
                        }),
                )
        };
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .px(rpx(12.))
                    .py(rpx(8.))
                    .flex()
                    .flex_col()
                    .gap(rpx(4.))
                    .border_b_1()
                    .border_color(p.bd)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(rpx(8.))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(MONO)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(ts::BASE)
                                    .child(address.clone()),
                            )
                            .when(inst.status.is_some(), |d| {
                                d.child(tag(
                                    inst.status.clone().unwrap_or_default().to_uppercase(),
                                    p.prod,
                                ))
                            })
                            .when(inst.deposed.is_some(), |d| d.child(tag("DEPOSED", p.stg)))
                            .child(div().flex_1())
                            .child(
                                ui::button("tf-copy-addr", "Copy address", Kind::Ghost, p)
                                    .h(rpx(22.))
                                    .on_click(
                                        cx.listener(move |t, _, _, cx| t.copy(address.clone(), cx)),
                                    ),
                            ),
                    )
                    .child(div().text_size(ts::SMALL).text_color(p.fg3).child(format!(
                        "{} · schema v{}{}",
                        res.provider_short(),
                        inst.schema_version,
                        inst.id().map(|i| format!(" · id {i}")).unwrap_or_default()
                    ))),
            )
            .child(attrs)
            .child(
                div()
                    .flex_none()
                    .px(rpx(12.))
                    .py(rpx(8.))
                    .flex()
                    .flex_col()
                    .gap(rpx(4.))
                    .border_t_1()
                    .border_color(p.bd)
                    .child(dep_line("Depends on", &deps))
                    .child(dep_line("Used by", &dependents)),
            )
            .into_any_element()
    }

    fn render_outputs(&self, s: &TfState, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        if s.outputs.is_empty() {
            return div()
                .p(rpx(16.))
                .text_color(p.fg3)
                .child("No outputs.")
                .into_any_element();
        }
        let mut list = div().flex().flex_col();
        for (i, o) in s.outputs.iter().enumerate() {
            let key = format!("out:{}", o.name);
            let shown = !o.sensitive || self.reveal_all || self.revealed.contains(&key);
            let value = tfstate::output_text(o);
            let copy = value.clone();
            list = list.child(
                div()
                    .flex()
                    .items_start()
                    .gap(rpx(10.))
                    .px(rpx(12.))
                    .py(rpx(6.))
                    .border_b_1()
                    .border_color(p.line)
                    .child(
                        div()
                            .w(rpx(200.))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .gap(rpx(2.))
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_size(ts::BODY)
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(o.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(ts::SMALL)
                                    .text_color(p.fg3)
                                    .child(o.type_label.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(MONO)
                            .text_size(ts::LABEL)
                            .text_color(if shown { p.fg } else { p.fg3 })
                            .child(if shown { value } else { MASK.to_owned() }),
                    )
                    .when(o.sensitive, |d| {
                        d.child(tag("SENSITIVE", p.stg)).child(
                            div()
                                .id(("tf-out-rv", i))
                                .text_size(ts::SMALL)
                                .text_color(p.acc)
                                .cursor_pointer()
                                .child(if shown { "Hide" } else { "Reveal" })
                                .on_click(
                                    cx.listener(move |t, _, _, cx| {
                                        t.toggle_reveal(key.clone(), cx)
                                    }),
                                ),
                        )
                    })
                    .when(shown, |d| {
                        d.child(
                            div()
                                .id(("tf-out-cp", i))
                                .text_size(ts::SMALL)
                                .text_color(p.fg3)
                                .cursor_pointer()
                                .child("Copy")
                                .on_click(cx.listener(move |t, _, _, cx| t.copy(copy.clone(), cx))),
                        )
                    }),
            );
        }
        div()
            .id("tf-outputs")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(list)
            .into_any_element()
    }

    fn render_compare(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let Some(d) = &self.diff else {
            return div().into_any_element();
        };
        let rows = self.diff_rows.clone();
        let reveal = self.reveal_all;
        let p2 = *p;
        let header = div()
            .flex_none()
            .px(rpx(12.))
            .py(rpx(6.))
            .text_size(ts::BODY)
            .font_weight(FontWeight::MEDIUM)
            .child(if d.is_empty() {
                "No differences.".to_owned()
            } else {
                d.summary()
            });
        let list = uniform_list(
            "tf-diff",
            rows.len(),
            cx.processor(move |_this, range: std::ops::Range<usize>, _w, _cx| {
                let p = &p2;
                range
                    .map(|i| {
                        let base = div()
                            .h(rpx(ROW_H))
                            .flex()
                            .items_center()
                            .gap(rpx(8.))
                            .px(rpx(12.))
                            .text_size(ts::LABEL)
                            .font_family(MONO);
                        match &rows[i] {
                            DiffRow::Section(t) => base
                                .font_family(SANS)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_size(ts::SMALL)
                                .text_color(p.fg3)
                                .child(t.to_uppercase()),
                            DiffRow::Added(a) => base
                                .text_color(p.dev)
                                .child("+")
                                .child(div().min_w_0().truncate().child(a.clone())),
                            DiffRow::Removed(a) => base
                                .text_color(p.prod)
                                .child("−")
                                .child(div().min_w_0().truncate().child(a.clone())),
                            DiffRow::Instance(a) => base
                                .text_color(p.stg)
                                .child("~")
                                .child(div().min_w_0().truncate().child(a.clone())),
                            DiffRow::Attr {
                                path,
                                old,
                                new,
                                sensitive,
                            } => {
                                let show = |v: &Option<String>| match v {
                                    None => "(absent)".to_owned(),
                                    Some(_) if *sensitive && !reveal => MASK.to_owned(),
                                    Some(v) => v.clone(),
                                };
                                base.pl(rpx(28.))
                                    .child(
                                        div()
                                            .w(rpx(220.))
                                            .flex_none()
                                            .truncate()
                                            .text_color(p.fg2)
                                            .child(path.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_color(p.prod)
                                            .child(show(old)),
                                    )
                                    .child(div().text_color(p.fg3).child("→"))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .text_color(p.dev)
                                            .child(show(new)),
                                    )
                            }
                        }
                        .into_any_element()
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(list)
            .into_any_element()
    }
}

impl Render for TfTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let mut root = div()
            .id("tf-tab")
            .size_full()
            .flex()
            .flex_col()
            .font_family(SANS)
            .bg(p.bg)
            .text_color(p.fg)
            .child(self.render_source(&p, cx))
            .children(self.render_versions(&p, cx))
            .children(
                self.status
                    .as_ref()
                    .map(|(ok, s)| status_line(*ok, s.clone(), &p).flex_none()),
            );
        if let Some(e) = &self.error {
            root = root.child(status_line(false, e.clone(), &p).flex_none());
        }
        let Some(state) = self.state.clone() else {
            return root.child(
                div()
                    .p(rpx(24.))
                    .flex()
                    .flex_col()
                    .gap(rpx(6.))
                    .text_size(ts::UI)
                    .text_color(p.fg2)
                    .child("Open a Terraform or OpenTofu state (format version 4).")
                    .child(
                        div()
                            .text_size(ts::BODY)
                            .text_color(p.fg3)
                            .child("Read-only: Switchyard never writes state. Sensitive values stay masked until you reveal them."),
                    ),
            );
        };
        root = root.child(self.render_info(&state, &p, cx));
        let body = match self.view {
            View::Resources => div()
                .flex_1()
                .min_h_0()
                .flex()
                .child(self.render_tree(&p, cx))
                .child(self.render_detail(&state, &p, cx))
                .into_any_element(),
            View::Outputs => self.render_outputs(&state, &p, cx),
            View::Compare => self.render_compare(&p, cx),
        };
        root.child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_rows_group_changes() {
        let a = tfstate::parse(include_bytes!(
            "../../../cloud/testdata/tfstate/serial-7.tfstate"
        ))
        .unwrap();
        let b = tfstate::parse(include_bytes!(
            "../../../cloud/testdata/tfstate/serial-9.tfstate"
        ))
        .unwrap();
        let rows = diff_rows(&tfstate::diff(&a, &b));
        let sections: Vec<String> = rows
            .iter()
            .filter_map(|r| match r {
                DiffRow::Section(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            sections,
            ["Added (1)", "Removed (1)", "Changed (2)", "Outputs (2)"]
        );
    }
}
