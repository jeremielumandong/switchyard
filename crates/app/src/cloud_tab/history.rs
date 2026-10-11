//! Earlier versions of the selected item (App Configuration revisions, Key Vault
//! versions): the list, one version's value, and Restore.

use super::*;

impl CloudTab {
    pub(super) fn show_history(&mut self, cx: &mut Context<Self>) {
        self.tab = DetailTab::History;
        let Some(item) = self.selected().cloned() else {
            return;
        };
        let request = next_id();
        self.history = History {
            request: Some(request),
            ..History::default()
        };
        self.core.send(Command::CloudRevisions {
            session: self.session,
            request,
            key: item.key,
            label: item.label,
        });
        cx.notify();
    }

    /// Earlier versions arrived.
    pub fn on_revisions(
        &mut self,
        request: RequestId,
        result: Result<Vec<KvItem>, String>,
        cx: &mut Context<Self>,
    ) {
        if self.history.request != Some(request) {
            return;
        }
        self.history.request = None;
        match result {
            Ok(items) => self.history.items = items,
            Err(e) => self.history.error = Some(e),
        }
        cx.notify();
    }

    pub(super) fn choose_revision(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.history.chosen = Some(ix);
        self.history.revealed = false;
        if let Some(r) = self.history.items.get(ix)
            && r.value.is_none()
            && let Some(version) = r.version.clone()
        {
            let request = next_id();
            self.history.value_request = Some(request);
            self.core.send(Command::CloudGet {
                session: self.session,
                request,
                scope: self.scope.clone(),
                key: r.key.clone(),
                label: r.label.clone(),
                version: Some(version),
            });
        }
        cx.notify();
    }

    /// Write the chosen version's value back as the current one.
    pub(super) fn restore(&mut self, cx: &mut Context<Self>) {
        let Some(r) = self
            .history
            .chosen
            .and_then(|i| self.history.items.get(i))
            .cloned()
        else {
            return;
        };
        let Some(value) = r.value.clone() else {
            return;
        };
        let current = self.selected().cloned();
        let when = r.modified_ms.map(display_ms).unwrap_or_default();
        let edit = CloudEdit::Put(KvWrite {
            scope: self.scope.clone(),
            key: r.key.clone(),
            label: r.label.clone(),
            value,
            content_type: r.content_type.clone(),
            tags: r.tags.clone(),
            enabled: r.enabled,
            etag: current.and_then(|c| c.etag),
            not_before_ms: r.not_before_ms,
            expires_ms: r.expires_ms,
            ..KvWrite::default()
        });
        self.ask(
            format!("Restore {} to the version of {when}?", r.key),
            "Restore",
            edit,
            true,
            cx,
        );
    }

