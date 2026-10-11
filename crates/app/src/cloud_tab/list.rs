//! The item list: filters, flat or grouped rows, ticks and the bulk actions bar.

use super::*;

impl CloudTab {
    pub(super) fn render_list(
        &self,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let caps = self.caps();
        let loading = self.list_request.is_some();
        let flags = self.view == View::Flags;
        let deleted = self.view == View::Deleted;
        let selected = match self.detail {
            Detail::Item(ix) => Some(ix),
            _ => None,
        };
        let labels_menu = (caps.labels && !deleted).then(|| {
            let this = cx.entity().downgrade();
            let labels = self.labels.clone();
            Button::new("cl-labels")
                .outline()
                .small()
                .label("▾")
                .dropdown_menu(move |mut menu, _, _| {
                    let t = this.clone();
                    menu = menu.item(PopupMenuItem::new("Any label").on_click(move |_, w, cx| {
                        let _ = t.update(cx, |t, cx| {
                            Self::set_input(&t.label_filter, "", w, cx);
                            t.reload(cx);
                        });
                    }));
                    for l in &labels {
                        let t = this.clone();
                        let l = l.clone();
                        menu = menu.item(PopupMenuItem::new(label_text(l.as_deref())).on_click(
                            move |_, w, cx| {
                                let l = l.clone();
                                let _ = t.update(cx, |t, cx| t.set_label_filter(l, w, cx));
                            },
                        ));
                    }
                    menu
                })
        });
        let filter_bar = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(6.))
            .p(rpx(8.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.filter).text_size(ts::BODY)),
            )
            .when(caps.labels && !deleted, |d| {
                d.child(
                    div()
                        .w(rpx(120.))
                        .flex_none()
                        .child(Input::new(&self.label_filter).text_size(ts::BODY)),
                )
            })
            .children(labels_menu);
        let this = cx.entity().downgrade();
        let layout_option = |label: &'static str, l: Layout| {
            let this = this.clone();
            let on: ui::OnClick = Box::new(move |_, _, cx| {
                let _ = this.update(cx, |t, cx| t.set_layout(l, cx));
            });
            (SharedString::from(label), self.layout == l, on)
        };
        let hint = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(8.))
            .px(rpx(10.))
            .pb(rpx(6.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .truncate()
                    .child(if caps.labels {
                        format!("{} · label \"-\": no label · ↵ to search", caps.filter_hint)
                    } else {
                        format!("{} · ↵ to search", caps.filter_hint)
                    }),
            )
            .when(self.view == View::Items, |d| {
                d.child(ui::segmented(
                    "cl-layout",
                    vec![
                        layout_option("Grouped", Layout::Tree),
                        layout_option("Flat", Layout::Flat),
                    ],
                    18.,
                    p,
                ))
            });
        let body: AnyElement = if let Some(e) = &self.list_error {
            div()
                .p(rpx(12.))
                .text_size(ts::BODY)
                .text_color(p.prod)
                .child(e.clone())
                .into_any_element()
        } else if caps.scope_label.is_some() && self.scope.is_none() && self.info.is_some() {
            div()
                .p(rpx(12.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child("No namespaces in this account yet.")
                .into_any_element()
        } else if self.items.is_empty() && !loading && self.info.is_some() {
            div()
                .p(rpx(12.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child(match self.view {
                    View::Flags => "No feature flags match.",
                    View::Deleted => "No deleted secrets.",
                    View::Items => "Nothing matches.",
                })
                .into_any_element()
        } else {
            let items: Vec<KvItem> = self.items.clone();
            let rows: Vec<Row> = self.rows.clone();
            let picked = self.picked.clone();
            let p2 = *p;
            let read_only = self.read_only();
            let pickable = !read_only || self.files();
            let now = now_ms();
            uniform_list(
                "cloud-items",
                rows.len(),
                cx.processor(move |_this, range: std::ops::Range<usize>, _w, cx| {
                    let p = &p2;
                    range
                        .map(|r| match &rows[r] {
                            Row::Group {
                                path,
                                name,
                                depth,
                                count,
                                open,
                            } => {
                                let path = path.clone();
                                div()
                                    .id(("cl-group", r))
                                    .w_full()
                                    .h(rpx(ROW_H))
                                    .flex()
                                    .items_center()
                                    .gap(rpx(6.))
                                    .pl(rpx(10. + INDENT * *depth as f32))
                                    .pr(rpx(10.))
                                    .border_b_1()
                                    .border_color(p.line)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(p.hover))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_folder(path.clone(), cx)
                                    }))
                                    .child(
                                        div()
                                            .w(rpx(12.))
                                            .flex_none()
                                            .text_size(ts::SMALL)
                                            .text_color(p.fg3)
                                            .child(if *open { "▾" } else { "▸" }),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_size(ts::BODY)
                                            .font_family(MONO)
                                            .font_weight(FontWeight::MEDIUM)
                                            .truncate()
                                            .child(name.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(ts::SMALL)
                                            .text_color(p.fg3)
                                            .child(count.to_string()),
                                    )
                                    .into_any_element()
                            }
                            Row::Item { ix, depth, name } => {
                                let ix = *ix;
                                let item = &items[ix];
                                let is_sel = selected == Some(ix);
                                let flag = flags
                                    .then(|| {
                                        parse_flag(&item.key, item.value.as_deref().unwrap_or(""))
                                    })
                                    .flatten();
                                let title = match &flag {
                                    Some(f) => f.id.clone(),
                                    None if name.is_empty() => item.key.clone(),
                                    None => name.clone(),
                                };
                                let (sub, sub_accent) =
                                    item_summary(item, flag.as_ref(), &caps, now);
                                div()
                                    .id(("cl-item", ix))
                                    .w_full()
                                    .h(rpx(ROW_H))
                                    .flex()
                                    .items_center()
                                    .gap(rpx(8.))
                                    .pl(rpx(10. + INDENT * *depth as f32))
                                    .pr(rpx(10.))
                                    .border_b_1()
                                    .border_color(p.line)
                                    .cursor_pointer()
                                    .when(is_sel, |d| d.bg(p.sel))
                                    .when(!is_sel, |d| d.hover(|s| s.bg(p.hover)))
                                    .on_click(
                                        cx.listener(move |this, _, w, cx| this.select(ix, w, cx)),
                                    )
                                    .when(pickable, |d| {
                                        let on = picked.contains(&ix);
                                        d.child(
                                            div()
                                                .id(("cl-pick", ix))
                                                .flex_none()
                                                .size(rpx(14.))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .border_1()
                                                .border_color(if on { p.acc } else { p.bd2 })
                                                .rounded(px(3.))
                                                .bg(if on { p.acc } else { p.surface })
                                                .text_color(p.acc_fg)
                                                .text_size(ts::CAPTION)
                                                .child(if on { "✓" } else { "" })
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    cx.stop_propagation();
                                                    this.toggle_pick(ix, cx)
                                                })),
                                        )
                                    })
                                    .when_some(flag.as_ref().map(|f| f.enabled), |d, on| {
                                        d.child(
                                            div()
                                                .id(("cl-flag", ix))
                                                .flex_none()
                                                .w(rpx(28.))
                                                .h(rpx(16.))
                                                .rounded(px(8.))
                                                .bg(if on { p.dev } else { p.bd2 })
                                                .flex()
                                                .items_center()
                                                .when(on, |d| d.justify_end())
                                                .px(rpx(2.))
                                                .when(!read_only, |d| {
                                                    d.on_click(cx.listener(
                                                        move |this, _, _, cx| {
                                                            cx.stop_propagation();
                                                            this.toggle_flag(ix, cx)
                                                        },
                                                    ))
                                                })
                                                .child(
                                                    div().size(rpx(12.)).rounded(px(6.)).bg(p.elev),
                                                ),
                                        )
                                    })
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .justify_center()
                                            .gap(rpx(2.))
                                            .child(
                                                div()
                                                    .text_size(ts::BODY)
                                                    .font_family(MONO)
                                                    .truncate()
                                                    .when(item.enabled == Some(false), |d| {
                                                        d.text_color(p.fg3)
                                                    })
                                                    .child(title),
                                            )
                                            .when(!sub.is_empty(), |d| {
                                                d.child(
                                                    div()
                                                        .text_size(ts::SMALL)
                                                        .text_color(match sub_accent {
                                                            Accent::Plain => p.fg3,
                                                            Accent::Link => p.acc,
                                                            Accent::Warn => p.prod,
                                                        })
                                                        .truncate()
                                                        .child(sub),
                                                )
                                            }),
                                    )
                                    .when_some(item.label.clone(), |d, l| d.child(chip(l, p)))
                                    .when(item.locked == Some(true), |d| {
                                        d.child(chip("locked".into(), p))
                                    })
                                    .when(item.enabled == Some(false), |d| {
                                        d.child(chip("disabled".into(), p))
                                    })
                                    .into_any_element()
                            }
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1()
            .into_any_element()
        };
        let bulk = (!self.picked.is_empty()).then(|| self.render_bulk(p, cx));
        let footer =
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(rpx(8.))
                .px(rpx(10.))
                .py(rpx(6.))
                .border_t_1()
                .border_color(p.bd)
                .text_size(ts::SMALL)
                .text_color(p.fg3)
                .child(if self.export_pending.is_some() {
                    format!("Loading every page to export… {}", self.items.len())
                } else if loading {
                    "Loading…".to_owned()
                } else {
                    let n = self.items.len();
                    format!(
                        "{n}{} {}",
                        if self.next.is_some() { "+" } else { "" },
                        match self.view {
                            View::Flags => format!("flag{}", plural(n)),
                            View::Deleted => "deleted".into(),
                            View::Items => format!("item{}", plural(n)),
                        }
                    )
                })
                .child(div().flex_1())
                .when(
                    self.layout == Layout::Tree && self.view == View::Items,
                    |d| {
                        d.child(
                            ui::button("cl-collapse", "Collapse all", Kind::Ghost, p).on_click(
                                cx.listener(|t, _, _, cx| {
                                    t.expanded.clear();
                                    t.refresh_rows(cx);
                                    cx.notify();
                                }),
                            ),
                        )
                    },
                )
                .when(self.next.is_some() && !loading, |d| {
                    d.child(ui::button("cl-more", "Load more", Kind::Ghost, p).on_click(
                        cx.listener(|t, _, _, cx| {
                            let next = t.next.clone();
                            t.list(next, cx);
                        }),
                    ))
                });
        let _ = window;
        div()
            .w(rpx(LIST_W))
            .flex_none()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(p.bd)
            .child(filter_bar)
            .child(hint)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(body),
            )
            .children(bulk)
            .child(footer)
            .into_any_element()
    }

    /// Actions for the ticked items.
    pub(super) fn render_bulk(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let n = self.picked.len();
        let read_only = self.read_only();
        let deleted = self.view == View::Deleted;
        let labels = self.caps().labels;
        let files = self.files();
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(rpx(6.))
            .px(rpx(10.))
            .py(rpx(6.))
            .border_t_1()
            .border_color(p.bd)
            .bg(p.elev)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(4.))
                    .child(
                        div()
                            .text_size(ts::BODY)
                            .font_weight(FontWeight::MEDIUM)
                            .child(format!("{n} selected")),
                    )
                    .child(div().flex_1())
                    .when(deleted && !read_only, |d| {
                        d.child(
                            ui::button("cl-b-recover", "Recover", Kind::Secondary, p)
                                .on_click(cx.listener(|t, _, _, cx| t.recover_picked(false, cx))),
                        )
                        .child(
                            ui::button("cl-b-purge", "Purge", Kind::Ghost, p)
                                .on_click(cx.listener(|t, _, _, cx| t.recover_picked(true, cx))),
                        )
                    })
                    .when(files && !deleted, |d| {
                        d.child(
                            ui::button("cl-b-export", "Export", Kind::Ghost, p)
                                .on_click(cx.listener(|t, _, _, cx| t.export(KvFormat::Json, cx))),
                        )
                    })
                    .when(labels && !read_only && !deleted, |d| {
                        d.child(
                            ui::button("cl-b-copy", "Copy to…", Kind::Ghost, p).on_click(
                                cx.listener(|t, _, _, cx| {
                                    t.bulk_copy = !t.bulk_copy;
                                    cx.notify();
                                }),
                            ),
                        )
                    })
                    .when(!read_only && !deleted, |d| {
                        d.child(
                            ui::button("cl-b-delete", "Delete", Kind::Secondary, p)
                                .on_click(cx.listener(|t, _, _, cx| t.delete_picked(cx))),
                        )
                    })
                    .child(ui::button("cl-b-clear", "Clear", Kind::Ghost, p).on_click(
                        cx.listener(|t, _, _, cx| {
                            t.picked.clear();
                            t.bulk_copy = false;
                            cx.notify();
                        }),
                    )),
            )
            .when(self.bulk_copy, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(rpx(6.))
                        .child(div().flex_1().min_w_0().child(Self::boxed(
                            &self.bulk_label,
                            true,
                            false,
                            p,
                        )))
                        .child(
                            ui::button("cl-b-copy-go", "Copy", Kind::Primary, p)
                                .on_click(cx.listener(|t, _, _, cx| t.copy_picked(cx))),
                        ),
                )
            })
            .into_any_element()
    }

    pub(super) fn toggle_pick(&mut self, ix: usize, cx: &mut Context<Self>) {
        if !self.picked.remove(&ix) {
            self.picked.insert(ix);
        }
        cx.notify();
    }

    pub(super) fn picked_items(&self) -> Vec<KvItem> {
        let mut ix: Vec<usize> = self.picked.iter().copied().collect();
        ix.sort_unstable();
        ix.into_iter()
            .filter_map(|i| self.items.get(i).cloned())
            .collect()
    }

    pub(super) fn delete_picked(&mut self, cx: &mut Context<Self>) {
        let items = self.picked_items();
        let edits = items
            .iter()
            .map(|i| CloudEdit::Delete {
                scope: self.scope.clone(),
                key: i.key.clone(),
                label: i.label.clone(),
            })
            .collect();
        self.ask(
            format!("Delete {} item{}?", items.len(), plural(items.len())),
            "Delete",
            CloudEdit::Batch(edits),
            true,
            cx,
        );
    }

    pub(super) fn recover_picked(&mut self, purge: bool, cx: &mut Context<Self>) {
        let items = self.picked_items();
        let edits = items
            .iter()
            .map(|i| {
                if purge {
                    CloudEdit::Purge { key: i.key.clone() }
                } else {
                    CloudEdit::Recover { key: i.key.clone() }
                }
            })
            .collect();
        let n = items.len();
        if purge {
            self.ask(
                format!(
                    "Purge {n} secret{}? They can't be recovered afterwards.",
                    plural(n)
                ),
                "Purge",
                CloudEdit::Batch(edits),
                true,
                cx,
            );
        } else {
            self.ask(
                format!("Recover {n} secret{}?", plural(n)),
                "Recover",
                CloudEdit::Batch(edits),
                false,
                cx,
            );
        }
    }

    /// Copy the ticked settings to the label typed in the bulk bar (overwriting).
    pub(super) fn copy_picked(&mut self, cx: &mut Context<Self>) {
        let to = Self::text(&self.bulk_label, cx);
        let to = Some(to).filter(|t| !t.is_empty());
        let items = self.picked_items();
        let keys: HashSet<String> = items.iter().map(|i| i.key.clone()).collect();
        let edits = compare::copy_edits(&items, &keys, to.as_deref());
        if edits.is_empty() {
            return;
        }
        let n = edits.len();
        self.ask(
            format!(
                "Copy {n} setting{} to {}? Settings already there are overwritten.",
                plural(n),
                label_text(to.as_deref())
            ),
            "Copy",
            CloudEdit::Batch(edits),
            true,
            cx,
        );
    }
}

