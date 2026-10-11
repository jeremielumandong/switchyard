//! A form for a feature flag's conditions: percentage, time window and targeting filters
//! (Microsoft.FeatureManagement's built-in ones), and whether any or all must match.
//! Other filters are kept as they are; the JSON editor stays available for them.

use serde_json::{Map, Value as Json};

use super::*;

/// Built-in filter names.
pub const PERCENTAGE: &str = "Microsoft.Percentage";
/// Time window filter.
pub const TIME_WINDOW: &str = "Microsoft.TimeWindow";
/// Targeting filter.
pub const TARGETING: &str = "Microsoft.Targeting";

/// Fields of a built-in filter: (label, placeholder).
pub fn fields(name: &str) -> &'static [(&'static str, &'static str)] {
    match name {
        PERCENTAGE => &[("Percentage of requests", "50")],
        TIME_WINDOW => &[
            ("Start (empty: now)", "Wed, 01 Jan 2026 09:00:00 GMT"),
            ("End (empty: no end)", "Fri, 31 Dec 2026 18:00:00 GMT"),
        ],
        TARGETING => &[
            ("Users", "alice@contoso.com, bob@contoso.com"),
            ("Groups (name=percent)", "beta-testers=100, staff=50"),
            ("Everyone else (percent)", "0"),
            ("Excluded users", "carol@contoso.com"),
            ("Excluded groups", "contractors"),
        ],
        _ => &[],
    }
}

/// Title shown for a filter.
pub fn title(name: &str) -> &str {
    match name {
        PERCENTAGE => "Percentage",
        TIME_WINDOW => "Time window",
        TARGETING => "Targeting",
        other => other,
    }
}

/// One filter in the form.
pub struct FilterEdit {
    /// Filter name.
    pub name: String,
    /// One input per [`fields`] entry.
    pub inputs: Vec<Entity<InputState>>,
    /// Its parameters as loaded (kept for fields the form does not show).
    pub raw: Json,
}

/// The conditions form.
#[derive(Default)]
pub struct FlagForm {
    /// Filters in order.
    pub filters: Vec<FilterEdit>,
    /// All filters must match (`requirement_type: All`), else any.
    pub require_all: bool,
}

