//! Popover menus (context menus, pickers): a floating glass card of items, opened at a point,
//! driven by the pointer or the keyboard (Up, Down, Enter, Escape), closed by a click
//! outside, and handing focus back to where it was.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    actions, anchored, deferred, div, px, App, Context, Corner, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, KeyBinding, Pixels,
    Point, SharedString, Subscription, Window,
};

use crate::assets::Icon;
use crate::theme::{radius, ActiveTheme};
use crate::ui::icon;
use crate::ui::OneLine;

actions!(menu, [SelectNext, SelectPrevious, Confirm, Dismiss]);

pub fn bind_keys(cx: &mut App) {
    let c = Some("Menu");
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, c),
        KeyBinding::new("up", SelectPrevious, c),
        KeyBinding::new("enter", Confirm, c),
        KeyBinding::new("escape", Dismiss, c),
    ]);
}

type Handler = Rc<dyn Fn(&mut Window, &mut App)>;

pub enum Entry {
    Item {
        label: SharedString,
        icon: Option<Icon>,
        detail: Option<SharedString>,
        keys: Option<SharedString>,
        checked: bool,
        danger: bool,
        disabled: bool,
        handler: Handler,
    },
    Header(SharedString),
    Separator,
}

impl Entry {
    pub fn item(label: impl Into<SharedString>, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        Self::Item {
            label: label.into(),
            icon: None,
            detail: None,
            keys: None,
            checked: false,
            danger: false,
            disabled: false,
            handler: Rc::new(handler),
        }
    }

    pub fn icon(mut self, value: Icon) -> Self {
        if let Self::Item { icon, .. } = &mut self {
            *icon = Some(value);
        }
        self
    }

    pub fn detail(mut self, value: impl Into<SharedString>) -> Self {
        if let Self::Item { detail, .. } = &mut self {
            *detail = Some(value.into());
        }
        self
    }

    pub fn keys(mut self, value: impl Into<SharedString>) -> Self {
        if let Self::Item { keys, .. } = &mut self {
            *keys = Some(value.into());
        }
        self
    }

    pub fn checked(mut self, value: bool) -> Self {
        if let Self::Item { checked, .. } = &mut self {
            *checked = value;
        }
        self
    }

    pub fn danger(mut self) -> Self {
        if let Self::Item { danger, .. } = &mut self {
            *danger = true;
        }
        self
    }

    pub fn disabled(mut self, value: bool) -> Self {
        if let Self::Item { disabled, .. } = &mut self {
            *disabled = value;
        }
        self
    }

    fn enabled(&self) -> bool {
        matches!(self, Self::Item { disabled: false, .. })
    }
}

pub struct Menu {
    entries: Vec<Entry>,
    selected: Option<usize>,
    focus_handle: FocusHandle,
    min_width: Pixels,
}

impl EventEmitter<DismissEvent> for Menu {}

