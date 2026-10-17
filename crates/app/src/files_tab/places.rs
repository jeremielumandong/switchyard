//! The places column: quick access (home and its usual folders, pinned folders), this
//! computer's disks, then Hosts and storage connections. Clicking a place shows it in the
//! active pane.

use std::path::{Path, PathBuf};

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px, svg,
};
use switchyard_core::FsRef;

use super::{FilesTab, base_name};
use crate::appearance::{rpx, ts};
use crate::theme::Palette;

/// One entry of the places column.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    /// Shown name.
    pub label: String,
    /// Icon path.
    pub icon: &'static str,
    /// File system.
    pub fs: FsRef,
    /// Folder (`None`: the file system's home).
    pub path: Option<PathBuf>,
}

impl Place {
    fn local(label: &str, icon: &'static str, path: PathBuf) -> Self {
        Self {
            label: label.to_owned(),
            icon,
            fs: FsRef::Local,
            path: Some(path),
        }
    }
}

/// Quick access folders and disks of this computer. Touches the disk: call it off the UI
/// thread.
pub fn local_places() -> (Vec<Place>, Vec<Place>) {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|h| h.is_dir());
    let mut quick = Vec::new();
    if let Some(home) = &home {
        quick.push(Place::local("Home", "icons/swy-house.svg", home.clone()));
        for (name, icon) in [
            ("Desktop", "icons/swy-monitor.svg"),
            ("Documents", "icons/swy-file-text.svg"),
            ("Downloads", "icons/swy-download.svg"),
            ("Pictures", "icons/swy-image.svg"),
        ] {
            let dir = home.join(name);
            if dir.is_dir() {
                quick.push(Place::local(name, icon, dir));
            }
        }
    }
    let disks = if cfg!(windows) {
        (b'A'..=b'Z')
            .map(|c| format!("{}:\\", c as char))
            .filter(|d| Path::new(d).is_dir())
            .map(|d| {
                let label = format!("Local Disk ({})", d.trim_end_matches('\\'));
                Place::local(&label, "icons/swy-hard-drive.svg", PathBuf::from(d))
            })
            .collect()
    } else {
        let mut v = vec![Place::local(
            "File system",
            "icons/swy-hard-drive.svg",
            PathBuf::from("/"),
        )];
        if cfg!(target_os = "macos") && Path::new("/Volumes").is_dir() {
            v.push(Place::local(
                "Volumes",
                "icons/swy-hard-drive.svg",
                PathBuf::from("/Volumes"),
            ));
        }
        v
    };
    (quick, disks)
}

impl FilesTab {
    /// The places column left of the panes.
    pub(super) fn render_places(&mut self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let pane = &self.panes[self.active];
        let (cur_fs, cur_path) = (pane.fs.clone(), pane.path.clone());
        let mut quick = self.local_places.0.clone();
        quick.extend(self.pinned.iter().map(|s| {
            let path = PathBuf::from(s);
            Place {
                label: base_name(&path),
                icon: "icons/swy-star.svg",
                fs: FsRef::Local,
                path: Some(path),
            }
        }));
        let servers: Vec<Place> = self
            .sources
            .iter()
            .filter(|(fs, ..)| matches!(fs, FsRef::Host(_)))
            .map(|(fs, name, _)| Place {
                label: name.clone(),
                icon: "icons/swy-server.svg",
                fs: fs.clone(),
                path: None,
            })
            .collect();
        let storage: Vec<Place> = self
            .sources
            .iter()
            .filter(|(fs, ..)| matches!(fs, FsRef::Conn(_)))
            .map(|(fs, name, _)| Place {
                label: name.clone(),
                icon: "icons/swy-cloud.svg",
                fs: fs.clone(),
                path: None,
            })
            .collect();
        let sections = [
            ("Quick access", quick),
            ("This computer", self.local_places.1.clone()),
            ("Servers", servers),
            ("Storage", storage),
        ];
        let mut col = div()
            .id("files-places")
            .w(rpx(190.))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .py(rpx(6.))
            .bg(p.panel)
            .border_r_1()
            .border_color(p.bd);
        let mut n = 0usize;
        for (title, items) in sections {
            if items.is_empty() {
                continue;
            }
            col = col.child(
                div()
                    .px(rpx(12.))
                    .pt(rpx(8.))
                    .pb(rpx(3.))
                    .text_size(ts::SMALL)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.fg3)
                    .child(title),
            );
            for place in items {
                n += 1;
                let on = place.fs == cur_fs
                    && match &place.path {
                        Some(path) => cur_path.as_ref() == Some(path),
                        None => true,
                    };
                let pinned = place.icon == "icons/swy-star.svg";
                let (fs, path) = (place.fs.clone(), place.path.clone());
                let unpin = place.path.clone();
                col = col.child(
                    div()
                        .id(("files-place", n))
                        .mx(rpx(6.))
                        .h(rpx(26.))
                        .px(rpx(8.))
                        .flex()
                        .items_center()
                        .gap(rpx(8.))
                        .rounded(px(5.))
                        .text_size(ts::UI)
                        .cursor_pointer()
                        .when(on, |d| d.bg(p.sel))
                        .when(!on, |d| d.hover(|s| s.bg(p.hover)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            window.focus(&this.focus, cx);
                            let ix = this.active;
                            this.set_source(ix, fs.clone(), path.clone(), cx);
                        }))
                        .when(pinned, |d| {
                            d.on_mouse_down(
                                gpui_kit::MouseButton::Right,
                                cx.listener(move |this, _, _, cx| {
                                    if let Some(p) = unpin.clone() {
                                        this.toggle_pin(p, cx);
                                    }
                                }),
                            )
                            .tooltip(|w, cx| {
                                gpui_kit::component::tooltip::Tooltip::new(
                                    "Pinned · right click to unpin",
                                )
                                .build(w, cx)
                            })
                        })
                        .child(
                            svg()
                                .path(place.icon)
                                .flex_none()
                                .size(rpx(14.))
                                .text_color(if on { p.acc } else { p.fg3 }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(if on { p.fg } else { p.fg2 })
                                .child(SharedString::from(place.label)),
                        ),
                );
            }
        }
        col.into_any_element()
    }
}
