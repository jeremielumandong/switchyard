//! Import from and export to files: JSON, `.env` and App Configuration's kvset.

use super::*;

impl CloudTab {
    pub(super) fn pick_import(&mut self, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |t, cx| {
                let request = next_id();
                t.import_request = Some(request);
                t.import = Some(ImportPlan {
                    source: path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    format: KvFormat::Json,
                    writes: Vec::new(),
                });
                t.core.send(Command::ReadTextFile {
                    request,
                    fs: FsRef::Local,
                    path,
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// A file chosen for import was read.
    pub fn on_text_file(
        &mut self,
        request: RequestId,
        result: &Result<TextFile, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.import_request != Some(request) {
            return;
        }
        self.import_request = None;
        let Some(plan) = self.import.as_mut() else {
            return;
        };
        let parsed = result.as_ref().map_err(Clone::clone).and_then(|f| {
            let format = KvFormat::detect(&plan.source, &f.content);
            kv_file::import(&f.content, format, None)
                .map(|w| (format, w))
                .map_err(|e| e.to_string())
        });
        match parsed {
            Ok((format, writes)) if !writes.is_empty() => {
                plan.format = format;
                plan.writes = writes;
                self.compare = None;
                let label = Self::text(&self.label_filter, cx);
                let label = if label == "-" || label.contains('*') {
                    String::new()
                } else {
                    label
                };
                Self::set_input(&self.import_label, label, window, cx);
            }
            Ok(_) => {
                self.import = None;
                self.status = Some((false, "The file has no settings".into()));
            }
            Err(e) => {
                self.import = None;
                self.status = Some((false, format!("Import: {e}")));
            }
        }
        cx.notify();
    }

    pub(super) fn run_import(&mut self, cx: &mut Context<Self>) {
        let Some(plan) = &self.import else { return };
        let label = Some(Self::text(&self.import_label, cx)).filter(|l| !l.is_empty());
        let edits: Vec<CloudEdit> = plan
            .writes
            .iter()
            .cloned()
            .map(|mut w| {
                if plan.format != KvFormat::KvSet {
                    w.label = label.clone();
                }
                w.scope = self.scope.clone();
                CloudEdit::Put(w)
            })
            .collect();
        let n = edits.len();
        let prompt = format!(
            "Import {n} setting{} from {}? Settings with the same key and label are overwritten.",
            plural(n),
            plan.source
        );
        self.ask(prompt, "Import", CloudEdit::Batch(edits), true, cx);
    }

    /// Export the ticked items, else everything the filter matches (loading every page
    /// first).
    pub(super) fn export(&mut self, format: KvFormat, cx: &mut Context<Self>) {
        let items = if self.picked.is_empty() {
            if self.next.is_some() {
                self.export_pending = Some(format);
                let next = self.next.clone();
                self.list(next, cx);
                return;
            }
            self.items.clone()
        } else {
            self.picked_items()
        };
        if items.is_empty() {
            self.status = Some((false, "Nothing to export".into()));
            cx.notify();
            return;
        }
        let n = items.len();
        let text = kv_file::export(&items, format);
        let dir = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        let rx = cx.prompt_for_new_path(&dir, Some(format.file_name()));
        let core = self.core.clone();
        cx.spawn(async move |this, cx| {
            let saved = if let Ok(Ok(Some(path))) = rx.await {
                let shown = path.display().to_string();
                core.send(Command::WriteFile {
                    path,
                    contents: text,
                });
                Some(shown)
            } else {
                None
            };
            let _ = this.update(cx, |t, cx| {
                t.status = Some(match saved {
                    Some(p) => (true, format!("Exported {n} item{} to {p}", plural(n))),
                    None => (false, "Export cancelled".into()),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Settings read from a file, before they are written.
    pub(super) fn render_import(
        &self,
        plan: &ImportPlan,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let loading = self.import_request.is_some();
        let writes = plan.writes.clone();
        let p2 = *p;
        let labels_in_file = plan.format == KvFormat::KvSet;
        let list = uniform_list(
            "cl-import",
            writes.len(),
            cx.processor(move |_this, range: std::ops::Range<usize>, _w, _cx| {
                let p = &p2;
                range
                    .map(|i| {
                        let w = &writes[i];
                        div()
                            .w_full()
                            .h(rpx(28.))
                            .flex()
                            .items_center()
                            .gap(rpx(8.))
                            .px(rpx(10.))
                            .border_b_1()
                            .border_color(p.line)
                            .text_size(ts::BODY)
                            .font_family(MONO)
                            .child(
                                div()
                                    .w(relative(0.4))
                                    .flex_none()
                                    .truncate()
                                    .child(w.key.clone()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(p.fg2)
                                    .child(w.value.replace('\n', " ")),
                            )
                            .when(labels_in_file, |d| {
                                d.child(chip(label_text(w.label.as_deref()), p))
                            })
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1();
        let n = plan.writes.len();
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(rpx(10.))
            .p(rpx(14.))
            .child(
                div()
                    .text_size(ts::TITLE)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if loading {
                        format!("Reading {}…", plan.source)
                    } else {
                        format!(
                            "Import {n} setting{} from {} ({})",
                            plural(n),
                            plan.source,
                            plan.format.title()
                        )
                    }),
            )
            .child(
                div()
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child("Settings with the same key and label are overwritten. Nothing is written until you import."),
            )
            .when(!labels_in_file, |d| {
                d.child(
                    div().w(rpx(320.)).child(Self::field(
                        "Into label",
                        Self::boxed(&self.import_label, true, false, p),
                        p,
                    )),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .border_1()
                    .border_color(p.bd)
                    .rounded(px(6.))
                    .overflow_hidden()
                    .child(list),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .gap(rpx(8.))
                    .child(
                        ui::button("cl-import-go", "Import", Kind::Primary, p)
                            .on_click(cx.listener(|t, _, _, cx| t.run_import(cx))),
                    )
                    .child(ui::button("cl-import-no", "Cancel", Kind::Ghost, p).on_click(
                        cx.listener(|t, _, _, cx| {
                            t.import = None;
                            t.confirm = None;
                            cx.notify();
                        }),
                    )),
            )
            .into_any_element()
    }
}