fn list(v: Option<&Json>) -> String {
    v.and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Json::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn number(v: Option<&Json>) -> String {
    match v {
        Some(Json::Number(n)) => n.to_string(),
        Some(Json::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// The text of each field of filter `name` from its parameters.
pub fn field_values(name: &str, p: &Json) -> Vec<String> {
    match name {
        PERCENTAGE => vec![number(p.get("Value"))],
        TIME_WINDOW => vec![
            p.get("Start")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_owned(),
            p.get("End").and_then(Json::as_str).unwrap_or("").to_owned(),
        ],
        TARGETING => {
            let a = p.get("Audience").cloned().unwrap_or(Json::Null);
            let groups = a
                .get("Groups")
                .and_then(Json::as_array)
                .map(|g| {
                    g.iter()
                        .map(|g| {
                            format!(
                                "{}={}",
                                g.get("Name").and_then(Json::as_str).unwrap_or(""),
                                number(g.get("RolloutPercentage"))
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            vec![
                list(a.get("Users")),
                groups,
                number(a.get("DefaultRolloutPercentage")),
                list(a.pointer("/Exclusion/Users")),
                list(a.pointer("/Exclusion/Groups")),
            ]
        }
        _ => Vec::new(),
    }
}

fn percent(s: &str, what: &str) -> Result<Json, String> {
    let s = s.trim().trim_end_matches('%').trim();
    if s.is_empty() {
        return Ok(json!(0));
    }
    match s.parse::<f64>() {
        Ok(n) if (0.0..=100.0).contains(&n) => Ok(if n.fract() == 0.0 {
            json!(n as i64)
        } else {
            json!(n)
        }),
        _ => Err(format!("{what}: enter a percentage from 0 to 100")),
    }
}

fn names(s: &str) -> Json {
    Json::Array(
        s.split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(|n| json!(n))
            .collect(),
    )
}

/// Parameters of filter `name` from its field texts, over `raw` (kept fields).
pub fn parameters(name: &str, values: &[String], raw: &Json) -> Result<Json, String> {
    let mut p = raw.as_object().cloned().unwrap_or_default();
    let v = |i: usize| values.get(i).map(|s| s.trim()).unwrap_or("");
    match name {
        PERCENTAGE => {
            p.insert("Value".into(), percent(v(0), "Percentage")?);
        }
        TIME_WINDOW => {
            for (i, k) in ["Start", "End"].iter().enumerate() {
                if v(i).is_empty() {
                    p.remove(*k);
                } else {
                    p.insert((*k).into(), json!(v(i)));
                }
            }
            if !p.contains_key("Start") && !p.contains_key("End") {
                return Err("Time window: enter a start, an end or both".into());
            }
        }
        TARGETING => {
            let mut groups = Vec::new();
            for g in v(1).split(',').map(str::trim).filter(|g| !g.is_empty()) {
                let (n, pct) = g.split_once('=').unwrap_or((g, "100"));
                groups.push(json!({
                    "Name": n.trim(),
                    "RolloutPercentage": percent(pct, &format!("Group {}", n.trim()))?,
                }));
            }
            let mut audience: Map<String, Json> = raw
                .get("Audience")
                .and_then(Json::as_object)
                .cloned()
                .unwrap_or_default();
            audience.insert("Users".into(), names(v(0)));
            audience.insert("Groups".into(), Json::Array(groups));
            audience.insert(
                "DefaultRolloutPercentage".into(),
                percent(v(2), "Everyone else")?,
            );
            audience.insert(
                "Exclusion".into(),
                json!({ "Users": names(v(3)), "Groups": names(v(4)) }),
            );
            p.insert("Audience".into(), Json::Object(audience));
        }
        _ => {}
    }
    Ok(Json::Object(p))
}

/// `value` (a flag's JSON) with its conditions replaced by `filters` (name, parameters).
pub fn with_conditions(
    value: &str,
    filters: Vec<(String, Json)>,
    require_all: bool,
) -> Result<String, String> {
    let mut j: Json =
        serde_json::from_str(value).map_err(|e| format!("The flag's JSON is not valid: {e}"))?;
    let o = j
        .as_object_mut()
        .ok_or("The flag's JSON is not an object")?;
    let mut conditions = o
        .get("conditions")
        .and_then(Json::as_object)
        .cloned()
        .unwrap_or_default();
    conditions.insert(
        "client_filters".into(),
        Json::Array(
            filters
                .into_iter()
                .map(|(name, parameters)| json!({ "name": name, "parameters": parameters }))
                .collect(),
        ),
    );
    if require_all {
        conditions.insert("requirement_type".into(), json!("All"));
    } else {
        conditions.remove("requirement_type");
    }
    o.insert("conditions".into(), Json::Object(conditions));
    serde_json::to_string(&j).map_err(|e| e.to_string())
}

impl FlagForm {
    /// The form for a flag's JSON (empty when it is not valid JSON).
    pub fn load(value: &str, window: &mut Window, cx: &mut Context<CloudTab>) -> Self {
        let j: Json = serde_json::from_str(value).unwrap_or(Json::Null);
        let filters = j
            .pointer("/conditions/client_filters")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|f| {
                let name = f
                    .get("name")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let raw = f.get("parameters").cloned().unwrap_or(json!({}));
                FilterEdit::new(name, raw, window, cx)
            })
            .collect();
        Self {
            filters,
            require_all: j
                .pointer("/conditions/requirement_type")
                .and_then(Json::as_str)
                .is_some_and(|r| r.eq_ignore_ascii_case("all")),
        }
    }

    /// `value` with the form's conditions.
    pub fn apply(&self, value: &str, cx: &Context<CloudTab>) -> Result<String, String> {
        let mut filters = Vec::new();
        for f in &self.filters {
            let values: Vec<String> = f
                .inputs
                .iter()
                .map(|i| i.read(cx).value().to_string())
                .collect();
            filters.push((f.name.clone(), parameters(&f.name, &values, &f.raw)?));
        }
        with_conditions(value, filters, self.require_all)
    }
}

impl FilterEdit {
    /// A filter with its fields filled from `raw`.
    pub fn new(name: String, raw: Json, window: &mut Window, cx: &mut Context<CloudTab>) -> Self {
        let values = field_values(&name, &raw);
        let inputs = fields(&name)
            .iter()
            .enumerate()
            .map(|(i, (_, ph))| {
                let v = values.get(i).cloned().unwrap_or_default();
                cx.new(|cx| {
                    let mut s = InputState::new(window, cx).placeholder(*ph);
                    s.set_value(v, window, cx);
                    s
                })
            })
            .collect();
        Self { name, inputs, raw }
    }
}

impl CloudTab {
    /// The flag's JSON as the form or the JSON editor has it.
    pub(super) fn flag_value(&self, base: &str, cx: &Context<Self>) -> Result<String, String> {
        if self.flag_json {
            Ok(base.to_owned())
        } else {
            self.flag_form.apply(base, cx)
        }
    }

    /// Switch between the conditions form and the flag's JSON.
    pub(super) fn toggle_flag_json(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.value.read(cx).value().to_string();
        if self.flag_json {
            if serde_json::from_str::<serde_json::Value>(&text).is_err() {
                self.status = Some((false, "Fix the JSON first".into()));
                cx.notify();
                return;
            }
            self.flag_form = FlagForm::load(&text, window, cx);
            self.flag_json = false;
        } else {
            match self.flag_form.apply(&text, cx) {
                Ok(v) => {
                    let v = pretty_json(&v);
                    self.value.update(cx, |e, cx| {
                        e.set_highlighter("json", cx);
                        e.set_value(v, window, cx);
                    });
                    self.flag_json = true;
                }
                Err(e) => self.status = Some((false, e)),
            }
        }
        cx.notify();
    }

    pub(super) fn add_filter(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let raw = if name == flags::PERCENTAGE {
            json!({ "Value": 50 })
        } else {
            json!({})
        };
        let f = FilterEdit::new(name.to_owned(), raw, window, cx);
        self.flag_form.filters.push(f);
        cx.notify();
    }

    /// The conditions form of a feature flag.
    pub(super) fn render_flag_form(
        &self,
        frozen: bool,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.entity().downgrade();
        let add = (!frozen).then(|| {
            Button::new("cl-add-filter")
                .outline()
                .small()
                .label("Add a filter")
                .dropdown_menu(move |mut menu, _, _| {
                    for name in [flags::PERCENTAGE, flags::TIME_WINDOW, flags::TARGETING] {
                        let t = this.clone();
                        menu = menu.item(PopupMenuItem::new(flags::title(name)).on_click(
                            move |_, w, cx| {
                                let _ = t.update(cx, |t, cx| t.add_filter(name, w, cx));
                            },
                        ));
                    }
                    menu
                })
        });
        let this = cx.entity().downgrade();
        let req = |label: &'static str, all: bool| {
            let this = this.clone();
            let on: ui::OnClick = Box::new(move |_, _, cx| {
                let _ = this.update(cx, |t, cx| {
                    t.flag_form.require_all = all;
                    cx.notify();
                });
            });
            (
                SharedString::from(label),
                self.flag_form.require_all == all,
                on,
            )
        };
        let cards: Vec<AnyElement> = self
            .flag_form
            .filters
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let fields = flags::fields(&f.name);
                Self::panel(p)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(rpx(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(ts::BODY)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(flags::title(&f.name).to_owned()),
                            )
                            .when(!frozen, |d| {
                                d.child(
                                    ui::button(("cl-filter-rm", i), "Remove", Kind::Ghost, p)
                                        .on_click(cx.listener(move |t, _, _, cx| {
                                            if i < t.flag_form.filters.len() {
                                                t.flag_form.filters.remove(i);
                                            }
                                            cx.notify();
                                        })),
                                )
                            }),
                    )
                    .when(fields.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(ts::SMALL)
                                .text_color(p.fg3)
                                .child("A custom filter: its parameters are kept; use Edit JSON to change them."),
                        )
                    })
                    .child(
                        div().grid().grid_cols(2).gap(rpx(8.)).children(
                            fields.iter().zip(&f.inputs).map(|((label, _), input)| {
                                Self::field(label, Self::boxed(input, false, frozen, p), p)
                            }),
                        ),
                    )
                    .into_any_element()
            })
            .collect();
        let n = self.flag_form.filters.len();
        div()
            .id("cl-flag-form")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(rpx(8.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .text_size(ts::BODY)
                    .text_color(p.fg2)
                    .child(if n == 0 {
                        "No filters: when enabled, the flag is on for everyone.".to_owned()
                    } else {
                        "On when".to_owned()
                    })
                    .when(n > 1, |d| {
                        d.child(ui::segmented(
                            "cl-req",
                            vec![
                                req("any filter matches", false),
                                req("all filters match", true),
                            ],
                            20.,
                            p,
                        ))
                    })
                    .when(n == 1, |d| d.child("this filter matches:"))
                    .child(div().flex_1())
                    .children(add),
            )
            .children(cards)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targeting_round_trip() {
        let raw = json!({"Audience": {"Users": ["a@x"], "Groups": [{"Name": "beta", "RolloutPercentage": 50}],
            "DefaultRolloutPercentage": 10, "Exclusion": {"Users": [], "Groups": ["ext"]}}});
        let values = field_values(TARGETING, &raw);
        assert_eq!(values, ["a@x", "beta=50", "10", "", "ext"]);
        let back = parameters(TARGETING, &values, &raw).unwrap();
        assert_eq!(back, raw);
        let edited = parameters(
            TARGETING,
            &[
                "a, b".into(),
                "g1=20, g2".into(),
                "".into(),
                "".into(),
                "".into(),
            ],
            &raw,
        )
        .unwrap();
        assert_eq!(edited.pointer("/Audience/Users"), Some(&json!(["a", "b"])));
        assert_eq!(
            edited.pointer("/Audience/Groups/1"),
            Some(&json!({"Name": "g2", "RolloutPercentage": 100}))
        );
        assert!(parameters(TARGETING, &["".into(), "g=150".into()], &raw).is_err());
    }

    #[test]
    fn percentage_and_time_window() {
        let p = parameters(PERCENTAGE, &["25%".into()], &json!({})).unwrap();
        assert_eq!(p, json!({"Value": 25}));
        assert!(parameters(PERCENTAGE, &["lots".into()], &json!({})).is_err());
        let t = parameters(
            TIME_WINDOW,
            &["".into(), "Fri, 31 Dec 2027 18:00:00 GMT".into()],
            &json!({"Start": "x"}),
        )
        .unwrap();
        assert_eq!(t, json!({"End": "Fri, 31 Dec 2027 18:00:00 GMT"}));
        assert!(parameters(TIME_WINDOW, &["".into(), "".into()], &json!({})).is_err());
    }

    #[test]
    fn conditions_replace_filters_and_keep_the_rest() {
        let v =
            r#"{"id":"B","enabled":true,"conditions":{"client_filters":[{"name":"x"}]},"other":1}"#;
        let out = with_conditions(v, vec![(PERCENTAGE.into(), json!({"Value": 5}))], true).unwrap();
        let j: Json = serde_json::from_str(&out).unwrap();
        assert_eq!(j["other"], 1);
        assert_eq!(j["conditions"]["requirement_type"], "All");
        assert_eq!(
            j["conditions"]["client_filters"][0]["parameters"]["Value"],
            5
        );
        let any = with_conditions(&out, vec![], false).unwrap();
        assert!(!any.contains("requirement_type"));
    }
}