    /// Earlier versions of the selected item, and the chosen one with Restore.
    pub(super) fn render_history(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let h = &self.history;
        let secret = self.secret_values();
        if let Some(e) = &h.error {
            return div()
                .text_size(ts::BODY)
                .text_color(p.prod)
                .child(e.clone())
                .into_any_element();
        }
        if h.request.is_some() {
            return div()
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child("Loading versions…")
                .into_any_element();
        }
        if h.items.is_empty() {
            return div()
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child("No earlier versions are kept for this item.")
                .into_any_element();
        }
        let items = h.items.clone();
        let chosen = h.chosen;
        let p2 = *p;
        let list = uniform_list(
            "cl-history",
            items.len(),
            cx.processor(move |_this, range: std::ops::Range<usize>, _w, cx| {
                let p = &p2;
                range
                    .map(|i| {
                        let r = &items[i];
                        let when = r.modified_ms.map(display_ms).unwrap_or_else(|| "—".into());
                        let mut sub = Vec::new();
                        if i == 0 {
                            sub.push("current".to_owned());
                        }
                        if r.enabled == Some(false) {
                            sub.push("disabled".into());
                        }
                        if let Some(e) = r.expires_ms {
                            sub.push(format!("expires {}", display_ms(e)));
                        }
                        if !secret && let Some(v) = &r.value {
                            sub.push(v.chars().take(60).collect::<String>().replace('\n', " "));
                        }
                        let sel = chosen == Some(i);
                        div()
                            .id(("cl-rev", i))
                            .w_full()
                            .h(rpx(ROW_H))
                            .flex()
                            .flex_col()
                            .justify_center()
                            .gap(rpx(2.))
                            .px(rpx(10.))
                            .border_b_1()
                            .border_color(p.line)
                            .cursor_pointer()
                            .when(sel, |d| d.bg(p.sel))
                            .when(!sel, |d| d.hover(|s| s.bg(p.hover)))
                            .on_click(cx.listener(move |t, _, _, cx| t.choose_revision(i, cx)))
                            .child(div().text_size(ts::BODY).child(when))
                            .child(
                                div()
                                    .text_size(ts::SMALL)
                                    .text_color(p.fg3)
                                    .truncate()
                                    .child(sub.join(" · ")),
                            )
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1();
        let preview: AnyElement = match chosen.and_then(|i| h.items.get(i).map(|r| (i, r))) {
            None => div()
                .p(rpx(12.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child("Choose a version to see its value.")
                .into_any_element(),
            Some((i, r)) => {
                let value = r.value.clone();
                let masked = secret && !h.revealed;
                let can_restore = i > 0 && value.is_some() && !self.read_only();
                div()
                    .id("cl-rev-value")
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap(rpx(8.))
                    .p(rpx(10.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(rpx(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(ts::LABEL)
                                    .text_color(p.fg2)
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(format!(
                                        "Version of {}",
                                        r.modified_ms.map(display_ms).unwrap_or_default()
                                    )),
                            )
                            .when(secret && value.is_some(), |d| {
                                d.child(
                                    ui::button(
                                        "cl-rev-reveal",
                                        if masked { "Show value" } else { "Hide value" },
                                        Kind::Ghost,
                                        p,
                                    )
                                    .on_click(cx.listener(
                                        |t, _, _, cx| {
                                            t.history.revealed = !t.history.revealed;
                                            cx.notify();
                                        },
                                    )),
                                )
                            })
                            .when_some(value.clone(), |d, v| {
                                d.child(ui::button("cl-rev-copy", "Copy", Kind::Ghost, p).on_click(
                                    cx.listener(move |t, _, _, cx| t.copy_text(v.clone(), cx)),
                                ))
                            })
                            .when(can_restore, |d| {
                                d.child(
                                    ui::button(
                                        "cl-restore",
                                        "Restore this version",
                                        Kind::Primary,
                                        p,
                                    )
                                    .on_click(cx.listener(|t, _, _, cx| t.restore(cx))),
                                )
                            }),
                    )
                    .when_some(r.content_type.clone(), |d, c| {
                        d.child(
                            div()
                                .text_size(ts::SMALL)
                                .text_color(p.fg3)
                                .child(format!("Content type {c}")),
                        )
                    })
                    .child(
                        div()
                            .id("cl-rev-text")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p(rpx(8.))
                            .border_1()
                            .border_color(p.bd2)
                            .rounded(px(6.))
                            .bg(p.bg)
                            .text_size(ts::BODY)
                            .font_family(MONO)
                            .child(match (&value, masked) {
                                (None, _) if h.value_request.is_some() => "Loading…".to_owned(),
                                (None, _) => "No value".to_owned(),
                                (Some(_), true) => "••••••••••••".to_owned(),
                                (Some(v), false) if looks_like_json(v) => pretty_json(v),
                                (Some(v), false) => v.clone(),
                            }),
                    )
                    .into_any_element()
            }
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .border_1()
            .border_color(p.bd)
            .rounded(px(6.))
            .overflow_hidden()
            .child(
                div()
                    .w(rpx(260.))
                    .flex_none()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .border_r_1()
                    .border_color(p.bd)
                    .child(list),
            )
            .child(preview)
            .into_any_element()
    }
}
