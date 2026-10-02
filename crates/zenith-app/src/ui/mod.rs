//! zenith's controls, drawn with GPUI's elements and the lsuite tokens.

pub mod badges;
pub mod controls;
pub mod markdown;
pub mod menu;
pub mod text_area;

use std::rc::Rc;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, percentage, px, svg, Animation, AnimationExt, AnyView, App, ClickEvent, ElementId, FontWeight, Hsla, SharedString, Svg, Transformation, Window,
};

use crate::assets::{Icon, MONO_FONT};
use crate::theme::{radius, text, ActiveTheme};

/// An icon at 14 px in the given color.
pub fn icon(icon: Icon, color: Hsla) -> Svg {
    svg().path(icon.path()).size(px(14.)).flex_none().text_color(color)
}

/// A spinning loader.
pub fn spinner(id: impl Into<ElementId>, color: Hsla, size: f32) -> impl IntoElement {
    svg().path(Icon::Loader.path()).size(px(size)).flex_none().text_color(color).with_animation(
        id,
        Animation::new(Duration::from_millis(900)).repeat(),
        |svg, delta| svg.with_transformation(Transformation::rotate(percentage(delta))),
    )
}

/// An icon-only button as the web draws them (`size="icon"` ghost buttons): `size` square,
/// rounded `radius`, the icon at `icon_size` in `color`; hovered, `hover_bg` behind it and the
/// icon in `hover_color`.
#[allow(clippy::too_many_arguments)]
pub fn icon_button(
    id: impl Into<ElementId>,
    glyph: Icon,
    size: f32,
    icon_size: f32,
    radius: f32,
    color: Hsla,
    hover_bg: Hsla,
    hover_color: Hsla,
) -> gpui::Stateful<gpui::Div> {
    let id: ElementId = id.into();
    let group: SharedString = format!("icon-button-{id}").into();
    div()
        .id(id)
        .group(group.clone())
        .size(px(size))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(radius))
        .cursor_pointer()
        .hover(move |s| s.bg(hover_bg))
        .child(
            svg()
                .path(glyph.path())
                .size(px(icon_size))
                .flex_none()
                .text_color(color)
                .group_hover(group, move |s| s.text_color(hover_color)),
        )
}

/// One line of text ending in "…" when too long. GPUI's `truncate` keeps the first measure
/// of text that does not wrap, taken before a flex row knows its width; text clamped to one
/// line is measured again once the width is known.
pub trait OneLine: Styled + Sized {
    fn one_line(self) -> Self {
        self.overflow_hidden().line_clamp(1).text_ellipsis()
    }
}

impl<T: Styled> OneLine for T {}

/// A small status dot.
pub fn dot(color: Hsla) -> gpui::Div {
    div().size(px(7.)).flex_none().rounded_full().bg(color)
}

/// A keyboard shortcut, in the mono font.
pub fn kbd(keys: impl Into<SharedString>, cx: &App) -> gpui::Div {
    let c = &cx.theme().colors;
    div()
        .px(px(5.))
        .py(px(1.))
        .rounded(px(radius::XS))
        .border_1()
        .border_color(c.line_strong)
        .font_family(MONO_FONT)
        .text_size(px(text::XS))
        .text_color(c.text_3)
        .child(keys.into())
}

/// A tooltip bubble.
pub struct Tooltip {
    text: SharedString,
    keys: Option<SharedString>,
}

