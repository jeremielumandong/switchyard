//! Two labels side by side (App Configuration): which keys differ, exist only on one
//! side, or match, and copying settings across.

use std::collections::BTreeMap;

use super::*;

/// How a key compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Same value, content type and tags.
    Same,
    /// Both labels have it, with differences.
    Differs,
    /// Only the left label has it.
    LeftOnly,
    /// Only the right label has it.
    RightOnly,
}

impl State {
    /// Short text for the row.
    pub fn text(self) -> &'static str {
        match self {
            State::Same => "same",
            State::Differs => "differs",
            State::LeftOnly => "left only",
            State::RightOnly => "right only",
        }
    }
}

/// One key of the comparison.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffRow {
    /// Key.
    pub key: String,
    /// Left label's setting.
    pub left: Option<KvItem>,
    /// Right label's setting.
    pub right: Option<KvItem>,
    /// How they compare.
    pub state: State,
}

/// The settings of two labels compared by key, in key order.
pub fn diff(left: &[KvItem], right: &[KvItem]) -> Vec<DiffRow> {
    let mut by_key: BTreeMap<&str, (Option<&KvItem>, Option<&KvItem>)> = BTreeMap::new();
    for i in left {
        by_key.entry(&i.key).or_default().0 = Some(i);
    }
    for i in right {
        by_key.entry(&i.key).or_default().1 = Some(i);
    }
    by_key
        .into_iter()
        .map(|(key, (l, r))| {
            let state = match (l, r) {
                (Some(a), Some(b))
                    if a.value == b.value
                        && a.content_type == b.content_type
                        && a.tags == b.tags =>
                {
                    State::Same
                }
                (Some(_), Some(_)) => State::Differs,
                (Some(_), None) => State::LeftOnly,
                _ => State::RightOnly,
            };
            DiffRow {
                key: key.to_owned(),
                left: l.cloned(),
                right: r.cloned(),
                state,
            }
        })
        .collect()
}

/// Writes copying `from`'s settings of `keys` to label `to` (overwriting).
pub fn copy_edits(from: &[KvItem], keys: &HashSet<String>, to: Option<&str>) -> Vec<CloudEdit> {
    from.iter()
        .filter(|i| keys.contains(&i.key))
        .map(|i| {
            CloudEdit::Put(KvWrite {
                key: i.key.clone(),
                label: to.map(str::to_owned),
                value: i.value.clone().unwrap_or_default(),
                content_type: i.content_type.clone(),
                tags: i.tags.clone(),
                ..KvWrite::default()
            })
        })
        .collect()
}

/// One side of the comparison while it loads.
#[derive(Clone, Debug, Default)]
pub struct Side {
    /// Label (`None` is the null label).
    pub label: Option<String>,
    /// Settings loaded so far.
    pub items: Vec<KvItem>,
    /// Page request in flight.
    pub request: Option<RequestId>,
    /// Why loading failed.
    pub error: Option<String>,
}

/// The comparison view.
#[derive(Clone, Debug, Default)]
pub struct Compare {
    /// Left label.
    pub left: Side,
    /// Right label.
    pub right: Side,
    /// Hide matching keys.
    pub only_differences: bool,
    /// Keys ticked for copying.
    pub picked: HashSet<String>,
    /// Rows, rebuilt when a side finishes.
    pub rows: Vec<DiffRow>,
}

impl Compare {
    /// Both sides are loaded.
    pub fn ready(&self) -> bool {
        self.left.request.is_none() && self.right.request.is_none()
    }

    /// Rows to show.
    pub fn visible(&self) -> Vec<DiffRow> {
        self.rows
            .iter()
            .filter(|r| !self.only_differences || r.state != State::Same)
            .cloned()
            .collect()
    }

    /// Rebuild the rows.
    pub fn refresh(&mut self) {
        self.rows = diff(&self.left.items, &self.right.items);
        let keys: HashSet<&String> = self.rows.iter().map(|r| &r.key).collect();
        self.picked.retain(|k| keys.contains(k));
    }
}

