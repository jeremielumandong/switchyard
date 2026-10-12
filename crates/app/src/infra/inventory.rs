//! Cloud resource inventory: VMs, databases, load balancers, Kubernetes clusters and
//! storage from AWS (saved connections and CLI profiles), Azure (Resource Graph), Cloudflare
//! and Google Cloud, in one filterable table. Rows become SSH Hosts or database
//! connections (prefilled, never with credentials).

use std::collections::{BTreeMap, HashSet};

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, uniform_list,
};
use switchyard_core::cloud::inventory::{CloudResource, DbFlavor, Provider, ResourceKind};
use switchyard_core::db::Engine;
use switchyard_core::infra::{InfraOp, InfraUpdate, InventoryAccount};
use switchyard_core::store::{CloudProvider, DbConnection, Host, Profile, ProfileId};
use switchyard_core::{Command, RequestId, RuntimeHandle};

use super::{InfraEvent, field, input_box, pick, plural, status_line, tag};
use crate::app_state::{Profiles, next_id};
use crate::appearance::{rpx, ts};
use crate::theme::{MONO, Palette, SANS, palette};
use crate::ui::{self, Kind};

const ROW_H: f32 = 26.;

/// One account the user can include.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Source {
    Conn(ProfileId),
    AwsProfile(String),
    AzureCli,
    Gcp,
}

/// Table columns: title and width (`0` = flexible).
const COLUMNS: [(&str, f32); 8] = [
    ("Provider", 78.),
    ("Account", 130.),
    ("Region", 104.),
    ("Type", 150.),
    ("Name", 0.),
    ("State", 84.),
    ("IPs", 170.),
    ("Tags", 200.),
];

/// The database engine for a flavor.
pub(crate) fn engine_of(f: DbFlavor) -> Engine {
    match f {
        DbFlavor::Postgres => Engine::Postgres,
        DbFlavor::MySql => Engine::MySql,
        DbFlavor::SqlServer => Engine::SqlServer,
        DbFlavor::Oracle => Engine::Oracle,
        DbFlavor::Redis => Engine::Redis,
        DbFlavor::MongoDb => Engine::MongoDb,
        DbFlavor::D1 => Engine::D1,
    }
}

/// A new SSH Host for a VM, or `None` when it has no address.
pub(crate) fn host_for(r: &CloudResource) -> Option<Host> {
    if r.kind != Some(ResourceKind::Vm) {
        return None;
    }
    let address = r.address()?.to_owned();
    let mut h = Host::new(r.name.clone(), address, r.user.clone().unwrap_or_default());
    if let Some(p) = r.port.filter(|p| *p == 22) {
        h.port = p;
    }
    h.tags = vec![r.provider.map_or("cloud", |p| p.label()).to_lowercase()];
    Some(h)
}

/// Whether `r` can be marked for "Import as Hosts": VMs only.
fn importable(r: &CloudResource) -> bool {
    r.kind == Some(ResourceKind::Vm)
}

/// A new database connection for a database resource (no credentials).
pub(crate) fn db_for(r: &CloudResource) -> Option<DbConnection> {
    let engine = engine_of(r.db?);
    let mut d = DbConnection::new(r.name.clone(), engine);
    if let Some(h) = &r.host {
        d.server = h.clone();
    }
    if let Some(p) = r.port {
        d.port = p;
    }
    if let Some(u) = &r.user {
        d.user = u.clone();
    }
    Some(d)
}

fn row_key(r: &CloudResource) -> String {
    format!("{}|{}|{}", r.account, r.region, r.id)
}

