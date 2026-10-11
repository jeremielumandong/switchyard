//! The detail pane: one item's fields and value, a Key Vault reference's secret, and a
//! deleted secret's Recover / Purge.

use super::*;

impl CloudTab {
    /// The selected item's name, label and kind, with the Value / History switch.
    pub(super) fn render_title(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let item = self.selected();
        let create = self.detail == Detail::New;
        let caps = self.caps();
        let title = if create {
            match self.view {
                View::Flags => "New feature flag".to_owned(),
                _ => "New item".to_owned(),
            }
        } else if self.view == View::Flags {
            Self::text(&self.key, cx)
        } else {
            item.map(|i| i.key.clone()).unwrap_or_default()
        };
        let reference =
            self.view == View::Items && is_key_vault_ref(Some(&Self::text(&self.content_type, cx)));
        let this = cx.entity().downgrade();
        let tab_option = |label: &'static str, t: DetailTab| {
            let this = this.clone();
            let on: ui::OnClick = Box::new(move |_, _, cx| {
                let _ = this.update(cx, |me, cx| {
                    if t == DetailTab::History {
                        me.show_history(cx);
                    } else {
                        me.tab = t;
                        cx.notify();
                    }
                });
            });
            (SharedString::from(label), self.tab == t, on)
        };
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(8.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(ts::TITLE)
                    .font_family(MONO)
                    .font_weight(FontWeight::SEMIBOLD)
                    .truncate()
                    .child(title),
            )
            .when_some(item.and_then(|i| i.label.clone()), |d, l| {
                d.child(chip(l, p))
            })
            .when(reference, |d| {
                d.child(chip("Key Vault reference".into(), p))
            })
            .when(item.and_then(|i| i.locked) == Some(true), |d| {
                d.child(chip("locked".into(), p))
            })
            .when(
                caps.history && !create && self.view != View::Deleted && item.is_some(),
                |d| {
                    d.child(ui::segmented(
                        "cl-tab",
                        vec![
                            tab_option("Value", DetailTab::Value),
                            tab_option("History", DetailTab::History),
                        ],
                        20.,
                        p,
                    ))
                },
            )
            .into_any_element()
    }

    pub(super) fn render_detail(
        &self,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(info) = &self.info else {
            return div().flex_1().into_any_element();
        };
        if self.detail == Detail::None {
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(rpx(6.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child(if self.read_only() {
                    "Select an item to see it."
                } else {
                    "Select an item to see or edit it."
                })
                .when(!self.read_only() && !self.picked.is_empty(), |d| {
                    d.child("Ticked items have their actions at the bottom of the list.")
                })
                .into_any_element();
        }
        let body = if self.view == View::Deleted {
            self.render_deleted(p, cx)
        } else if self.tab == DetailTab::History {
            self.render_history(p, cx)
        } else {
            self.render_value_tab(info, p, window, cx)
        };
        div()
            .id("cl-detail")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap(rpx(10.))
            .p(rpx(14.))
            .child(self.render_title(p, cx))
            .child(body)
            .into_any_element()
    }

    pub(super) fn render_value_tab(
        &self,
        info: &CloudInfo,
        p: &Palette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let caps = &info.caps;
        let create = self.detail == Detail::New;
        let flags = self.view == View::Flags;
        let read_only = self.read_only();
        let item = self.selected();
        let locked = item.and_then(|i| i.locked).unwrap_or(false);
        let frozen = read_only || locked;
        let busy = self.edit_request.is_some();
        let key_label = if flags {
            "Flag ID"
        } else if caps.hierarchical {
            "Name"
        } else {
            "Key"
        };
        let mut fields: Vec<AnyElement> = vec![Self::field(
            key_label,
            Self::boxed(&self.key, true, !create, p),
            p,
        )];
        if caps.labels {
            fields.push(Self::field(
                "Label",
                Self::boxed(&self.label, true, !create, p),
                p,
            ));
        }
        if create && !caps.kinds.is_empty() {
            let this = cx.entity().downgrade();
            let options = caps
                .kinds
                .iter()
                .map(|k| {
                    let k = *k;
                    let this = this.clone();
                    let on: ui::OnClick = Box::new(move |_, _, cx| {
                        let _ = this.update(cx, |t, cx| {
                            t.kind = Some(k);
                            cx.notify();
                        });
                    });
                    (SharedString::from(k), self.kind == Some(k), on)
                })
                .collect();
            fields.push(Self::field(
                "Type",
                ui::segmented("cl-kind", options, 22., p).into_any_element(),
                p,
            ));
        }
        if caps.content_type && !flags {
            fields.push(Self::field(
                "Content type",
                Self::boxed(&self.content_type, true, frozen, p),
                p,
            ));
        }
        if flags
            || matches!(
                self.connection.service,
                CloudService::SecretsManager | CloudService::ParameterStore
            )
        {
            fields.push(Self::field(
                "Description",
                Self::boxed(&self.description, false, frozen, p),
                p,
            ));
        }
        if caps.tags && !flags {
            fields.push(Self::field(
                "Tags",
                Self::boxed(&self.tags, true, frozen, p),
                p,
            ));
        }
        if caps.dates {
            fields.push(Self::field(
                "Activation (UTC)",
                Self::boxed(&self.not_before, true, frozen, p),
                p,
            ));
            fields.push(Self::field(
                "Expiry (UTC)",
                Self::boxed(&self.expires, true, frozen, p),
                p,
            ));
        }
        let enabled_box = (caps.enabled || flags).then(|| {
            ui::checkbox(
                "cl-enabled",
                self.enabled,
                if flags {
                    "Enabled"
                } else {
                    "Enabled (apps can read it)"
                },
                p,
            )
            .when(!frozen, |d| {
                d.on_click(cx.listener(|t, _, _, cx| {
                    t.enabled = !t.enabled;
                    cx.notify();
                }))
            })
        });
        let expired = item
            .and_then(|i| i.expires_ms)
            .is_some_and(|e| e < now_ms());
        let reference =
            self.view == View::Items && is_key_vault_ref(Some(&Self::text(&self.content_type, cx)));
        let value_label = if flags {
            if self.flag_json {
                "Definition (JSON; Enabled and Description above win)"
            } else {
                "Conditions"
            }
        } else if reference {
            "Reference"
        } else {
            "Value"
        };
        let value_header = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(8.))
            .child(
                div()
                    .text_size(ts::LABEL)
                    .text_color(p.fg2)
                    .font_weight(FontWeight::MEDIUM)
                    .child(value_label),
            )
            .when(expired, |d| {
                d.child(
                    div()
                        .text_size(ts::SMALL)
                        .text_color(p.prod)
                        .child("Expired: apps can no longer read it"),
                )
            })
            .child(div().flex_1())
            .when(create && !flags && caps.feature_flags, |d| {
                d.child(
                    ui::button(
                        "cl-make-ref",
                        "Make it a Key Vault reference",
                        Kind::Ghost,
                        p,
                    )
                    .on_click(cx.listener(|t, _, w, cx| t.make_reference(w, cx))),
                )
            })
            .when(flags, |d| {
                d.child(
                    ui::button(
                        "cl-flag-json",
                        if self.flag_json {
                            "Use the form"
                        } else {
                            "Edit JSON"
                        },
                        Kind::Ghost,
                        p,
                    )
                    .on_click(cx.listener(|t, _, w, cx| t.toggle_flag_json(w, cx))),
                )
            })
            .when(
                !flags && !create && self.loaded && !(self.secret_values() && !self.revealed),
                |d| {
                    d.child(
                        ui::button("cl-copy", "Copy", Kind::Ghost, p)
                            .on_click(cx.listener(|t, _, _, cx| t.copy_value(cx))),
                    )
                },
            );
        let hidden = item.is_some_and(|i| self.hides_value(i)) && !(self.loaded && self.revealed);
        let loading_value = self.get_request.is_some();
        let value: AnyElement = if flags && !self.flag_json {
            self.render_flag_form(frozen, p, cx)
        } else if hidden {
            Self::panel(p)
                .items_start()
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child(if self.loaded {
                    "••••••••••••  The value is hidden.".to_owned()
                } else {
                    "The value is a secret and is fetched only when you ask.".to_owned()
                })
                .child(
                    div()
                        .flex()
                        .gap(rpx(8.))
                        .child(
                            ui::button(
                                "cl-reveal",
                                if loading_value {
                                    "Loading…"
                                } else {
                                    "Show value"
                                },
                                Kind::Secondary,
                                p,
                            )
                            .on_click(cx.listener(|t, _, _, cx| {
                                if t.loaded {
                                    t.revealed = true;
                                    cx.notify();
                                } else if let Some(i) = t.selected().cloned() {
                                    t.fetch(&i, cx);
                                }
                            })),
                        )
                        .child(
                            ui::button("cl-copy-secret", "Copy value", Kind::Ghost, p)
                                .on_click(cx.listener(|t, _, _, cx| t.copy_value(cx))),
                        ),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h(rpx(120.))
                .overflow_hidden()
                .border_1()
                .border_color(p.bd2)
                .rounded(px(6.))
                .bg(p.bg)
                .p(rpx(6.))
                .child(
                    Editor::new(&self.value)
                        .readonly(frozen || !self.loaded)
                        .bordered(false)
                        .appearance(false)
                        .h(relative(1.))
                        .font_family(MONO)
                        .text_size(ts::BODY),
                )
                .into_any_element()
        };
        let reference_card = (reference && !create).then(|| self.render_reference(p, cx));
        let secret_hide =
            (self.secret_values() && self.loaded && self.revealed && !create).then(|| {
                ui::button("cl-hide", "Hide value", Kind::Ghost, p).on_click(cx.listener(
                    |t, _, _, cx| {
                        t.revealed = false;
                        cx.notify();
                    },
                ))
            });
        let meta = item.map(|i| {
            let mut parts = Vec::new();
            if let Some(ms) = i.modified_ms {
                parts.push(format!("Changed {}", display_ms(ms)));
            }
            if locked {
                parts.push("Locked: unlock to change or delete it".into());
            }
            parts.join(" · ")
        });
        let actions = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(8.))
            .pt(rpx(8.))
            .border_t_1()
            .border_color(p.line)
            .when(!read_only, |d| match &self.confirm {
                Some(Confirm::Delete) => d
                    .child(div().text_size(ts::BODY).child(format!(
                        "Delete {}{}?",
                        item.map(|i| i.key.as_str()).unwrap_or(""),
                        match self.connection.service {
                            CloudService::SecretsManager => " (recoverable for 7 days)",
                            CloudService::KeyVault =>
                                " (recoverable from Deleted while the vault keeps them)",
                            _ => "",
                        }
                    )))
                    .child(
                        ui::button("cl-del-yes", "Delete", Kind::Destructive, p)
                            .on_click(cx.listener(|t, _, _, cx| t.delete(true, cx))),
                    )
                    .child(ui::button("cl-del-no", "Cancel", Kind::Ghost, p).on_click(
                        cx.listener(|t, _, _, cx| {
                            t.confirm = None;
                            cx.notify();
                        }),
                    )),
                Some(Confirm::Save) => d
                    .child(
                        div()
                            .text_size(ts::BODY)
                            .child("This is a Production connection. Save the change?"),
                    )
                    .child(
                        ui::button("cl-save-yes", "Save to Production", Kind::Destructive, p)
                            .on_click(cx.listener(|t, _, _, cx| t.save(true, cx))),
                    )
                    .child(ui::button("cl-save-no", "Cancel", Kind::Ghost, p).on_click(
                        cx.listener(|t, _, _, cx| {
                            t.confirm = None;
                            cx.notify();
                        }),
                    )),
                _ => d
                    .when(!locked, |d| {
                        d.child(
                            ui::button(
                                "cl-save",
                                if busy {
                                    "Saving…"
                                } else if create {
                                    "Create"
                                } else {
                                    "Save"
                                },
                                Kind::Primary,
                                p,
                            )
                            .on_click(cx.listener(|t, _, _, cx| t.save(false, cx))),
                        )
                    })
                    .when(!create && !locked, |d| {
                        d.child(
                            ui::button("cl-delete", "Delete", Kind::Secondary, p)
                                .on_click(cx.listener(|t, _, _, cx| t.delete(false, cx))),
                        )
                    })
                    .when(!create && caps.locks, |d| {
                        d.child(
                            ui::button(
                                "cl-lock",
                                if locked { "Unlock" } else { "Lock" },
                                Kind::Ghost,
                                p,
                            )
                            .on_click(cx.listener(|t, _, _, cx| t.toggle_lock(cx))),
                        )
                    })
                    .when(create, |d| {
                        d.child(ui::button("cl-cancel", "Cancel", Kind::Ghost, p).on_click(
                            cx.listener(|t, _, _, cx| {
                                t.detail = Detail::None;
                                cx.notify();
                            }),
                        ))
                    }),
            })
            .children(secret_hide)
            .child(div().flex_1())
            .when_some(meta, |d, m| {
                d.child(
                    div()
                        .text_size(ts::SMALL)
                        .text_color(p.fg3)
                        .truncate()
                        .child(m),
                )
            });
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(rpx(10.))
            .child(
                div()
                    .flex_none()
                    .grid()
                    .grid_cols(2)
                    .gap(rpx(10.))
                    .children(fields),
            )
            .children(enabled_box)
            .children(reference_card)
            .child(value_header)
            .child(value)
            .child(actions)
            .into_any_element()
    }

    /// A Key Vault reference: where it points, and its secret on request.
    pub(super) fn render_reference(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let text = self.value.read(cx).value().to_string();
        let target = key_vault_ref_uri(&text).and_then(|u| parse_secret_uri(&u).ok());
        let r = &self.resolved;
        let loading = r.request.is_some();
        let where_to = match &target {
            Some(t) => format!(
                "{} › {}{}",
                t.vault.host_str().unwrap_or_default(),
                t.name,
                t.version
                    .as_ref()
                    .map(|v| format!(" (version {v})"))
                    .unwrap_or_default()
            ),
            None => "The value has no valid \"uri\"".into(),
        };
        Self::panel(p)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .child(
                        div()
                            .text_size(ts::SMALL)
                            .text_color(p.fg3)
                            .flex_none()
                            .child("Key Vault secret"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(ts::BODY)
                            .font_family(MONO)
                            .text_color(p.acc)
                            .truncate()
                            .child(where_to),
                    )
                    .when(target.is_some(), |d| {
                        d.child(
                            ui::button(
                                "cl-ref-show",
                                if loading {
                                    "Reading…"
                                } else if r.shown {
                                    "Hide secret"
                                } else {
                                    "Show secret"
                                },
                                Kind::Secondary,
                                p,
                            )
                            .on_click(cx.listener(|t, _, _, cx| t.resolve(false, cx))),
                        )
                        .child(
                            ui::button("cl-ref-copy", "Copy secret", Kind::Ghost, p)
                                .on_click(cx.listener(|t, _, _, cx| t.resolve(true, cx))),
                        )
                    }),
            )
            .when_some(r.error.clone(), |d, e| {
                d.child(div().text_size(ts::SMALL).text_color(p.prod).child(e))
            })
            .when_some(r.value.clone().filter(|_| r.shown), |d, v| {
                d.child(
                    div()
                        .text_size(ts::BODY)
                        .font_family(MONO)
                        .text_color(p.fg)
                        .child(v),
                )
            })
            .when(r.value.is_some() && !r.shown, |d| {
                d.child(
                    div()
                        .text_size(ts::BODY)
                        .text_color(p.fg3)
                        .child("••••••••••••"),
                )
            })
            .into_any_element()
    }

    /// A deleted secret: when it goes for good, and Recover / Purge.
    pub(super) fn render_deleted(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let item = self.selected();
        let read_only = self.read_only();
        let key = item.map(|i| i.key.clone()).unwrap_or_default();
        let k2 = key.clone();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(rpx(10.))
            .child(
                Self::panel(p)
                    .text_size(ts::BODY)
                    .child(
                        item.and_then(|i| i.kind.clone())
                            .unwrap_or_else(|| "Deleted".into()),
                    )
                    .child(
                        div()
                            .text_size(ts::SMALL)
                            .text_color(p.fg3)
                            .child("Recovering brings the secret back with all its versions. Purging deletes it for good."),
                    ),
            )
            .when(!read_only, |d| {
                d.child(
                    div()
                        .flex()
                        .gap(rpx(8.))
                        .child(
                            ui::button("cl-recover", "Recover", Kind::Primary, p).on_click(
                                cx.listener(move |t, _, _, cx| {
                                    t.ask(
                                        format!("Recover {key}?"),
                                        "Recover",
                                        CloudEdit::Recover { key: key.clone() },
                                        false,
                                        cx,
                                    )
                                }),
                            ),
                        )
                        .child(
                            ui::button("cl-purge", "Purge…", Kind::Secondary, p).on_click(
                                cx.listener(move |t, _, _, cx| {
                                    t.ask(
                                        format!("Purge {k2}? It can't be recovered afterwards."),
                                        "Purge",
                                        CloudEdit::Purge { key: k2.clone() },
                                        true,
                                        cx,
                                    )
                                }),
                            ),
                        ),
                )
            })
            .into_any_element()
    }

    pub(super) fn resolve(&mut self, copy: bool, cx: &mut Context<Self>) {
        let text = self.value.read(cx).value().to_string();
        let Some(uri) = key_vault_ref_uri(&text) else {
            self.resolved.error = Some("The value has no \"uri\"".into());
            cx.notify();
            return;
        };
        if let Some(v) = self.resolved.value.clone() {
            if copy {
                self.copy_text(v, cx);
            } else {
                self.resolved.shown = !self.resolved.shown;
            }
            cx.notify();
            return;
        }
        let request = next_id();
        self.resolved = Resolved {
            request: Some(request),
            copy,
            ..Resolved::default()
        };
        self.core.send(Command::CloudResolveRef {
            session: self.session,
            request,
            uri,
        });
        cx.notify();
    }

    /// A Key Vault reference's secret arrived.
    pub fn on_resolved(
        &mut self,
        request: RequestId,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        if self.resolved.request != Some(request) {
            return;
        }
        self.resolved.request = None;
        match result {
            Ok(v) => {
                if std::mem::take(&mut self.resolved.copy) {
                    self.copy_text(v.clone(), cx);
                } else {
                    self.resolved.shown = true;
                }
                self.resolved.value = Some(v);
            }
            Err(e) => self.resolved.error = Some(e),
        }
        cx.notify();
    }
}
