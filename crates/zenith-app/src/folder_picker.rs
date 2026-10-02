//! Picks a folder on the server's machine, for "Add Project" when the server is remote (the
//! Mac's own folder picker would show the Mac's folders). The path is typed, its folders are
//! listed by `project.browse` as it changes; Enter goes into the selected folder, and the first
//! row adds the folder the path names.

use gpui::prelude::*;
use gpui::{div, px, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, SharedString, Subscription, Task, Window};
use serde_json::json;

use crate::assets::Icon;
use crate::store;
use crate::theme::{radius, text, ActiveTheme};
use crate::ui::icon;
use crate::ui::text_area::{TextArea, TextAreaEvent};

pub enum FolderPickerEvent {
    Dismissed,
    /// An absolute path on the server's machine.
    Picked(String),
}

/// What the server listed for the path typed.
#[derive(Default)]
struct Listing {
    /// The folder the path ends in (absolute), when it ends with `/`.
    folder: Option<String>,
    /// Its folders (or the folders the last part of the path starts).
    entries: Vec<(SharedString, String)>,
    error: Option<SharedString>,
}

pub struct FolderPicker {
    input: Entity<TextArea>,
    listing: Listing,
    selected: usize,
    server: SharedString,
    focus_handle: FocusHandle,
    browsing: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<FolderPickerEvent> for FolderPicker {}

impl Focusable for FolderPicker {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl FolderPicker {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextArea::single_line(cx).with_placeholder("~/projects/").with_font(text::MD, 22.));
        let subscription = cx.subscribe_in(&input, window, |this, _, event: &TextAreaEvent, _, cx| match event {
            TextAreaEvent::Changed => this.browse(cx),
            TextAreaEvent::MoveDown => this.step(1, cx),
            TextAreaEvent::MoveUp => this.step(-1, cx),
            TextAreaEvent::Submit => this.confirm(this.selected, cx),
            TextAreaEvent::Cancel => cx.emit(FolderPickerEvent::Dismissed),
            TextAreaEvent::PastedImage { .. } => {}
        });
        let server = store::store(cx).read(cx).server_name();
        let this = Self {
            input,
            listing: Listing::default(),
            selected: 0,
            server,
            focus_handle: cx.focus_handle(),
            browsing: None,
            _subscription: subscription,
        };
        this.input.update(cx, |input, cx| input.set_text("~/", cx));
        this
    }

    pub fn focus(&self, window: &mut Window, cx: &App) {
        self.input.read(cx).focus(window);
    }

    /// Rows: the folder itself first (when the path ends with `/`), then its folders.
    fn rows(&self) -> usize {
        usize::from(self.listing.folder.is_some()) + self.listing.entries.len()
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let rows = self.rows() as isize;
        if rows > 0 {
            self.selected = (self.selected as isize + delta).rem_euclid(rows) as usize;
            cx.notify();
        }
    }

    fn browse(&mut self, cx: &mut Context<Self>) {
        let path = self.input.read(cx).text().trim().to_owned();
        if path.is_empty() {
            self.listing = Listing::default();
            self.browsing = None;
            cx.notify();
            return;
        }
        let task = store::store(cx).read(cx).run_command_quiet("project.browse", json!({"path": path}));
        let ends_in_folder = path.ends_with('/') || path == "~";
        // A newer path replaces this task, so a late answer never shows over a newer one.
        self.browsing = Some(cx.spawn(async move |this, cx| {
            let listing = match task.await {
                Ok(found) => Listing {
                    folder: ends_in_folder.then(|| found["parentPath"].as_str().map(String::from)).flatten(),
                    entries: found["entries"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|e| Some((SharedString::from(e["name"].as_str()?.to_owned()), e["fullPath"].as_str()?.to_owned())))
                        .collect(),
                    error: None,
                },
                Err(error) => Listing {
                    error: Some(error.into()),
                    ..Listing::default()
                },
            };
            let _ = this.update(cx, |this, cx| {
                this.listing = listing;
                this.selected = 0;
                cx.notify();
            });
        }));
    }

    fn confirm(&mut self, row: usize, cx: &mut Context<Self>) {
        let entry = match &self.listing.folder {
            Some(folder) if row == 0 => {
                cx.emit(FolderPickerEvent::Picked(folder.clone()));
                return;
            }
            Some(_) => self.listing.entries.get(row - 1),
            None => self.listing.entries.get(row),
        };
        if let Some((_, full_path)) = entry {
            let path = format!("{}/", full_path.trim_end_matches('/'));
            self.input.update(cx, |input, cx| input.set_text(path, cx));
        }
    }
}

impl Render for FolderPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let c = theme.colors.clone();
        let row = |position: usize, glyph: Icon, label: SharedString, detail: Option<SharedString>, cx: &mut Context<Self>| {
            let selected = position == self.selected;
            div()
                .id(position)
                .flex()
                .items_center()
                .gap(px(10.))
                .mx(px(6.))
                .px(px(8.))
                .h(px(34.))
                .rounded(px(radius::SM))
                .cursor_pointer()
                .when(selected, |this| this.bg(c.accent_soft))
                .hover(|s| s.bg(c.accent_soft))
                .on_click(cx.listener(move |this, _, _, cx| this.confirm(position, cx)))
                .child(icon(glyph, if selected { c.accent_text } else { c.text_3 }))
                .child(
                    div()
                        .flex_none()
                        .max_w(px(380.))
                        .truncate()
                        .text_size(px(text::BASE))
                        .text_color(c.text)
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(text::SM))
                        .text_color(c.text_3)
                        .children(detail),
                )
                .into_any_element()
        };
        let mut rows = Vec::new();
        if let Some(folder) = self.listing.folder.clone() {
            rows.push(row(0, Icon::FolderPlus, "Add this folder".into(), Some(folder.into()), cx));
        }
        let offset = rows.len();
        for (i, (name, _)) in self.listing.entries.clone().into_iter().enumerate() {
            rows.push(row(offset + i, Icon::Folder, name, None, cx));
        }
        div()
            .track_focus(&self.focus_handle)
            .occlude()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w(px(640.))
            .max_h(px(520.))
            .flex()
            .flex_col()
            .rounded(px(radius::LG))
            .bg(theme.floating_bg())
            .border_1()
            .border_color(c.glass_edge)
            .shadow(theme.floating_shadow())
            .child(
                div()
                    .px(px(16.))
                    .pt(px(12.))
                    .text_size(px(text::XS))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.text_3)
                    .child(SharedString::from(format!("Add a project: a folder on {}", self.server))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(16.))
                    .h(px(48.))
                    .border_b_1()
                    .border_color(c.line)
                    .child(icon(Icon::Folder, c.text_3).size(px(16.)))
                    .child(div().flex_1().child(self.input.clone())),
            )
            .child(
                div()
                    .id("folder-picker-results")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .py(px(6.))
                    .children(rows)
                    .when_some(self.listing.error.clone(), |this, error| {
                        this.child(div().p(px(16.)).text_size(px(text::BASE)).text_color(c.danger).child(error))
                    })
                    .when(self.listing.error.is_none() && self.rows() == 0, |this| {
                        this.child(
                            div()
                                .p(px(16.))
                                .text_size(px(text::BASE))
                                .text_color(c.text_3)
                                .child("No folder here. End the path with / to add it."),
                        )
                    }),
            )
    }
}
