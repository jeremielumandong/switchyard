//! The "Import from DBeaver" preview: every connection DBeaver saved, which ones can come
//! over, and a file picker for a workspace Switchyard did not find.

use std::collections::HashSet;
use std::path::PathBuf;

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, PathPromptOptions, StatefulInteractiveElement as _, Styled as _, div, px,
    uniform_list,
};
use switchyard_core::Command;
use switchyard_core::dbeaver_import::DbeaverCandidate;

use crate::appearance::{rpx, ts};
use crate::overlays::{Overlay, dialog, scrim};
use crate::theme::{MONO, Palette};
use crate::ui::{self, Kind};
use crate::workspace::Workspace;

/// The DBeaver import preview.
pub struct DbeaverImportPreview {
    /// Files read (empty: no DBeaver workspace was found).
    pub files: Vec<PathBuf>,
    /// Connections in them.
    pub connections: Vec<DbeaverCandidate>,
    /// Keys ticked for import.
    pub chosen: HashSet<String>,
}

impl DbeaverImportPreview {
    /// A preview with every importable, not yet saved connection ticked.
    pub fn new(files: Vec<PathBuf>, connections: Vec<DbeaverCandidate>) -> Self {
        let chosen = connections
            .iter()
            .filter(|c| importable(c))
            .map(|c| c.key.clone())
            .collect();
        Self {
            files,
            connections,
            chosen,
        }
    }
}

fn importable(c: &DbeaverCandidate) -> bool {
    !c.exists && c.problem.is_none()
}

/// Pick a `data-sources.json` and preview it.
fn choose_file(ws: &mut Workspace, cx: &mut Context<Workspace>) {
    let chosen = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Import DBeaver connections".into()),
    });
    let core = ws.core.clone();
    cx.spawn(async move |_, _| {
        if let Ok(Ok(Some(mut paths))) = chosen.await
            && let Some(path) = paths.pop()
        {
            core.send(Command::PreviewDbeaver { file: Some(path) });
        }
    })
    .detach();
}

fn summary(pr: &DbeaverImportPreview) -> String {
    let from = match pr.files.as_slice() {
        [] => return "No DBeaver workspace found on this computer".into(),
        [one] => one.display().to_string(),
        many => format!("{} DBeaver files", many.len()),
    };
    let ready = pr.connections.iter().filter(|c| importable(c)).count();
    format!(
        "{from} · {} connections, {ready} can be imported",
        pr.connections.len()
    )
}

/// What the row's right column says.
fn status(c: &DbeaverCandidate) -> String {
    if c.exists {
        return "already saved".into();
    }
    if let Some(p) = &c.problem {
        return p.clone();
    }
    let mut parts = Vec::new();
    if let Some(f) = &c.folder {
        parts.push(f.clone());
    }
    parts.push(c.environment.name().to_owned());
    if c.password {
        parts.push("password".into());
    }
    parts.join(" · ")
}