/// The inventory tab.
pub struct InventoryTab {
    core: RuntimeHandle,
    profiles: Profiles,
    aws_profiles: Vec<String>,
    chosen: HashSet<Source>,
    regions: Entity<InputState>,
    gcp_project: Entity<InputState>,
    filter: Entity<InputState>,
    kind: Option<ResourceKind>,
    provider: Option<Provider>,
    running_only: bool,
    pending: HashSet<RequestId>,
    current: Option<RequestId>,
    /// Results per account label, in arrival order.
    accounts: BTreeMap<String, Vec<CloudResource>>,
    errors: Vec<(String, String)>,
    shown: Vec<CloudResource>,
    focused: Option<String>,
    marked: HashSet<String>,
    show_errors: bool,
    status: Option<(bool, String)>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<InfraEvent> for InventoryTab {}

impl InventoryTab {
    /// A new tab; asks for the AWS CLI's profiles.
    pub fn new(
        core: RuntimeHandle,
        profiles: Profiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let regions = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("AWS regions: default, us-east-1,eu-west-1 or all")
        });
        let gcp_project =
            cx.new(|cx| InputState::new(window, cx).placeholder("gcloud default project"));
        let filter = cx
            .new(|cx| InputState::new(window, cx).placeholder("Filter name, id, IP, tag, region…"));
        let subs = vec![
            cx.subscribe_in(&filter, window, |t, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    t.refilter(cx);
                }
            }),
        ];
        let mut tab = Self {
            core,
            profiles,
            aws_profiles: Vec::new(),
            chosen: HashSet::new(),
            regions,
            gcp_project,
            filter,
            kind: None,
            provider: None,
            running_only: false,
            pending: HashSet::new(),
            current: None,
            accounts: BTreeMap::new(),
            errors: Vec::new(),
            shown: Vec::new(),
            focused: None,
            marked: HashSet::new(),
            show_errors: false,
            status: None,
            _subs: subs,
        };
        // Saved connections that can list resources start chosen.
        for c in tab.cloud_conns() {
            tab.chosen.insert(Source::Conn(c.0));
        }
        tab.send(InfraOp::AwsProfiles);
        tab
    }

    /// Whether `request` is this tab's.
    pub fn owns(&self, request: RequestId) -> bool {
        self.pending.contains(&request)
    }

    /// New profile list.
    pub fn set_profiles(&mut self, profiles: Profiles, cx: &mut Context<Self>) {
        self.profiles = profiles;
        cx.notify();
    }

    fn send(&mut self, op: InfraOp) -> RequestId {
        let request = next_id();
        self.pending.insert(request);
        self.core.send(Command::Infra { request, op });
        request
    }

    /// Saved cloud connections, one per sign-in: (id, name, provider).
    fn cloud_conns(&self) -> Vec<(ProfileId, String, CloudProvider)> {
        let mut seen = HashSet::new();
        self.profiles
            .all
            .iter()
            .filter_map(|p| match p {
                Profile::Cloud(c) => {
                    let provider = c.service.provider();
                    // Custom S3 endpoints (MinIO…) have no EC2 behind them.
                    if provider == CloudProvider::Aws && !c.endpoint.trim().is_empty() {
                        return None;
                    }
                    let key = format!(
                        "{provider:?}|{:?}|{}|{}",
                        c.auth,
                        c.user,
                        c.tenant.clone().unwrap_or_default()
                    );
                    seen.insert(key)
                        .then(|| (c.id.clone(), c.name.clone(), provider))
                }
                _ => None,
            })
            .collect()
    }

    fn toggle_source(&mut self, s: Source, cx: &mut Context<Self>) {
        if !self.chosen.remove(&s) {
            self.chosen.insert(s);
        }
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let regions: Vec<String> = self
            .regions
            .read(cx)
            .value()
            .split([',', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "default")
            .map(str::to_owned)
            .collect();
        let project = self.gcp_project.read(cx).value().trim().to_owned();
        let mut accounts = Vec::new();
        for s in &self.chosen {
            accounts.push(match s {
                Source::Conn(id) => InventoryAccount::Connection {
                    conn: id.clone(),
                    regions: regions.clone(),
                },
                Source::AwsProfile(p) => InventoryAccount::AwsProfile {
                    profile: p.clone(),
                    regions: regions.clone(),
                },
                Source::AzureCli => InventoryAccount::AzureCli,
                Source::Gcp => InventoryAccount::Gcp {
                    project: project.clone(),
                },
            });
        }
        if accounts.is_empty() {
            self.status = Some((false, "Choose at least one account".into()));
            cx.notify();
            return;
        }
        // A new refresh replaces the old one.
        if let Some(old) = self.current.take() {
            self.pending.remove(&old);
        }
        self.accounts.clear();
        self.errors.clear();
        self.marked.clear();
        self.status = Some((
            true,
            format!("Listing {}…", plural(accounts.len(), "account")),
        ));
        self.current = Some(self.send(InfraOp::Inventory(accounts)));
        self.refilter(cx);
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let q = self.filter.read(cx).value().trim().to_owned();
        self.shown = self
            .accounts
            .values()
            .flatten()
            .filter(|r| self.kind.is_none() || r.kind == self.kind)
            .filter(|r| self.provider.is_none() || r.provider == self.provider)
            .filter(|r| !self.running_only || r.is_up())
            .filter(|r| r.matches(&q))
            .cloned()
            .collect();
        cx.notify();
    }

    fn set_kind(&mut self, k: Option<ResourceKind>, cx: &mut Context<Self>) {
        self.kind = k;
        self.refilter(cx);
    }

    fn set_provider(&mut self, p: Option<Provider>, cx: &mut Context<Self>) {
        self.provider = p;
        self.refilter(cx);
    }

    /// An update for one of this tab's requests.
    pub fn on_update(&mut self, request: RequestId, update: InfraUpdate, cx: &mut Context<Self>) {
        match update {
            InfraUpdate::AwsProfiles(names) => {
                self.pending.remove(&request);
                self.aws_profiles = names;
            }
            InfraUpdate::Inventory {
                account,
                items,
                errors,
            } if self.current == Some(request) => {
                for e in errors {
                    self.errors.push((account.clone(), e));
                }
                self.accounts.entry(account).or_default().extend(items);
                self.refilter(cx);
            }
            InfraUpdate::Done(_) if self.current == Some(request) => {
                self.pending.remove(&request);
                self.current = None;
                let n: usize = self.accounts.values().map(Vec::len).sum();
                self.status = Some((
                    self.errors.is_empty(),
                    format!(
                        "{} in {}{}",
                        plural(n, "resource"),
                        plural(self.accounts.len(), "account"),
                        if self.errors.is_empty() {
                            String::new()
                        } else {
                            format!(" · {}", plural(self.errors.len(), "error"))
                        }
                    ),
                ));
            }
            _ => {}
        }
        cx.notify();
    }

    fn focused(&self) -> Option<&CloudResource> {
        let k = self.focused.as_ref()?;
        self.shown.iter().find(|r| &row_key(r) == k)
    }

    fn focus(&mut self, key: String, cx: &mut Context<Self>) {
        self.focused = Some(key);
        cx.notify();
    }

    fn toggle_mark(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.marked.remove(&key) {
            self.marked.insert(key);
        }
        cx.notify();
    }

    fn copy(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        cx.emit(InfraEvent::Toast("Copied".into()));
    }

    /// Save every marked VM as a Host (no credentials; the user picks the auth later).
    fn import_marked(&mut self, cx: &mut Context<Self>) {
        let hosts: Vec<Host> = self
            .shown
            .iter()
            .filter(|r| importable(r) && self.marked.contains(&row_key(r)))
            .filter_map(host_for)
            .collect();
        if hosts.is_empty() {
            cx.emit(InfraEvent::Toast("No marked VM has an address".into()));
            return;
        }
        let n = hosts.len();
        for h in hosts {
            self.core.send(Command::SaveProfile {
                request: next_id(),
                profile: Profile::Host(h),
                secret: None,
            });
        }
        self.marked.clear();
        cx.emit(InfraEvent::Toast(format!(
            "Imported {} · set each one's sign-in before connecting",
            plural(n, "Host")
        )));
        cx.notify();
    }

    // ---------------------------------------------------------------- render

    fn render_accounts(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div().flex().flex_wrap().gap(rpx(12.)).items_center();
        let mut sources: Vec<(Source, String)> = self
            .cloud_conns()
            .into_iter()
            .map(|(id, name, pr)| (Source::Conn(id), format!("{name} ({})", pr.display_name())))
            .collect();
        sources.extend(
            self.aws_profiles
                .iter()
                .map(|n| (Source::AwsProfile(n.clone()), format!("AWS profile {n}"))),
        );
        sources.push((Source::AzureCli, "Azure CLI (az login)".into()));
        sources.push((Source::Gcp, "Google Cloud (gcloud)".into()));
        for (i, (s, label)) in sources.into_iter().enumerate() {
            let checked = self.chosen.contains(&s);
            list = list.child(
                ui::checkbox(("inv-acc", i), checked, label, p)
                    .on_click(cx.listener(move |t, _, _, cx| t.toggle_source(s.clone(), cx))),
            );
        }
        let loading = self.current.is_some();
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(rpx(8.))
            .px(rpx(12.))
            .py(rpx(8.))
            .border_b_1()
            .border_color(p.bd)
            .child(field("Accounts", list, p))
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(rpx(8.))
                    .child(field(
                        "AWS regions",
                        input_box(p).w(rpx(300.)).child(
                            Input::new(&self.regions)
                                .appearance(false)
                                .text_size(ts::BODY),
                        ),
                        p,
                    ))
                    .when(self.chosen.contains(&Source::Gcp), |d| {
                        d.child(field(
                            "Google Cloud project",
                            input_box(p).w(rpx(180.)).child(
                                Input::new(&self.gcp_project)
                                    .appearance(false)
                                    .text_size(ts::BODY),
                            ),
                            p,
                        ))
                    })
                    .child(if loading {
                        ui::shimmer(70., p)
                    } else {
                        ui::button("inv-refresh", "Refresh", Kind::Primary, p)
                            .on_click(cx.listener(|t, _, _, cx| t.refresh(cx)))
                            .into_any_element()
                    }),
            )
            .into_any_element()
    }

    fn render_filters(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let mut kinds = vec![("All types".to_owned(), self.kind.is_none(), None)];
        kinds.extend(
            ResourceKind::ALL
                .iter()
                .map(|k| (k.label().to_owned(), self.kind == Some(*k), Some(*k))),
        );
        let provs = [
            Provider::Aws,
            Provider::Azure,
            Provider::Cloudflare,
            Provider::Gcp,
        ];
        let mut providers = vec![("All providers".to_owned(), self.provider.is_none(), None)];
        providers.extend(
            provs
                .iter()
                .map(|v| (v.label().to_owned(), self.provider == Some(*v), Some(*v))),
        );
        // Only shown VMs count: a storage account or database is never imported as a Host.
        let marked = self
            .shown
            .iter()
            .filter(|r| importable(r) && self.marked.contains(&row_key(r)))
            .count();
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(8.))
            .px(rpx(12.))
            .py(rpx(6.))
            .border_b_1()
            .border_color(p.bd)
            .child(
                input_box(p).w(rpx(280.)).child(
                    Input::new(&self.filter)
                        .appearance(false)
                        .text_size(ts::BODY),
                ),
            )
            .child(pick(
                "inv-kind",
                self.kind.map_or("All types", |k| k.label()),
                kinds,
                this.clone(),
                Self::set_kind,
            ))
            .child(pick(
                "inv-prov",
                self.provider.map_or("All providers", |k| k.label()),
                providers,
                this,
                Self::set_provider,
            ))
            .child(
                ui::checkbox("inv-up", self.running_only, "Running only", p).on_click(cx.listener(
                    |t, _, _, cx| {
                        t.running_only = !t.running_only;
                        t.refilter(cx);
                    },
                )),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child(plural(self.shown.len(), "row")),
            )
            .when(marked > 0, |d| {
                d.child(
                    ui::button(
                        "inv-import",
                        format!("Import {} as Hosts", plural(marked, "VM")),
                        Kind::Secondary,
                        p,
                    )
                    .on_click(cx.listener(|t, _, _, cx| t.import_marked(cx))),
                )
            })
            .into_any_element()
    }

    fn render_table(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let cell = |w: f32| {
            let d = div().px(rpx(6.)).min_w_0().truncate();
            if w == 0. {
                d.flex_1()
            } else {
                d.w(rpx(w)).flex_none()
            }
        };
        let mut header = div()
            .flex_none()
            .h(rpx(24.))
            .flex()
            .items_center()
            .pl(rpx(30.))
            .pr(rpx(8.))
            .bg(p.panel)
            .border_b_1()
            .border_color(p.bd)
            .text_size(ts::SMALL)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(p.fg3);
        for (t, w) in COLUMNS {
            header = header.child(cell(w).child(t.to_uppercase()));
        }
        let rows = self.shown.clone();
        let focused = self.focused.clone();
        let marked = self.marked.clone();
        let p2 = *p;
        let list = uniform_list(
            "inv-rows",
            rows.len(),
            cx.processor(move |_t, range: std::ops::Range<usize>, _w, cx| {
                let p = &p2;
                range
                    .map(|i| {
                        let r = &rows[i];
                        let key = row_key(r);
                        let k2 = key.clone();
                        let k3 = key.clone();
                        let state_color = if r.is_up() {
                            p.dev
                        } else if r.state.is_empty() {
                            p.fg3
                        } else {
                            p.stg
                        };
                        let values = [
                            r.provider.map_or("", |v| v.label()).to_owned(),
                            r.account.clone(),
                            r.region.clone(),
                            r.rtype.clone(),
                            r.name.clone(),
                            r.state.clone(),
                            r.ips_text(),
                            r.tags_text(),
                        ];
                        let mut row = div()
                            .id(("inv-r", i))
                            .w_full()
                            .h(rpx(ROW_H))
                            .flex()
                            .items_center()
                            .pl(rpx(8.))
                            .pr(rpx(8.))
                            .border_b_1()
                            .border_color(p.line)
                            .text_size(ts::BODY)
                            .cursor_pointer()
                            .when(focused.as_deref() == Some(key.as_str()), |d| d.bg(p.sel))
                            .hover(|s| s.bg(p.hover))
                            .on_click(cx.listener(move |t, _, _, cx| t.focus(k2.clone(), cx)))
                            .child(
                                div()
                                    .id(("inv-m", i))
                                    .w(rpx(22.))
                                    .flex_none()
                                    // Only VMs import as Hosts; other rows keep the column empty.
                                    .when(importable(r), |d| {
                                        d.child(ui::checkbox(
                                            ("inv-mc", i),
                                            marked.contains(&key),
                                            "",
                                            p,
                                        ))
                                        .on_click(
                                            cx.listener(move |t, _, _, cx| {
                                                cx.stop_propagation();
                                                t.toggle_mark(k3.clone(), cx)
                                            }),
                                        )
                                    }),
                            );
                        for (c, ((_, w), v)) in COLUMNS.iter().zip(values).enumerate() {
                            let mut d = cell(*w).child(v);
                            if c == 4 {
                                d = d.font_weight(FontWeight::MEDIUM);
                            } else if c == 5 {
                                d = d.text_color(state_color);
                            } else if c == 6 || c == 7 {
                                d = d.font_family(MONO).text_size(ts::LABEL).text_color(p.fg2);
                            } else {
                                d = d.text_color(p.fg2);
                            }
                            row = row.child(d);
                        }
                        row.into_any_element()
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
            .child(if self.shown.is_empty() {
                div()
                    .p(rpx(16.))
                    .text_size(ts::BODY)
                    .text_color(p.fg3)
                    .child(if self.accounts.is_empty() {
                        "Choose accounts and press Refresh. Listing is read-only: Describe, List and Resource Graph calls only."
                    } else {
                        "Nothing matches the filter."
                    })
                    .into_any_element()
            } else {
                list.into_any_element()
            })
            .into_any_element()
    }

    fn render_actions(&self, r: &CloudResource, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let id = r.id.clone();
        let addr = r.address().map(str::to_owned);
        let host = host_for(r);
        let db = db_for(r);
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(8.))
            .px(rpx(12.))
            .py(rpx(6.))
            .border_t_1()
            .border_color(p.bd)
            .bg(p.panel)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .gap(rpx(6.))
                            .items_center()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(ts::BASE)
                                    .truncate()
                                    .child(r.name.clone()),
                            )
                            .when_some(r.kind, |d, k| {
                                d.child(tag(k.label().to_uppercase(), p.acc))
                            }),
                    )
                    .child(
                        div()
                            .font_family(MONO)
                            .text_size(ts::SMALL)
                            .text_color(p.fg3)
                            .truncate()
                            .child(r.id.clone()),
                    ),
            )
            .when_some(host, |d, h| {
                d.child(
                    ui::button("inv-add-host", "Add as SSH Host", Kind::Secondary, p).on_click(
                        cx.listener(move |_, _, _, cx| {
                            cx.emit(InfraEvent::AddHost(Box::new(h.clone())))
                        }),
                    ),
                )
            })
            .when_some(db, |d, c| {
                d.child(
                    ui::button("inv-add-db", "Add as connection", Kind::Secondary, p).on_click(
                        cx.listener(move |_, _, _, cx| {
                            cx.emit(InfraEvent::AddDb(Box::new(c.clone())))
                        }),
                    ),
                )
            })
            .when_some(addr, |d, a| {
                d.child(
                    ui::button("inv-copy-addr", "Copy address", Kind::Ghost, p)
                        .on_click(cx.listener(move |t, _, _, cx| t.copy(a.clone(), cx))),
                )
            })
            .child(
                ui::button("inv-copy-id", "Copy ID", Kind::Ghost, p)
                    .on_click(cx.listener(move |t, _, _, cx| t.copy(id.clone(), cx))),
            )
            .into_any_element()
    }

    fn render_errors(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.errors.is_empty() {
            return None;
        }
        let mut d = div()
            .flex_none()
            .px(rpx(12.))
            .py(rpx(4.))
            .flex()
            .flex_col()
            .gap(rpx(2.))
            .border_b_1()
            .border_color(p.bd)
            .child(
                div()
                    .id("inv-errs")
                    .cursor_pointer()
                    .text_size(ts::BODY)
                    .text_color(p.stg)
                    .child(format!(
                        "{} {} — some resources may be missing",
                        if self.show_errors { "▾" } else { "▸" },
                        plural(self.errors.len(), "error")
                    ))
                    .on_click(cx.listener(|t, _, _, cx| {
                        t.show_errors = !t.show_errors;
                        cx.notify();
                    })),
            );
        if self.show_errors {
            for (acc, e) in self.errors.iter().take(50) {
                d = d.child(
                    div()
                        .text_size(ts::SMALL)
                        .text_color(p.fg2)
                        .child(format!("{acc}: {e}")),
                );
            }
        }
        Some(d.into_any_element())
    }
}