impl CloudTab {
    pub(super) fn open_compare(&mut self, cx: &mut Context<Self>) {
        let current = Self::text(&self.label_filter, cx);
        // Named labels first; the null label only when it is all there is.
        let named: Vec<String> = self.labels.iter().flatten().cloned().collect();
        let left = match current.as_str() {
            "" => named.first().cloned(),
            "-" => None,
            l => Some(l.to_owned()),
        };
        let right = named.iter().find(|l| Some(*l) != left.as_ref()).cloned();
        self.start_compare(left, right, cx);
    }

    pub(super) fn start_compare(
        &mut self,
        left: Option<String>,
        right: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let keep = self
            .compare
            .as_ref()
            .map(|c| (c.only_differences, c.picked.clone()));
        let mut c = Compare::default();
        if let Some((only, picked)) = keep {
            c.only_differences = only;
            c.picked = picked;
        } else {
            c.only_differences = true;
        }
        c.left.label = left;
        c.right.label = right;
        self.compare = Some(c);
        self.import = None;
        self.compare_page(true, None, cx);
        self.compare_page(false, None, cx);
    }

    pub(super) fn compare_page(
        &mut self,
        left: bool,
        cursor: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let request = next_id();
        let Some(c) = self.compare.as_mut() else {
            return;
        };
        let side = if left { &mut c.left } else { &mut c.right };
        side.request = Some(request);
        if cursor.is_none() {
            side.items.clear();
            side.error = None;
        }
        let label = label_query(&side.label.clone().unwrap_or_else(|| "-".into()));
        let mut query = self.query(Some(label), cursor, cx);
        query.key = Self::text(&self.filter, cx);
        self.core.send(Command::CloudList {
            session: self.session,
            request,
            query,
        });
        cx.notify();
    }

    pub(super) fn on_compare_page(
        &mut self,
        request: RequestId,
        result: Result<KvPage, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(c) = self.compare.as_mut() else {
            return;
        };
        let left = if c.left.request == Some(request) {
            true
        } else if c.right.request == Some(request) {
            false
        } else {
            return;
        };
        let side = if left { &mut c.left } else { &mut c.right };
        side.request = None;
        let mut next = None;
        match result {
            Ok(page) => {
                side.items
                    .extend(page.items.into_iter().filter(|i| !is_feature_flag(&i.key)));
                next = page.next;
            }
            Err(e) => side.error = Some(e),
        }
        if next.is_some() {
            self.compare_page(left, next, cx);
        } else if c.ready() {
            c.refresh();
        }
        cx.notify();
    }

    /// Copy the ticked keys from one side of the comparison to the other.
    pub(super) fn compare_copy(&mut self, to_right: bool, cx: &mut Context<Self>) {
        let Some(c) = &self.compare else { return };
        let (from, to) = if to_right {
            (&c.left, &c.right)
        } else {
            (&c.right, &c.left)
        };
        let edits = compare::copy_edits(&from.items, &c.picked, to.label.as_deref());
        if edits.is_empty() {
            self.status = Some((false, "Tick keys the source label has".into()));
            cx.notify();
            return;
        }
        let n = edits.len();
        let prompt = format!(
            "Copy {n} setting{} from {} to {}? Settings already there are overwritten.",
            plural(n),
            label_text(from.label.as_deref()),
            label_text(to.label.as_deref())
        );
        self.ask(prompt, "Copy", CloudEdit::Batch(edits), true, cx);
    }