impl Tooltip {
    pub fn view(text: impl Into<SharedString>, keys: Option<SharedString>, cx: &mut App) -> AnyView {
        let text = text.into();
        cx.new(|_| Self { text, keys }).into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let c = &theme.colors;
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(px(radius::SM))
            .bg(theme.floating_bg())
            .border_1()
            .border_color(c.glass_edge)
            .shadow(theme.floating_shadow())
            .text_size(px(text::SM))
            .text_color(c.text)
            .child(self.text.clone())
            .when_some(self.keys.clone(), |this, keys| this.child(kbd(keys, cx)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// The one main action (accent fill).
    Primary,
    /// A bordered secondary action.
    Secondary,
    /// No chrome until hovered.
    Ghost,
    /// Destructive.
    Danger,
}

type ClickHandler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: Option<SharedString>,
    icon: Option<Icon>,
    variant: Variant,
    small: bool,
    disabled: bool,
    selected: bool,
    tooltip: Option<(SharedString, Option<SharedString>)>,
    on_click: Option<ClickHandler>,
}

impl Button {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            label: None,
            icon: None,
            variant: Variant::Ghost,
            small: false,
            disabled: false,
            selected: false,
            tooltip: None,
            on_click: None,
        }
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn variant(mut self, variant: Variant) -> Self {
        self.variant = variant;
        self
    }

    pub fn small(mut self) -> Self {
        self.small = true;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.tooltip = Some((text.into(), None));
        self
    }

    pub fn tooltip_keys(mut self, text: impl Into<SharedString>, keys: impl Into<SharedString>) -> Self {
        self.tooltip = Some((text.into(), Some(keys.into())));
        self
    }

    pub fn on_click(mut self, handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let c = cx.theme().colors.clone();
        let height = if self.small { 24. } else { 28. };
        let (bg, fg, border, hover_bg) = match self.variant {
            Variant::Primary => (c.accent_fill, c.text_on_accent, None, c.accent_hover),
            Variant::Secondary => (gpui::transparent_black(), c.text, Some(c.line_strong), c.hover),
            Variant::Ghost => (gpui::transparent_black(), c.text_2, None, c.hover),
            Variant::Danger => (gpui::transparent_black(), c.danger, Some(c.line_strong), Hsla { a: 0.12, ..c.danger }),
        };
        let (bg, fg) = if self.selected { (c.accent_soft, c.accent_text) } else { (bg, fg) };
        let icon_only = self.label.is_none();
        let disabled = self.disabled;
        let on_click = self.on_click.clone();
        div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .h(px(height))
            .when(icon_only, |this| this.w(px(height)))
            .when(!icon_only, |this| this.px(px(if self.small { 8. } else { 10. })))
            .rounded(px(radius::SM))
            .bg(bg)
            .when_some(border, |this, border| this.border_1().border_color(border))
            .text_size(px(text::BASE))
            .font_weight(FontWeight::MEDIUM)
            .text_color(fg)
            .when(disabled, |this| this.opacity(0.45))
            .when(!disabled, |this| {
                this.cursor_pointer()
                    .hover(move |style| style.bg(if self.selected { c.accent_soft } else { hover_bg }))
            })
            .when_some(self.icon, |this, i| this.child(icon(i, fg)))
            .when_some(self.label, |this, label| this.child(label))
            .when_some(self.tooltip, |this, (text, keys)| {
                this.tooltip(move |_, cx| Tooltip::view(text.clone(), keys.clone(), cx))
            })
            .when_some(on_click.filter(|_| !disabled), |this, handler| {
                this.on_click(move |event, window, cx| {
                    cx.stop_propagation();
                    handler(event, window, cx)
                })
            })
    }
}

/// A small rounded label (a status, a count, a model name).
pub fn pill(label: impl Into<SharedString>, fg: Hsla, bg: Hsla) -> gpui::Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap_1()
        .px(px(7.))
        .h(px(20.))
        .rounded_full()
        .bg(bg)
        .text_color(fg)
        .text_size(px(text::XS))
        .font_weight(FontWeight::SEMIBOLD)
        .child(label.into())
}

/// A section heading in caps, in the mono font (DESIGN.md: labels in caps use Plex Mono).
pub fn caps_label(label: impl Into<SharedString>, cx: &App) -> gpui::Div {
    div()
        .font_family(MONO_FONT)
        .text_size(px(text::XS))
        .text_color(cx.theme().colors.text_3)
        .child(SharedString::from(label.into().to_uppercase()))
}
