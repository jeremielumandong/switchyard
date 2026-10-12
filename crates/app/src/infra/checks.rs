//! Network checks: DNS records, TLS certificates, TCP ports and HTTP(S), run from this
//! machine or from an SSH Host. Lists of checks are saved by name; "Run all" runs a list
//! and certificates expiring within 30 days are called out.

use std::collections::HashMap;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
};
use switchyard_core::infra::netcheck::{
    CHECK_LISTS_SETTING, CheckKind, CheckList, CheckResult, DNS_TYPES, NetCheck, Verdict,
};
use switchyard_core::infra::{InfraOp, InfraUpdate};
use switchyard_core::store::{Profile, ProfileId};
use switchyard_core::{Command, RequestId, RuntimeHandle};

use super::{field, input_box, pick, plural, status_line, tag};
use crate::app_state::{Profiles, next_id};
use crate::appearance::{rpx, ts};
use crate::theme::{MONO, Palette, SANS, palette};
use crate::ui::{self, Kind};

/// Certificates expiring within this many days are flagged.
const EXPIRY_WARN_DAYS: i64 = 30;

/// One check in the list and its last result.
struct Row {
    check: NetCheck,
    result: Option<CheckResult>,
    running: Option<RequestId>,
    open: bool,
}

/// `Expires in 12 days`-style warning for a result, when due.
pub(crate) fn expiry_note(r: &CheckResult) -> Option<String> {
    let d = r.expires_in_days?;
    if d < 0 {
        Some(format!("expired {} ago", plural((-d) as usize, "day")))
    } else if d < EXPIRY_WARN_DAYS {
        Some(format!("expires in {}", plural(d as usize, "day")))
    } else {
        None
    }
}

/// The network checks tab.
pub struct ChecksTab {
    core: RuntimeHandle,
    profiles: Profiles,
    kind: CheckKind,
    record: String,
    via: Option<ProfileId>,
    target: Entity<InputState>,
    list_name: Entity<InputState>,
    rows: Vec<Row>,
    lists: Vec<CheckList>,
    pending: HashMap<RequestId, usize>,
    status: Option<(bool, String)>,
    _subs: Vec<Subscription>,
}