impl Focusable for Menu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Menu {
    pub fn new(entries: Vec<Entry>, cx: &mut Context<Self>) -> Self {
        Self {
            entries,
            selected: None,
            focus_handle: cx.focus_handle(),
            min_width: px(200.),
        }
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let count = self.entries.len();
        if count == 0 {
            return;
        }
        let mut index = self.selected.unwrap_or(if forward { count - 1 } else { 0 });
        for _ in 0..count {
            index = if forward { (index + 1) % count } else { (index + count - 1) % count };
            if self.entries[index].enabled() {
                self.selected = Some(index);
                cx.notify();
                return;
            }
        }
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step(true, cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.step(false, cx);
    }

    fn run(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Entry::Item { handler, disabled: false, .. }) = self.entries.get(index) {
            let handler = handler.clone();
            cx.emit(DismissEvent);
            handler(window, cx);
        }
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected {
            self.run(index, window, cx);
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Render for Menu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let c = &theme.colors;
        div()
            .key_context("Menu")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(DismissEvent)))
            .occlude()
            .flex()
            .flex_col()
            .min_w(self.min_width.max(px(160.)))
            .max_w(px(360.))
            .p(px(4.))
            .rounded(px(radius::LG))
            .bg(crate::composer::composer_surface(cx).0)
            .border_1()
            .border_color(c.glass_edge)
            .shadow(theme.floating_shadow())
            .text_size(px(14.))
            .line_height(px(20.))
            .children(self.entries.iter().enumerate().map(|(index, entry)| {
                match entry {
                    // `MenuSeparator`: 1 px of --border, 8 px in, 4 px above and under.
                    Entry::Separator => div().mx(px(8.)).my(px(4.)).h(px(1.)).bg(c.line).into_any_element(),
                    // `MenuGroupLabel`: 12 px medium muted text.
                    Entry::Header(label) => div()
                        .px(px(8.))
                        .py(px(6.))
                        .text_size(px(12.))
                        .line_height(px(16.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(c.text_2)
                        .child(label.clone())
                        .into_any_element(),
                    Entry::Item {
                        label,
                        icon: item_icon,
                        detail,
                        keys,
                        checked,
                        danger,
                        disabled,
                        ..
                    } => {
                        let selected = self.selected == Some(index);
                        let fg = if *danger { c.danger } else { c.text };
                        // `MenuItem`: 28 px, 8 px across, 6 px corners; its icon at 80% of the
                        // muted color; highlighted on --accent.
                        div()
                            .id(index)
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .min_h(px(28.))
                            .px(px(8.))
                            .py(px(4.))
                            .rounded(px(radius::SM))
                            .text_color(fg)
                            .when(selected, |this| this.bg(c.accent_soft))
                            .when(*disabled, |this| this.opacity(0.64))
                            .when(!*disabled, |this| {
                                this.cursor_pointer()
                                    .hover(|s| s.bg(c.accent_soft))
                                    .on_click(cx.listener(move |menu, _, window, cx| menu.run(index, window, cx)))
                            })
                            .when_some(*item_icon, |this, i| {
                                this.child(icon(i, if *danger { c.danger } else { c.text_2.opacity(0.8) }).size(px(16.)))
                            })
                            .child(div().flex_1().one_line().child(label.clone()))
                            .when_some(detail.clone(), |this, detail| {
                                this.child(
                                    div()
                                        .max_w(px(200.))
                                        .text_size(px(12.))
                                        .text_color(c.text_2.opacity(0.8))
                                        .one_line()
                                        .child(detail),
                                )
                            })
                            // `MenuShortcut`: 12 px medium in --secondary-label.
                            .when_some(keys.clone(), |this, keys| {
                                this.child(
                                    div()
                                        .ml(px(16.))
                                        .text_size(px(12.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(c.text_2)
                                        .child(keys),
                                )
                            })
                            .when(*checked, |this| this.child(icon(Icon::Check, c.text).size(px(16.))))
                            .into_any_element()
                    }
                }
            }))
    }
}

/// A menu open somewhere in a view.
pub struct OpenMenu {
    pub menu: Entity<Menu>,
    pub position: Point<Pixels>,
    pub corner: Corner,
    _dismiss: Subscription,
}

impl OpenMenu {
    /// Opens `entries` at `position` (window coordinates); `on_close` clears the owner's slot.
    pub fn new<V: 'static>(
        entries: Vec<Entry>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<V>,
        on_close: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    ) -> Self {
        let previous_focus = window.focused(cx);
        let menu = cx.new(|cx| Menu::new(entries, cx));
        let dismiss = cx.subscribe_in(&menu, window, move |this, _, _: &DismissEvent, window, cx| {
            on_close(this, window, cx);
            if let Some(focus) = previous_focus.as_ref() {
                window.focus(focus);
            }
            cx.notify();
        });
        window.focus(&menu.focus_handle(cx));
        Self {
            menu,
            position,
            corner: Corner::TopLeft,
            _dismiss: dismiss,
        }
    }

    /// Opens upward from the point (menus under the composer).
    pub fn upward(mut self) -> Self {
        self.corner = Corner::BottomLeft;
        self
    }

    pub fn render(&self) -> impl IntoElement {
        deferred(
            anchored()
                .position(self.position)
                .anchor(self.corner)
                .snap_to_window_with_margin(px(8.))
                .child(div().p(px(2.)).child(self.menu.clone())),
        )
        .with_priority(2)
    }
}