/// Render the preview dialog.
pub(crate) fn render(
    _ws: &Workspace,
    preview: &DbeaverImportPreview,
    p: &Palette,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let chosen = preview.chosen.len();
    let p2 = *p;
    let count = preview.connections.len();
    let list = uniform_list(
        "dbeaver-import-rows",
        count,
        cx.processor(move |this, range: std::ops::Range<usize>, _w, cx| {
            let p = &p2;
            let Some(Overlay::DbeaverImport(pr)) = &this.overlay else {
                return Vec::new();
            };
            range
                .filter_map(|i| pr.connections.get(i).map(|c| (i, c)))
                .map(|(i, c)| {
                    let on = pr.chosen.contains(&c.key);
                    let key = c.key.clone();
                    let ok = importable(c);
                    div()
                        .id(("dbeaver-import", i))
                        .w_full()
                        .flex()
                        .items_center()
                        .gap(rpx(10.))
                        .h(rpx(34.))
                        .px(rpx(12.))
                        .rounded(px(6.))
                        .when(ok, |d| {
                            d.hover(|s| s.bg(p.sel))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(Overlay::DbeaverImport(pr)) = &mut this.overlay
                                        && !pr.chosen.remove(&key)
                                    {
                                        pr.chosen.insert(key.clone());
                                    }
                                    cx.notify();
                                }))
                        })
                        .when(!ok, |d| d.opacity(0.55))
                        .child(ui::checkbox(("dbeaver-import-check", i), on, "", p))
                        .child(
                            div()
                                .w(rpx(160.))
                                .flex_none()
                                .truncate()
                                .text_size(ts::UI)
                                .font_weight(FontWeight::MEDIUM)
                                .child(c.name.clone()),
                        )
                        .child(
                            div()
                                .w(rpx(84.))
                                .flex_none()
                                .truncate()
                                .text_size(ts::SMALL)
                                .text_color(p.fg2)
                                .child(match c.engine {
                                    Some(e) => e.display_name().to_owned(),
                                    None => c.driver.clone(),
                                }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(MONO)
                                .text_size(ts::LABEL)
                                .text_color(p.fg2)
                                .child(c.address.clone()),
                        )
                        .when_some(c.via.clone(), |d, v| {
                            d.child(
                                div()
                                    .flex_none()
                                    .max_w(rpx(160.))
                                    .truncate()
                                    .font_family(MONO)
                                    .text_size(ts::SMALL)
                                    .text_color(p.acc)
                                    .child(format!("via {v}")),
                            )
                        })
                        .child(
                            div()
                                .w(rpx(190.))
                                .flex_none()
                                .truncate()
                                .text_size(ts::LABEL)
                                .text_color(if ok { p.fg3 } else { p.fg2 })
                                .child(status(c)),
                        )
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        }),
    )
    .h(rpx((count.min(12) as f32) * 34.))
    .into_any_element();
    let all: Vec<String> = preview
        .connections
        .iter()
        .filter(|c| importable(c))
        .map(|c| c.key.clone())
        .collect();
    let toggle_all = chosen < all.len();
    let empty = if preview.files.is_empty() {
        Some(
            "Choose DBeaver's data-sources.json: it is in DBeaverData/workspace6/<project>/.dbeaver. \
             Saved passwords come along when credentials-config.json sits beside it.",
        )
    } else if preview.connections.is_empty() {
        Some("No connections in this file.")
    } else {
        None
    };
    scrim(p, true)
        .key_context("Overlay")
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _, w, cx| this.dismiss(w, cx)),
        )
        .child(
            dialog(p, 880.)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(rpx(2.))
                        .px(rpx(16.))
                        .py(rpx(12.))
                        .border_b_1()
                        .border_color(p.bd)
                        .child(
                            div()
                                .text_size(ts::TITLE)
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Import from DBeaver"),
                        )
                        .child(
                            div()
                                .font_family(MONO)
                                .text_size(ts::SMALL)
                                .text_color(p.fg3)
                                .child(summary(preview)),
                        ),
                )
                .child(
                    div()
                        .id("dbeaver-import-list")
                        .p(rpx(4.))
                        .child(list)
                        .when_some(empty, |d, t| {
                            d.child(
                                div()
                                    .p(rpx(24.))
                                    .text_color(p.fg3)
                                    .text_size(ts::UI)
                                    .child(t),
                            )
                        }),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(rpx(8.))
                        .px(rpx(16.))
                        .py(rpx(12.))
                        .border_t_1()
                        .border_color(p.bd)
                        .child(
                            ui::button("dbeaver-import-file", "Choose file…", Kind::Ghost, p)
                                .on_click(cx.listener(|this, _, _, cx| choose_file(this, cx))),
                        )
                        .when(!all.is_empty(), |d| {
                            d.child(
                                ui::button(
                                    "dbeaver-import-all",
                                    if toggle_all {
                                        "Select all"
                                    } else {
                                        "Select none"
                                    },
                                    Kind::Ghost,
                                    p,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        if let Some(Overlay::DbeaverImport(pr)) = &mut this.overlay
                                        {
                                            pr.chosen = if toggle_all {
                                                all.iter().cloned().collect()
                                            } else {
                                                Default::default()
                                            };
                                        }
                                        cx.notify();
                                    },
                                )),
                            )
                        })
                        .child(div().flex_1())
                        .child(
                            ui::button("dbeaver-import-cancel", "Cancel", Kind::Ghost, p)
                                .on_click(cx.listener(|this, _, w, cx| this.dismiss(w, cx))),
                        )
                        .child(
                            ui::button(
                                "dbeaver-import-go",
                                match chosen {
                                    1 => "Import 1 connection".to_owned(),
                                    n => format!("Import {n} connections"),
                                },
                                Kind::Primary,
                                p,
                            )
                            .when(chosen == 0, |b| b.opacity(0.5))
                            .on_click(cx.listener(|this, _, w, cx| {
                                let Some(Overlay::DbeaverImport(pr)) = &this.overlay else {
                                    return;
                                };
                                if pr.chosen.is_empty() {
                                    return;
                                }
                                let cmd = Command::ImportDbeaver {
                                    files: pr.files.clone(),
                                    only: pr.chosen.iter().cloned().collect(),
                                };
                                this.core.send(cmd);
                                this.dismiss(w, cx);
                            })),
                        ),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::store::EnvironmentLabel;

    fn cand(key: &str, exists: bool, problem: Option<&str>) -> DbeaverCandidate {
        DbeaverCandidate {
            key: key.into(),
            name: key.into(),
            engine: None,
            driver: "postgresql".into(),
            address: String::new(),
            folder: Some("Prod".into()),
            environment: EnvironmentLabel::Production,
            password: true,
            via: None,
            exists,
            problem: problem.map(Into::into),
        }
    }

    #[test]
    fn ticks_only_new_importable_connections() {
        let pr = DbeaverImportPreview::new(
            vec!["/x/data-sources.json".into()],
            vec![
                cand("a", false, None),
                cand("b", true, None),
                cand("c", false, Some("no Switchyard driver for clickhouse")),
            ],
        );
        assert_eq!(pr.chosen, HashSet::from(["a".to_owned()]));
        assert_eq!(
            summary(&pr),
            "/x/data-sources.json · 3 connections, 1 can be imported"
        );
        assert_eq!(status(&pr.connections[0]), "Prod · Production · password");
        assert_eq!(status(&pr.connections[1]), "already saved");
    }
}