    /// Two labels side by side.
    pub(super) fn render_compare(
        &self,
        c: &Compare,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let labels = self.labels.clone();
        let side_picker = |left: bool, current: Option<String>| {
            let this = cx.entity().downgrade();
            let labels = labels.clone();
            Button::new(if left { "cl-cmp-left" } else { "cl-cmp-right" })
                .outline()
                .small()
                .label(label_text(current.as_deref()))
                .dropdown_menu(move |mut menu, _, _| {
                    for l in &labels {
                        let t = this.clone();
                        let l = l.clone();
                        menu = menu.item(
                            PopupMenuItem::new(label_text(l.as_deref()))
                                .checked(l == current)
                                .on_click(move |_, _, cx| {
                                    let l = l.clone();
                                    let _ = t.update(cx, |t, cx| {
                                        let Some(c) = &t.compare else { return };
                                        let (a, b) = if left {
                                            (l, c.right.label.clone())
                                        } else {
                                            (c.left.label.clone(), l)
                                        };
                                        t.start_compare(a, b, cx);
                                    });
                                }),
                        );
                    }
                    menu
                })
        };
        let rows = c.visible();
        let counts = |s: compare::State| c.rows.iter().filter(|r| r.state == s).count();
        let summary = if !c.ready() {
            "Loading…".to_owned()
        } else {
            format!(
                "{} differ · {} only left · {} only right · {} same",
                counts(compare::State::Differs),
                counts(compare::State::LeftOnly),
                counts(compare::State::RightOnly),
                counts(compare::State::Same)
            )
        };
        let errors: Vec<String> = [&c.left.error, &c.right.error]
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        let read_only = self.read_only();
        let picked = c.picked.clone();
        let p2 = *p;
        let rows2 = rows.clone();
        let list = uniform_list(
            "cl-compare",
            rows2.len(),
            cx.processor(move |_this, range: std::ops::Range<usize>, _w, cx| {
                let p = &p2;
                range
                    .map(|i| {
                        let r = &rows2[i];
                        let on = picked.contains(&r.key);
                        let key = r.key.clone();
                        let cell = |v: &Option<KvItem>| {
                            let text = match v {
                                Some(it) => it
                                    .value
                                    .clone()
                                    .unwrap_or_default()
                                    .chars()
                                    .take(200)
                                    .collect::<String>()
                                    .replace('\n', " "),
                                None => "—".into(),
                            };
                            div()
                                .flex_1()
                                .min_w_0()
                                .px(rpx(8.))
                                .text_size(ts::BODY)
                                .font_family(MONO)
                                .text_color(if v.is_some() { p.fg } else { p.fg3 })
                                .truncate()
                                .child(text)
                        };
                        let state_color = match r.state {
                            compare::State::Same => p.fg3,
                            compare::State::Differs => p.stg,
                            _ => p.acc,
                        };
                        div()
                            .id(("cl-cmp-row", i))
                            .w_full()
                            .h(rpx(32.))
                            .flex()
                            .items_center()
                            .px(rpx(10.))
                            .border_b_1()
                            .border_color(p.line)
                            .when(!read_only, |d| {
                                d.cursor_pointer()
                                    .hover(|s| s.bg(p.hover))
                                    .on_click(cx.listener(move |t, _, _, cx| {
                                        if let Some(c) = t.compare.as_mut()
                                            && !c.picked.remove(&key)
                                        {
                                            c.picked.insert(key.clone());
                                        }
                                        cx.notify();
                                    }))
                                    .child(
                                        div()
                                            .flex_none()
                                            .mr(rpx(8.))
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
                                            .child(if on { "✓" } else { "" }),
                                    )
                            })
                            .child(
                                div()
                                    .w(relative(0.3))
                                    .flex_none()
                                    .min_w_0()
                                    .text_size(ts::BODY)
                                    .font_family(MONO)
                                    .truncate()
                                    .child(r.key.clone()),
                            )
                            .child(cell(&r.left))
                            .child(cell(&r.right))
                            .child(
                                div()
                                    .w(rpx(72.))
                                    .flex_none()
                                    .text_size(ts::SMALL)
                                    .text_color(state_color)
                                    .child(r.state.text()),
                            )
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1();
        let n_picked = c.picked.len();
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(rpx(8.))
            .p(rpx(14.))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .child(
                        div()
                            .text_size(ts::TITLE)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Compare labels"),
                    )
                    .child(side_picker(true, c.left.label.clone()))
                    .child(div().text_size(ts::BODY).text_color(p.fg3).child("with"))
                    .child(side_picker(false, c.right.label.clone()))
                    .child(
                        ui::checkbox("cl-cmp-diff", c.only_differences, "Differences only", p)
                            .on_click(cx.listener(|t, _, _, cx| {
                                if let Some(c) = t.compare.as_mut() {
                                    c.only_differences = !c.only_differences;
                                }
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1())
                    .child(div().text_size(ts::SMALL).text_color(p.fg3).child(summary)),
            )
            .children(
                errors
                    .into_iter()
                    .map(|e| div().text_size(ts::BODY).text_color(p.prod).child(e)),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(rpx(10.))
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .when(!read_only, |d| d.child(div().w(rpx(22.)).flex_none()))
                    .child(div().w(relative(0.3)).flex_none().child("Key"))
                    .child(
                        div()
                            .flex_1()
                            .px(rpx(8.))
                            .child(label_text(c.left.label.as_deref())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .px(rpx(8.))
                            .child(label_text(c.right.label.as_deref())),
                    )
                    .child(div().w(rpx(72.)).flex_none()),
            )
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
                    .when(rows.is_empty() && c.ready(), |d| {
                        d.child(
                            div()
                                .p(rpx(12.))
                                .text_size(ts::BODY)
                                .text_color(p.fg3)
                                .child("No differences."),
                        )
                    })
                    .child(list),
            )
            .when(!read_only, |d| {
                d.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(rpx(8.))
                        .child(
                            div()
                                .text_size(ts::BODY)
                                .text_color(p.fg2)
                                .child(format!("{n_picked} ticked")),
                        )
                        .child(
                            ui::button("cl-cmp-tick", "Tick all shown", Kind::Ghost, p).on_click(
                                cx.listener(move |t, _, _, cx| {
                                    if let Some(c) = t.compare.as_mut() {
                                        let keys: Vec<String> =
                                            c.visible().into_iter().map(|r| r.key).collect();
                                        c.picked.extend(keys);
                                    }
                                    cx.notify();
                                }),
                            ),
                        )
                        .child(div().flex_1())
                        .child(
                            ui::button(
                                "cl-cmp-to-left",
                                format!("← Copy to {}", label_text(c.left.label.as_deref())),
                                Kind::Secondary,
                                p,
                            )
                            .on_click(cx.listener(|t, _, _, cx| t.compare_copy(false, cx))),
                        )
                        .child(
                            ui::button(
                                "cl-cmp-to-right",
                                format!("Copy to {} →", label_text(c.right.label.as_deref())),
                                Kind::Secondary,
                                p,
                            )
                            .on_click(cx.listener(|t, _, _, cx| t.compare_copy(true, cx))),
                        ),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, value: &str) -> KvItem {
        KvItem {
            key: key.into(),
            value: Some(value.into()),
            ..KvItem::default()
        }
    }

    #[test]
    fn compares_by_key() {
        let left = [item("a", "1"), item("b", "2"), item("c", "3")];
        let mut right = vec![item("b", "2"), item("c", "x"), item("d", "4")];
        let rows = diff(&left, &right);
        let states: Vec<_> = rows.iter().map(|r| (r.key.as_str(), r.state)).collect();
        assert_eq!(
            states,
            [
                ("a", State::LeftOnly),
                ("b", State::Same),
                ("c", State::Differs),
                ("d", State::RightOnly),
            ]
        );
        right[0].content_type = Some("application/json".into());
        assert_eq!(diff(&left, &right)[1].state, State::Differs);
    }

    #[test]
    fn copies_picked_keys_to_a_label() {
        let mut from = vec![item("a", "1"), item("b", "2")];
        from[0].tags.insert("t".into(), "v".into());
        let picked: HashSet<String> = ["a".to_owned()].into();
        let edits = copy_edits(&from, &picked, Some("prod"));
        assert_eq!(edits.len(), 1);
        let CloudEdit::Put(w) = &edits[0] else {
            panic!("a put")
        };
        assert_eq!((w.key.as_str(), w.label.as_deref()), ("a", Some("prod")));
        assert_eq!(w.tags["t"], "v");
        assert!(!w.create && w.etag.is_none());
    }
}