impl ChecksTab {
    /// A new tab; loads the saved lists.
    pub fn new(
        core: RuntimeHandle,
        profiles: Profiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let target =
            cx.new(|cx| InputState::new(window, cx).placeholder(CheckKind::Dns.placeholder()));
        let list_name = cx.new(|cx| InputState::new(window, cx).placeholder("List name"));
        let subs = vec![
            cx.subscribe_in(&target, window, |t, _, ev: &InputEvent, _, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    t.add(true, cx);
                }
            }),
        ];
        core.send(Command::LoadSetting {
            key: CHECK_LISTS_SETTING.into(),
        });
        Self {
            core,
            profiles,
            kind: CheckKind::Dns,
            record: DNS_TYPES[0].into(),
            via: None,
            target,
            list_name,
            rows: Vec::new(),
            lists: Vec::new(),
            pending: HashMap::new(),
            status: None,
            _subs: subs,
        }
    }

    /// Whether `request` is this tab's.
    pub fn owns(&self, request: RequestId) -> bool {
        self.pending.contains_key(&request)
    }

    /// New profile list.
    pub fn set_profiles(&mut self, profiles: Profiles, cx: &mut Context<Self>) {
        self.profiles = profiles;
        cx.notify();
    }

    /// The saved lists ([`CHECK_LISTS_SETTING`]).
    pub fn set_lists(&mut self, value: Option<serde_json::Value>, cx: &mut Context<Self>) {
        self.lists = value
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        cx.notify();
    }

    fn hosts(&self) -> Vec<(ProfileId, String)> {
        self.profiles
            .all
            .iter()
            .filter_map(|p| match p {
                Profile::Host(h) => Some((h.id.clone(), h.name.clone())),
                _ => None,
            })
            .collect()
    }

    fn host_name(&self, id: &Option<ProfileId>) -> String {
        match id {
            None => "This machine".into(),
            Some(id) => self
                .hosts()
                .into_iter()
                .find(|h| &h.0 == id)
                .map_or_else(|| "missing Host".into(), |h| h.1),
        }
    }

    fn set_kind(&mut self, k: CheckKind, window: &mut Window, cx: &mut Context<Self>) {
        self.kind = k;
        self.target
            .update(cx, |i, cx| i.set_placeholder(k.placeholder(), window, cx));
        cx.notify();
    }

    fn set_record(&mut self, r: String, cx: &mut Context<Self>) {
        self.record = r;
        cx.notify();
    }

    fn set_via(&mut self, v: Option<ProfileId>, cx: &mut Context<Self>) {
        self.via = v;
        cx.notify();
    }

    fn add(&mut self, run: bool, cx: &mut Context<Self>) {
        let target = self.target.read(cx).value().trim().to_owned();
        if target.is_empty() {
            self.status = Some((false, "Enter a target".into()));
            cx.notify();
            return;
        }
        let check = NetCheck {
            kind: self.kind,
            target,
            record: (self.kind == CheckKind::Dns).then(|| self.record.clone()),
            via: self.via.clone(),
        };
        let ix = match self.rows.iter().position(|r| r.check == check) {
            Some(ix) => ix,
            None => {
                self.rows.push(Row {
                    check,
                    result: None,
                    running: None,
                    open: false,
                });
                self.rows.len() - 1
            }
        };
        self.status = None;
        if run {
            self.run(ix, cx);
        }
        cx.notify();
    }

    fn run(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get_mut(ix) else {
            return;
        };
        if row.running.is_some() {
            return;
        }
        let request = next_id();
        row.running = Some(request);
        self.pending.insert(request, ix);
        self.core.send(Command::Infra {
            request,
            op: InfraOp::Check(row.check.clone()),
        });
        cx.notify();
    }

    fn run_all(&mut self, cx: &mut Context<Self>) {
        for ix in 0..self.rows.len() {
            self.run(ix, cx);
        }
    }

    fn remove(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.rows.len() {
            self.rows.remove(ix);
            // Results for later rows arrive by index: shift them.
            self.pending.retain(|_, i| *i != ix);
            for i in self.pending.values_mut() {
                if *i > ix {
                    *i -= 1;
                }
            }
        }
        cx.notify();
    }

    fn save_list(&mut self, cx: &mut Context<Self>) {
        let name = self.list_name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.status = Some((false, "Name the list first".into()));
            cx.notify();
            return;
        }
        let list = CheckList {
            name: name.clone(),
            checks: self.rows.iter().map(|r| r.check.clone()).collect(),
        };
        match self.lists.iter_mut().find(|l| l.name == name) {
            Some(l) => *l = list,
            None => self.lists.push(list),
        }
        self.persist();
        self.status = Some((true, format!("Saved “{name}”")));
        cx.notify();
    }

    fn delete_list(&mut self, cx: &mut Context<Self>) {
        let name = self.list_name.read(cx).value().trim().to_owned();
        let before = self.lists.len();
        self.lists.retain(|l| l.name != name);
        if self.lists.len() != before {
            self.persist();
            self.status = Some((true, format!("Deleted “{name}”")));
        }
        cx.notify();
    }

    fn persist(&self) {
        if let Ok(value) = serde_json::to_value(&self.lists) {
            self.core.send(Command::SetSetting {
                key: CHECK_LISTS_SETTING.into(),
                value,
            });
        }
    }

    fn open_list(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(l) = self.lists.iter().find(|l| l.name == name).cloned() else {
            return;
        };
        self.pending.clear();
        self.rows = l
            .checks
            .into_iter()
            .map(|check| Row {
                check,
                result: None,
                running: None,
                open: false,
            })
            .collect();
        self.list_name
            .update(cx, |i, cx| i.set_value(name, window, cx));
        self.run_all(cx);
    }

    /// An update for one of this tab's requests.
    pub fn on_update(&mut self, request: RequestId, update: InfraUpdate, cx: &mut Context<Self>) {
        let Some(ix) = self.pending.remove(&request) else {
            return;
        };
        if let (Some(row), InfraUpdate::Check(r)) = (self.rows.get_mut(ix), update) {
            row.running = None;
            row.open = row.open || r.verdict != Verdict::Ok;
            row.result = Some(r);
        }
        cx.notify();
    }

    fn copy_report(&mut self, cx: &mut Context<Self>) {
        let mut out = String::new();
        for r in &self.rows {
            let c = &r.check;
            out.push_str(&format!(
                "{} {}{} via {}: ",
                c.kind.label(),
                c.target,
                c.record
                    .as_ref()
                    .map(|r| format!(" {r}"))
                    .unwrap_or_default(),
                self.host_name(&c.via)
            ));
            match &r.result {
                Some(res) => {
                    out.push_str(&format!(
                        "{:?} {} ({} ms)\n",
                        res.verdict, res.summary, res.elapsed_ms
                    ));
                    for (k, v) in &res.details {
                        out.push_str(&format!("    {k}: {v}\n"));
                    }
                }
                None => out.push_str("not run\n"),
            }
        }
        cx.write_to_clipboard(ClipboardItem::new_string(out));
        self.status = Some((true, "Results copied".into()));
        cx.notify();
    }

    // ---------------------------------------------------------------- render

    fn render_form(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let kinds: Vec<(SharedString, bool, ui::OnClick)> = CheckKind::ALL
            .iter()
            .map(|k| {
                let k = *k;
                let this = this.clone();
                let cb: ui::OnClick = Box::new(move |_, window, cx| {
                    let _ = this.update(cx, |t, cx| t.set_kind(k, window, cx));
                });
                (SharedString::from(k.label()), self.kind == k, cb)
            })
            .collect();
        let mut vias = vec![("This machine".to_owned(), self.via.is_none(), None)];
        vias.extend(
            self.hosts()
                .into_iter()
                .map(|(id, n)| (n, self.via.as_ref() == Some(&id), Some(id))),
        );
        let records: Vec<(String, bool, String)> = DNS_TYPES
            .iter()
            .map(|r| (r.to_string(), self.record == *r, r.to_string()))
            .collect();
        let list_items: Vec<(String, bool, String)> = self
            .lists
            .iter()
            .map(|l| {
                (
                    format!("{} · {}", l.name, plural(l.checks.len(), "check")),
                    false,
                    l.name.clone(),
                )
            })
            .collect();
        let has_lists = !list_items.is_empty();
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(rpx(8.))
            .px(rpx(12.))
            .py(rpx(8.))
            .border_b_1()
            .border_color(p.bd)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_end()
                    .gap(rpx(8.))
                    .child(field("Check", ui::segmented("chk-kind", kinds, 22., p), p))
                    .child(field(
                        "Target",
                        input_box(p).w(rpx(300.)).child(
                            Input::new(&self.target)
                                .appearance(false)
                                .text_size(ts::BODY),
                        ),
                        p,
                    ))
                    .when(self.kind == CheckKind::Dns, |d| {
                        d.child(field(
                            "Record",
                            pick(
                                "chk-rec",
                                self.record.clone(),
                                records,
                                cx.entity().downgrade(),
                                Self::set_record,
                            ),
                            p,
                        ))
                    })
                    .child(field(
                        "Run from",
                        pick(
                            "chk-via",
                            self.host_name(&self.via),
                            vias,
                            cx.entity().downgrade(),
                            Self::set_via,
                        ),
                        p,
                    ))
                    .child(
                        ui::button("chk-run", "Run", Kind::Primary, p)
                            .on_click(cx.listener(|t, _, _, cx| t.add(true, cx))),
                    )
                    .child(
                        ui::button("chk-add", "Add to list", Kind::Secondary, p)
                            .on_click(cx.listener(|t, _, _, cx| t.add(false, cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(rpx(8.))
                    .child(field(
                        "Saved list",
                        input_box(p).w(rpx(200.)).child(
                            Input::new(&self.list_name)
                                .appearance(false)
                                .text_size(ts::BODY),
                        ),
                        p,
                    ))
                    .when(has_lists, |d| {
                        let weak = cx.entity().downgrade();
                        d.child(
                            gpui_kit::component::button::Button::new("chk-lists")
                                .outline()
                                .small()
                                .label("Open list")
                                .dropdown_menu(move |mut menu, _, _| {
                                    for (title, _, name) in &list_items {
                                        let weak = weak.clone();
                                        let name = name.clone();
                                        menu =
                                            menu.item(PopupMenuItem::new(title.clone()).on_click(
                                                move |_, window, cx| {
                                                    let _ = weak.update(cx, |t, cx| {
                                                        t.open_list(name.clone(), window, cx)
                                                    });
                                                },
                                            ));
                                    }
                                    menu
                                }),
                        )
                    })
                    .child(
                        ui::button("chk-save", "Save list", Kind::Secondary, p)
                            .on_click(cx.listener(|t, _, _, cx| t.save_list(cx))),
                    )
                    .child(
                        ui::button("chk-del", "Delete list", Kind::Ghost, p)
                            .on_click(cx.listener(|t, _, _, cx| t.delete_list(cx))),
                    )
                    .child(div().flex_1())
                    .when(!self.rows.is_empty(), |d| {
                        d.child(
                            ui::button("chk-copy", "Copy results", Kind::Ghost, p)
                                .on_click(cx.listener(|t, _, _, cx| t.copy_report(cx))),
                        )
                        .child(
                            ui::button(
                                "chk-all",
                                format!("Run all ({})", self.rows.len()),
                                Kind::Secondary,
                                p,
                            )
                            .on_click(cx.listener(|t, _, _, cx| t.run_all(cx))),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_row(&self, ix: usize, row: &Row, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let c = &row.check;
        let (color, mark) = match row.result.as_ref().map(|r| r.verdict) {
            Some(Verdict::Ok) => (p.dev, "●"),
            Some(Verdict::Warn) => (p.stg, "●"),
            Some(Verdict::Fail) => (p.prod, "●"),
            None => (p.fg3, "○"),
        };
        let expiry = row.result.as_ref().and_then(expiry_note);
        let mut d = div()
            .flex_none()
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(p.line)
            .child(
                div()
                    .id(("chk-row", ix))
                    .h(rpx(30.))
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .px(rpx(12.))
                    .cursor_pointer()
                    .hover(|s| s.bg(p.hover))
                    .on_click(cx.listener(move |t, _, _, cx| {
                        if let Some(r) = t.rows.get_mut(ix) {
                            r.open = !r.open;
                        }
                        cx.notify();
                    }))
                    .child(div().w(rpx(12.)).text_color(color).child(mark))
                    .child(tag(c.kind.label().to_uppercase(), p.acc))
                    .child(
                        div()
                            .w(rpx(260.))
                            .flex_none()
                            .truncate()
                            .font_family(MONO)
                            .text_size(ts::BODY)
                            .child(match &c.record {
                                Some(r) => format!("{} {r}", c.target),
                                None => c.target.clone(),
                            }),
                    )
                    .child(
                        div()
                            .w(rpx(110.))
                            .flex_none()
                            .truncate()
                            .text_size(ts::SMALL)
                            .text_color(p.fg3)
                            .child(self.host_name(&c.via)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(ts::BODY)
                            .text_color(if row.result.is_some() { p.fg } else { p.fg3 })
                            .child(match (&row.result, row.running) {
                                (_, Some(_)) => "Running…".to_owned(),
                                (Some(r), None) => r.summary.clone(),
                                (None, None) => "Not run".to_owned(),
                            }),
                    )
                    .when_some(expiry, |d, e| d.child(tag(e.to_uppercase(), p.stg)))
                    .when_some(row.result.as_ref(), |d, r| {
                        d.child(
                            div()
                                .w(rpx(60.))
                                .flex_none()
                                .text_size(ts::SMALL)
                                .text_color(p.fg3)
                                .child(format!("{} ms", r.elapsed_ms)),
                        )
                    })
                    .child(
                        ui::button(("chk-r", ix), "Run", Kind::Ghost, p)
                            .h(rpx(22.))
                            .on_click(cx.listener(move |t, _, _, cx| {
                                cx.stop_propagation();
                                t.run(ix, cx)
                            })),
                    )
                    .child(
                        ui::button(("chk-x", ix), "Remove", Kind::Ghost, p)
                            .h(rpx(22.))
                            .on_click(cx.listener(move |t, _, _, cx| {
                                cx.stop_propagation();
                                t.remove(ix, cx)
                            })),
                    ),
            );
        if row.open
            && let Some(r) = &row.result
        {
            let mut details = div()
                .flex()
                .flex_col()
                .gap(rpx(2.))
                .pl(rpx(42.))
                .pr(rpx(12.))
                .pb(rpx(8.));
            for (k, v) in &r.details {
                details = details.child(
                    div()
                        .flex()
                        .gap(rpx(10.))
                        .text_size(ts::LABEL)
                        .child(
                            div()
                                .w(rpx(130.))
                                .flex_none()
                                .text_color(p.fg3)
                                .child(k.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .font_family(MONO)
                                .text_color(p.fg2)
                                .child(v.clone()),
                        ),
                );
            }
            d = d.child(details);
        }
        d.into_any_element()
    }
}

impl Render for ChecksTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let expiring: Vec<String> = self
            .rows
            .iter()
            .filter_map(|r| {
                let note = expiry_note(r.result.as_ref()?)?;
                Some(format!("{} ({note})", r.check.target))
            })
            .collect();
        let mut list = div().flex().flex_col();
        for (ix, row) in self.rows.iter().enumerate() {
            list = list.child(self.render_row(ix, row, &p, cx));
        }
        div()
            .id("chk-tab")
            .size_full()
            .flex()
            .flex_col()
            .font_family(SANS)
            .bg(p.bg)
            .text_color(p.fg)
            .child(self.render_form(&p, cx))
            .children(
                self.status
                    .as_ref()
                    .map(|(ok, s)| status_line(*ok, s.clone(), &p).flex_none()),
            )
            .when(!expiring.is_empty(), |d| {
                d.child(
                    div()
                        .flex_none()
                        .mx(rpx(12.))
                        .my(rpx(6.))
                        .px(rpx(10.))
                        .py(rpx(6.))
                        .rounded(rpx(5.))
                        .border_1()
                        .border_color(p.stg.opacity(0.5))
                        .text_size(ts::BODY)
                        .text_color(p.stg)
                        .font_weight(FontWeight::MEDIUM)
                        .child(format!(
                            "Certificates expiring within {EXPIRY_WARN_DAYS} days: {}",
                            expiring.join(", ")
                        )),
                )
            })
            .child(if self.rows.is_empty() {
                div()
                    .p(rpx(16.))
                    .text_size(ts::BODY)
                    .text_color(p.fg3)
                    .child("Add a check above, or open a saved list. Checks run from this machine or, with “Run from”, on a Host over SSH (getent, nc, openssl, curl).")
                    .into_any_element()
            } else {
                div()
                    .id("chk-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list)
                    .into_any_element()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(days: Option<i64>) -> CheckResult {
        CheckResult {
            verdict: Verdict::Ok,
            summary: String::new(),
            details: Vec::new(),
            elapsed_ms: 1,
            expires_in_days: days,
        }
    }

    #[test]
    fn expiry_notes() {
        assert_eq!(expiry_note(&result(None)), None);
        assert_eq!(expiry_note(&result(Some(90))), None);
        assert_eq!(
            expiry_note(&result(Some(12))).as_deref(),
            Some("expires in 12 days")
        );
        assert_eq!(
            expiry_note(&result(Some(0))).as_deref(),
            Some("expires in 0 days")
        );
        assert_eq!(
            expiry_note(&result(Some(-1))).as_deref(),
            Some("expired 1 day ago")
        );
    }
}
