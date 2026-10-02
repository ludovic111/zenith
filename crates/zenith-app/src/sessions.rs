//! Sessions & costs: every Claude Code and Codex session on this Mac (from their own logs,
//! read by the server's `zc-sessions`), with totals, today's and this week's spend, a chart of
//! sessions per day, the cost per project, and each session's resume command.

use std::time::Duration;

use gpui::prelude::*;
use gpui::{div, px, App, ClipboardItem, Context, Entity, FontWeight, Hsla, SharedString, Task, Window};
use serde_json::{json, Value};

use crate::assets::{Icon, MONO_FONT};
use crate::store::{self, Store};
use crate::theme::{radius, text, ActiveTheme};
use crate::ui::{caps_label, dot, icon, pill, spinner, Button, Variant};

pub struct SessionsView {
    store: Entity<Store>,
    data: Option<Value>,
    error: Option<String>,
    loading: bool,
    _refresh: Task<()>,
}

impl SessionsView {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let refresh = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(60)).await;
            if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                return;
            }
        });
        let mut this = Self {
            store: store::store(cx),
            data: None,
            error: None,
            loading: false,
            _refresh: refresh,
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        let task = self.store.update(cx, |s, cx| s.run_command("sessions.list", json!({"limit": 200}), cx));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(value) => {
                        this.data = Some(value);
                        this.error = None;
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

fn money(value: f64) -> String {
    if value >= 100. {
        format!("${value:.0}")
    } else {
        format!("${value:.2}")
    }
}

fn number(value: &Value, keys: &[&str]) -> f64 {
    keys.iter().find_map(|k| value.get(*k).and_then(Value::as_f64)).unwrap_or(0.)
}

fn stat(label: &str, value: String, cx: &App) -> gpui::Div {
    let c = cx.theme().colors.clone();
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .flex_1()
        .p(px(16.))
        .rounded(px(radius::LG))
        .bg(c.bg_sunken)
        .border_1()
        .border_color(c.line)
        .child(caps_label(label.to_owned(), cx))
        .child(
            div()
                .font_family(MONO_FONT)
                .text_size(px(text::XL))
                .font_weight(FontWeight::MEDIUM)
                .text_color(c.text)
                .child(SharedString::from(value)),
        )
}

impl Render for SessionsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = cx.theme().colors.clone();
        let Some(data) = self.data.clone() else {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .text_color(c.text_3)
                .child(match &self.error {
                    Some(error) => div().text_color(c.danger).child(SharedString::from(error.clone())).into_any_element(),
                    None => div()
                        .flex()
                        .gap(px(8.))
                        .child(spinner("sessions-loading", c.text_3, 14.))
                        .child("Reading the sessions…")
                        .into_any_element(),
                })
                .into_any_element();
        };
        let totals = data.get("totals").cloned().unwrap_or(Value::Null);
        let week = totals.get("week").cloned().unwrap_or(Value::Null);
        let sessions = data.get("sessions").and_then(Value::as_array).cloned().unwrap_or_default();
        let per_day = totals.get("perDay").and_then(Value::as_array).cloned().unwrap_or_default();
        let per_project = totals.get("perProject").and_then(Value::as_array).cloned().unwrap_or_default();
        let max_day = per_day
            .iter()
            .map(|d| d.get("claude").and_then(Value::as_u64).unwrap_or(0) + d.get("codex").and_then(Value::as_u64).unwrap_or(0))
            .max()
            .unwrap_or(1)
            .max(1);
        let claude_color = c.accent;
        let codex_color = Hsla { a: 0.55, ..c.text_2 };

        let chart = div()
            .flex()
            .items_end()
            .gap(px(4.))
            .h(px(120.))
            .px(px(16.))
            .pt(px(16.))
            .children(per_day.iter().map(|day| {
                let claude = day.get("claude").and_then(Value::as_u64).unwrap_or(0) as f32;
                let codex = day.get("codex").and_then(Value::as_u64).unwrap_or(0) as f32;
                let scale = 96. / max_day as f32;
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .justify_end()
                    .child(div().h(px(codex * scale)).bg(codex_color).rounded_t(px(2.)))
                    .child(div().h(px(claude * scale)).bg(claude_color))
            }));

        let rows = sessions.iter().take(120).map(|s| {
            let live = s.get("live").and_then(Value::as_bool).unwrap_or(false);
            let agent = s.get("agent").and_then(Value::as_str).unwrap_or("");
            let title = s.get("title").and_then(Value::as_str).unwrap_or("Untitled").to_owned();
            let project = s.get("project").and_then(|p| p.get("title")).and_then(Value::as_str).unwrap_or("").to_owned();
            let cost = s.get("costUSD").or_else(|| s.get("costUsd")).and_then(Value::as_f64);
            let model = s.get("model").and_then(Value::as_str).unwrap_or("").to_owned();
            let resume = s.get("resume").and_then(Value::as_str).unwrap_or("").to_owned();
            let id = s.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(16.))
                .py(px(9.))
                .border_b_1()
                .border_color(c.line)
                .child(dot(if live {
                    c.success
                } else if agent == "claude" {
                    claude_color
                } else {
                    codex_color
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().truncate().text_size(px(text::BASE)).text_color(c.text).child(SharedString::from(title)))
                        .child(
                            div().truncate().text_size(px(text::XS)).text_color(c.text_3).child(SharedString::from(
                                [if agent == "claude" { "Claude Code" } else { "Codex" }, &project, &model]
                                    .iter()
                                    .filter(|s| !s.is_empty())
                                    .cloned()
                                    .collect::<Vec<_>>()
                                    .join(" · "),
                            )),
                        ),
                )
                .when(live, |this| this.child(pill("Live", c.success, Hsla { a: 0.14, ..c.success })))
                .child(
                    div()
                        .w(px(70.))
                        .text_right()
                        .font_family(MONO_FONT)
                        .text_size(px(text::SM))
                        .text_color(c.text_2)
                        .child(SharedString::from(cost.map(money).unwrap_or_else(|| "—".into()))),
                )
                .child(
                    Button::new(SharedString::from(format!("resume-{id}")))
                        .icon(Icon::Copy)
                        .small()
                        .tooltip("Copy the resume command")
                        .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(resume.clone()))),
                )
        });

        div()
            .id("sessions-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(
                div().flex().justify_center().px(px(32.)).py(px(28.)).child(
                    div()
                        .w_full()
                        .max_w(px(920.))
                        .flex()
                        .flex_col()
                        .gap(px(20.))
                        .child(
                            div()
                                .flex()
                                .gap(px(12.))
                                .child(stat("Sessions", format!("{}", number(&totals, &["sessions"]) as u64), cx))
                                .child(stat("Live now", format!("{}", number(&totals, &["live"]) as u64), cx))
                                .child(stat("Today", format!("{}", number(&totals, &["today"]) as u64), cx))
                                .child(stat("Cost", money(number(&totals, &["costUSD", "costUsd"])), cx))
                                .child(stat("This week", money(number(&week, &["costUSD", "costUsd"])), cx)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .rounded(px(radius::LG))
                                .bg(c.bg_sunken)
                                .border_1()
                                .border_color(c.line)
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(12.))
                                        .px(px(16.))
                                        .pt(px(14.))
                                        .child(caps_label("Sessions per day", cx))
                                        .child(div().flex_1())
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(px(4.))
                                                .text_size(px(text::XS))
                                                .text_color(c.text_3)
                                                .child(dot(claude_color))
                                                .child("Claude Code"),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(px(4.))
                                                .text_size(px(text::XS))
                                                .text_color(c.text_3)
                                                .child(dot(codex_color))
                                                .child("Codex"),
                                        ),
                                )
                                .child(chart)
                                .child(div().h(px(14.))),
                        )
                        .when(!per_project.is_empty(), |this| {
                            this.child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .rounded(px(radius::LG))
                                    .bg(c.bg_sunken)
                                    .border_1()
                                    .border_color(c.line)
                                    .child(div().px(px(16.)).pt(px(14.)).pb(px(6.)).child(caps_label("By project", cx)))
                                    .children(per_project.iter().take(12).map(|p| {
                                        let title = p
                                            .get("project")
                                            .and_then(|x| x.get("title"))
                                            .and_then(Value::as_str)
                                            .unwrap_or("Elsewhere")
                                            .to_owned();
                                        div()
                                            .flex()
                                            .gap(px(12.))
                                            .px(px(16.))
                                            .py(px(6.))
                                            .text_size(px(text::BASE))
                                            .child(div().flex_1().truncate().text_color(c.text).child(SharedString::from(title)))
                                            .child(
                                                div()
                                                    .text_color(c.text_3)
                                                    .child(SharedString::from(format!("{} sessions", number(p, &["sessions"]) as u64))),
                                            )
                                            .child(
                                                div()
                                                    .w(px(80.))
                                                    .text_right()
                                                    .font_family(MONO_FONT)
                                                    .text_color(c.text_2)
                                                    .child(SharedString::from(money(number(p, &["costUSD", "costUsd"])))),
                                            )
                                    })),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .rounded(px(radius::LG))
                                .bg(c.bg_sunken)
                                .border_1()
                                .border_color(c.line)
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .px(px(16.))
                                        .pt(px(14.))
                                        .pb(px(6.))
                                        .child(caps_label("History", cx))
                                        .child(div().flex_1())
                                        .child(
                                            Button::new("refresh-sessions")
                                                .icon(Icon::Refresh)
                                                .small()
                                                .variant(Variant::Ghost)
                                                .tooltip("Read again")
                                                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                                        ),
                                )
                                .children(rows)
                                .when(sessions.is_empty(), |this| {
                                    this.child(
                                        div()
                                            .p(px(16.))
                                            .text_size(px(text::BASE))
                                            .text_color(c.text_3)
                                            .child("No session found in this Mac's Claude Code and Codex logs."),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .text_size(px(text::XS))
                                .text_color(c.text_3)
                                .child(icon(Icon::Info, c.text_3))
                                .child("Costs are estimates from the agents' own logs."),
                        ),
                ),
            )
            .into_any_element()
    }
}