impl Render for InventoryTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let focused = self.focused().cloned();
        div()
            .id("inv-tab")
            .size_full()
            .flex()
            .flex_col()
            .font_family(SANS)
            .bg(p.bg)
            .text_color(p.fg)
            .child(self.render_accounts(&p, cx))
            .children(
                self.status
                    .as_ref()
                    .map(|(ok, s)| status_line(*ok, SharedString::from(s.clone()), &p).flex_none()),
            )
            .children(self.render_errors(&p, cx))
            .child(self.render_filters(&p, cx))
            .child(self.render_table(&p, cx))
            .children(focused.map(|r| self.render_actions(&r, &p, cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm() -> CloudResource {
        CloudResource {
            provider: Some(Provider::Aws),
            account: "prod".into(),
            region: "eu-west-1".into(),
            kind: Some(ResourceKind::Vm),
            rtype: "EC2 t3.micro".into(),
            id: "i-0abc".into(),
            name: "web-1".into(),
            state: "running".into(),
            public_ips: vec!["203.0.113.10".into()],
            private_ips: vec!["10.0.1.5".into()],
            port: Some(22),
            user: Some("ec2-user".into()),
            ..CloudResource::default()
        }
    }

    #[test]
    fn vm_becomes_a_host_without_secrets() {
        let h = host_for(&vm()).unwrap();
        assert_eq!(h.name, "web-1");
        assert_eq!(h.address, "203.0.113.10");
        assert_eq!(h.user, "ec2-user");
        assert_eq!(h.port, 22);
        assert!(h.secret.is_none());
        let mut win = vm();
        win.port = Some(3389);
        assert_eq!(host_for(&win).unwrap().port, 22);
        let mut db = vm();
        db.kind = Some(ResourceKind::Database);
        assert!(host_for(&db).is_none());
        assert!(importable(&vm()) && !importable(&db));
        let mut storage = vm();
        storage.kind = Some(ResourceKind::Storage);
        assert!(!importable(&storage) && host_for(&storage).is_none());
    }

    #[test]
    fn database_becomes_a_connection() {
        let r = CloudResource {
            kind: Some(ResourceKind::Database),
            name: "orders".into(),
            host: Some("orders.abc.eu-west-1.rds.amazonaws.com".into()),
            port: Some(5433),
            db: Some(DbFlavor::Postgres),
            user: Some("app".into()),
            ..CloudResource::default()
        };
        let d = db_for(&r).unwrap();
        assert_eq!(d.engine, Engine::Postgres);
        assert_eq!(d.server, "orders.abc.eu-west-1.rds.amazonaws.com");
        assert_eq!(d.port, 5433);
        assert_eq!(d.user, "app");
        assert!(d.secret.is_none());
        assert!(db_for(&vm()).is_none());
    }
}
