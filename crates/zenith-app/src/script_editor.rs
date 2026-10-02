//! "Add action": a project script (`project.saveScript`) from the header's button, as the web's
//! `ProjectScriptEditorDialog` asks for it: a name, the command it runs and its icon.

use gpui::prelude::*;
use gpui::{div, px, svg, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, SharedString, Subscription, Window};
use serde_json::json;
use zc_contracts::ProjectId;

use crate::assets::Icon;
use crate::store;
use crate::theme::{radius, ActiveTheme};
use crate::ui::controls::{outline_button, Part};
use crate::ui::text_area::{TextArea, TextAreaEvent};

pub enum ScriptEditorEvent {
    Dismissed,
    Saved,
}

/// The icons a script can have, in the web's order, with their wire names.
pub const SCRIPT_ICONS: [(&str, Icon, &str); 6] = [
    ("play", Icon::Play, "Run"),
    ("test", Icon::FlaskConical, "Test"),
    ("lint", Icon::ListChecks, "Lint"),
    ("configure", Icon::Wrench, "Configure"),
    ("build", Icon::Hammer, "Build"),
    ("debug", Icon::Bug, "Debug"),
];

pub struct ScriptEditor {
    project: ProjectId,
    name: Entity<TextArea>,
    command: Entity<TextArea>,
    icon: &'static str,
    error: Option<SharedString>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ScriptEditorEvent> for ScriptEditor {}

impl Focusable for ScriptEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ScriptEditor {
    pub fn new(project: ProjectId, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| TextArea::single_line(cx).with_placeholder("Dev server").with_font(14., 20.));
        let command = cx.new(|cx| TextArea::single_line(cx).with_placeholder("npm run dev").with_font(14., 20.));
        let on_field = |this: &mut Self, _: &Entity<TextArea>, event: &TextAreaEvent, window: &mut Window, cx: &mut Context<Self>| match event {
            TextAreaEvent::Submit => this.save(window, cx),
            TextAreaEvent::Cancel => cx.emit(ScriptEditorEvent::Dismissed),
            _ => {}
        };
        let subscriptions = vec![cx.subscribe_in(&name, window, on_field), cx.subscribe_in(&command, window, on_field)];
        Self {
            project,
            name,
            command,
            icon: "play",
            error: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &App) {
        self.name.read(cx).focus(window);
    }

    fn save(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let name = self.name.read(cx).text().trim().to_owned();
        let command = self.command.read(cx).text().trim().to_owned();
        if name.is_empty() || command.is_empty() {
            self.error = Some("Give the action a name and a command.".into());
            cx.notify();
            return;
        }
        let params = json!({"projectId": self.project.as_str(), "name": name, "command": command, "icon": self.icon});
        let task = store::store(cx).update(cx, |s, cx| s.run_command("project.saveScript", params, cx));
        cx.spawn(async move |this, cx| {
            if task.await.is_ok() {
                let _ = this.update(cx, |_, cx| cx.emit(ScriptEditorEvent::Saved));
            }
        })
        .detach();
    }
}

impl Render for ScriptEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let c = theme.colors.clone();
        let field = |label: &'static str, input: Entity<TextArea>| {
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(div().text_size(px(14.)).font_weight(FontWeight::MEDIUM).text_color(c.text).child(label))
                .child(
                    div()
                        .h(px(36.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .rounded(px(radius::MD))
                        .border_1()
                        .border_color(c.line_strong)
                        .bg(c.bg_raised)
                        .child(div().flex_1().child(input)),
                )
        };
        let icons = SCRIPT_ICONS.iter().map(|&(id, glyph, label)| {
            let selected = self.icon == id;
            div()
                .id(id)
                .size(px(32.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(radius::MD))
                .border_1()
                .border_color(if selected { c.accent_ring } else { c.line_strong })
                .when(selected, |el| el.bg(c.accent_soft))
                .cursor_pointer()
                .tooltip(move |_, cx| crate::ui::Tooltip::view(label, None, cx))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.icon = id;
                    cx.notify();
                }))
                .child(
                    svg()
                        .path(glyph.path())
                        .size(px(16.))
                        .text_color(if selected { c.accent_text } else { c.text_2 }),
                )
        });
        div()
            .track_focus(&self.focus_handle)
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w(px(448.))
            .flex()
            .flex_col()
            .gap(px(16.))
            .p(px(24.))
            .rounded(px(radius::XL))
            .bg(theme.floating_bg())
            .border_1()
            .border_color(c.glass_edge)
            .shadow(theme.floating_shadow())
            .child(
                div()
                    .text_size(px(18.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.text)
                    .child("Add action"),
            )
            .child(field("Name", self.name.clone()))
            .child(field("Command", self.command.clone()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(div().text_size(px(14.)).font_weight(FontWeight::MEDIUM).text_color(c.text).child("Icon"))
                    .child(div().flex().gap(px(6.)).children(icons)),
            )
            .when_some(self.error.clone(), |el, error| {
                el.child(div().text_size(px(12.)).text_color(c.danger).child(error))
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        outline_button("script-cancel", None, Some("Cancel".into()), Part::Whole, false, cx)
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ScriptEditorEvent::Dismissed))),
                    )
                    .child(
                        div()
                            .id("script-save")
                            .h(px(24.))
                            .px(px(8.))
                            .flex()
                            .items_center()
                            .rounded(px(radius::MD))
                            .bg(c.accent_fill)
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.9))
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(c.text_on_accent)
                            .child("Save")
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    ),
            )
    }
}