/// The second line of a list row.
pub(super) fn item_summary(
    item: &KvItem,
    flag: Option<&switchyard_core::cloud::appconfig::FeatureFlag>,
    caps: &switchyard_core::cloud::KvCaps,
    now: i64,
) -> (String, Accent) {
    if let Some(f) = flag {
        let mut s = f.description.clone();
        if f.filters > 0 {
            if !s.is_empty() {
                s.push_str(" · ");
            }
            s.push_str(&format!("{} filter{}", f.filters, plural(f.filters)));
        }
        return (s, Accent::Plain);
    }
    let mut parts = Vec::new();
    let mut accent = Accent::Plain;
    if is_key_vault_ref(item.content_type.as_deref())
        && let Some(t) = item
            .value
            .as_deref()
            .and_then(key_vault_ref_uri)
            .and_then(|u| parse_secret_uri(&u).ok())
    {
        let vault = t.vault.host_str().unwrap_or_default();
        let vault = vault.split('.').next().unwrap_or(vault).to_owned();
        parts.push(format!("Key Vault › {vault} › {}", t.name));
        accent = Accent::Link;
    } else {
        if let Some(k) = &item.kind {
            parts.push(k.clone());
        }
        if let Some(v) = item.value.as_deref().filter(|_| caps.values_in_list) {
            let v: String = v.chars().take(80).collect::<String>().replace('\n', " ");
            parts.push(if v.is_empty() { "(empty)".into() } else { v });
        }
    }
    if item.expires_ms.is_some_and(|e| e < now) {
        parts.insert(0, "expired".into());
        accent = Accent::Warn;
    }
    if let Some(ms) = item.modified_ms
        && item
            .kind
            .as_deref()
            .is_none_or(|k| !k.starts_with("deleted"))
    {
        parts.push(display_ms(ms));
    }
    (parts.join(" · "), accent)
}

/// How a list row's second line is colored.
#[derive(Clone, Copy)]
pub(super) enum Accent {
    Plain,
    Link,
    Warn,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_summarize_references_and_expiry() {
        let caps = switchyard_core::cloud::KvCaps {
            values_in_list: true,
            ..Default::default()
        };
        let reference = KvItem {
            key: "Conn".into(),
            value: Some(key_vault_ref(
                "https://kv-dev.vault.azure.net/secrets/Db--Conn",
            )),
            content_type: Some(KEY_VAULT_REF_CONTENT_TYPE.into()),
            ..KvItem::default()
        };
        let (s, _) = item_summary(&reference, None, &caps, 0);
        assert_eq!(s, "Key Vault › kv-dev › Db--Conn");
        let expired = KvItem {
            key: "Old".into(),
            expires_ms: Some(10),
            ..KvItem::default()
        };
        let (s, a) = item_summary(&expired, None, &caps, 20);
        assert_eq!(s, "expired");
        assert!(matches!(a, Accent::Warn));
        assert_eq!(label_query("-"), "\0");
        assert_eq!(label_text(None), "(No label)");
    }
}
