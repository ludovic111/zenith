//! The web interface's small buttons, measured on its computed styles: the `xs` outline button
//! of the header ("Add action", "Open", the git action) and its split variant, and the ghost
//! toggles beside them.

use gpui::prelude::*;
use gpui::{div, px, svg, App, BoxShadow, ElementId, FontWeight, Hsla, SharedString, Stateful};

use crate::assets::Icon;
use crate::theme::{radius, ActiveTheme, Mode};

/// Which side of a split button a part is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Whole,
    /// The main half: rounded on the left, no right border (the separator takes its place).
    Main,
    /// The chevron half: rounded on the right, no left border.
    Chevron,
}

fn outline_colors(cx: &App) -> (Hsla, Hsla, Hsla) {
    let theme = cx.theme();
    let c = &theme.colors;
    // `bg-popover` in light, `bg-input/32` in dark; hovered `bg-accent/50`, `bg-input/64`.
    match theme.mode {
        Mode::Light => (c.glass_opaque, c.accent_soft.opacity(c.accent_soft.a * 0.5), c.line_strong),
        Mode::Dark => (
            c.line_strong.opacity(c.line_strong.a * 0.32),
            c.line_strong.opacity(c.line_strong.a * 0.64),
            c.line_strong,
        ),
    }
}

/// An `xs` outline button (24 px, 12 px medium text, 14 px muted icon), or one half of a split
/// one.
pub fn outline_button(id: impl Into<ElementId>, glyph: Option<Icon>, label: Option<SharedString>, part: Part, disabled: bool, cx: &App) -> Stateful<gpui::Div> {
    let c = cx.theme().colors.clone();
    let (bg, hover_bg, border) = outline_colors(cx);
    let chevron = part == Part::Chevron;
    div()
        .id(id.into())
        .h(px(24.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .when(chevron, |this| this.w(px(24.)))
        .when(!chevron, |this| this.pl(px(5.)).pr(px(7.)))
        .gap(px(4.))
        .bg(bg)
        .border_color(border)
        .border_t_1()
        .border_b_1()
        .when(part != Part::Chevron, |this| this.border_l_1())
        .when(part != Part::Main, |this| this.border_r_1())
        .map(|this| match part {
            Part::Whole => this.rounded(px(radius::MD)),
            Part::Main => this.rounded_l(px(radius::MD)),
            Part::Chevron => this.rounded_r(px(radius::MD)),
        })
        .when(part == Part::Whole || part == Part::Main, |this| {
            this.shadow(vec![BoxShadow {
                color: gpui::hsla(0., 0., 0., 0.05),
                offset: gpui::point(px(0.), px(1.)),
                blur_radius: px(2.),
                spread_radius: px(0.),
            }])
        })
        .text_size(px(12.))
        .line_height(px(16.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(c.text)
        .when(disabled, |this| this.opacity(0.64))
        .when(!disabled, |this| this.cursor_pointer().hover(move |s| s.bg(hover_bg)))
        .when(chevron, |this| {
            this.child(svg().path(Icon::ChevronDown.path()).size(px(16.)).flex_none().text_color(c.text_2))
        })
        .when_some(glyph.filter(|_| !chevron), |this, glyph| {
            this.child(svg().path(glyph.path()).size(px(14.)).flex_none().text_color(c.text_2))
        })
        .when_some(label, |this, label| this.child(label))
}

/// The 1 px line between the halves of a split button.
pub fn split_separator(cx: &App) -> gpui::Div {
    div().w(px(1.)).h(px(24.)).flex_none().bg(cx.theme().colors.line_strong)
}

/// A ghost toggle of the header's right end (`Toggle variant="ghost" size="sm"`): 28 px, the
/// icon at 80% of the text color, the soft accent behind it when on or hovered.
pub fn header_toggle(id: impl Into<ElementId>, glyph: Icon, on: bool, cx: &App) -> Stateful<gpui::Div> {
    let c = cx.theme().colors.clone();
    let accent = c.accent_soft;
    div()
        .id(id.into())
        .size(px(28.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(radius::LG))
        .cursor_pointer()
        .when(on, |this| this.bg(accent))
        .hover(move |s| s.bg(accent))
        .child(svg().path(glyph.path()).size(px(16.)).flex_none().text_color(c.text.opacity(0.8)))
}
