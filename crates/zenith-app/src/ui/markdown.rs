//! Markdown (what agents write) as GPUI elements: paragraphs and headings with bold, italic,
//! inline code, strikethrough and links that open in the browser; fenced code in the mono
//! font with a copy button; lists (with task boxes), quotes, tables and rules.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    div, font, px, AnyElement, App, ClipboardItem, ElementId, FontStyle, FontWeight, Hsla, InteractiveText, SharedString, StrikethroughStyle, StyledText,
    TextRun,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::assets::{Icon, MONO_FONT, UI_FONT};
use crate::theme::{radius, ActiveTheme};
use crate::ui::Button;

#[derive(Clone, Copy, Default, PartialEq)]
struct SpanStyle {
    bold: bool,
    italic: bool,
    code: bool,
    strike: bool,
    link: bool,
}

#[derive(Clone, Default)]
struct Inline {
    text: String,
    spans: Vec<(Range<usize>, SpanStyle)>,
    links: Vec<(Range<usize>, String)>,
}

impl Inline {
    fn push(&mut self, text: &str, style: SpanStyle) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        self.spans.push((start..self.text.len(), style));
    }

    fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

#[derive(Clone)]
enum Block {
    Paragraph(Inline),
    Heading(u8, Inline),
    Code { lang: Option<String>, text: String },
    List { start: Option<u64>, items: Vec<ListItem> },
    Quote(Vec<Block>),
    Rule,
    Table { head: Vec<Inline>, rows: Vec<Vec<Inline>> },
}

#[derive(Clone, Default)]
struct ListItem {
    checked: Option<bool>,
    blocks: Vec<Block>,
}

/// The parser's state: open containers, the inline being built, the current style.
struct Builder {
    stack: Vec<Container>,
    inline: Option<Inline>,
    style: SpanStyle,
    link: Option<(usize, String)>,
    code: Option<(Option<String>, String)>,
    table: Option<TableState>,
}

/// A table being read: head cells, body rows, the row in progress, inside the head.
type TableState = (Vec<Inline>, Vec<Vec<Inline>>, Vec<Inline>, bool);

enum Container {
    Root(Vec<Block>),
    Quote(Vec<Block>),
    List(Option<u64>, Vec<ListItem>),
    Item(ListItem),
}

impl Container {
    fn blocks(&mut self) -> Option<&mut Vec<Block>> {
        match self {
            Self::Root(b) | Self::Quote(b) => Some(b),
            Self::Item(item) => Some(&mut item.blocks),
            Self::List(..) => None,
        }
    }
}

impl Builder {
    fn push_block(&mut self, block: Block) {
        for container in self.stack.iter_mut().rev() {
            if let Some(blocks) = container.blocks() {
                blocks.push(block);
                return;
            }
        }
    }

    fn flush_inline(&mut self) {
        if let Some(inline) = self.inline.take() {
            if !inline.is_empty() {
                self.push_block(Block::Paragraph(inline));
            }
        }
    }

    fn inline(&mut self) -> &mut Inline {
        self.inline.get_or_insert_with(Inline::default)
    }
}

fn parse(markdown: &str) -> Vec<Block> {
    let mut b = Builder {
        stack: vec![Container::Root(Vec::new())],
        inline: None,
        style: SpanStyle::default(),
        link: None,
        code: None,
        table: None,
    };
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => b.flush_inline(),
                Tag::Heading { .. } => b.flush_inline(),
                Tag::BlockQuote(_) => {
                    b.flush_inline();
                    b.stack.push(Container::Quote(Vec::new()));
                }
                Tag::CodeBlock(kind) => {
                    b.flush_inline();
                    let lang = match kind {
                        CodeBlockKind::Fenced(lang) if !lang.is_empty() => Some(lang.split_whitespace().next().unwrap_or("").to_owned()),
                        _ => None,
                    };
                    b.code = Some((lang, String::new()));
                }
                Tag::List(start) => {
                    b.flush_inline();
                    b.stack.push(Container::List(start, Vec::new()));
                }
                Tag::Item => {
                    b.flush_inline();
                    b.stack.push(Container::Item(ListItem::default()));
                }
                Tag::Emphasis => b.style.italic = true,
                Tag::Strong => b.style.bold = true,
                Tag::Strikethrough => b.style.strike = true,
                Tag::Link { dest_url, .. } => {
                    let start = b.inline().text.len();
                    b.link = Some((start, dest_url.to_string()));
                    b.style.link = true;
                }
                Tag::Table(_) => {
                    b.flush_inline();
                    b.table = Some((Vec::new(), Vec::new(), Vec::new(), true));
                }
                Tag::TableHead => {
                    if let Some(t) = b.table.as_mut() {
                        t.3 = true;
                    }
                }
                Tag::TableRow => {
                    if let Some(t) = b.table.as_mut() {
                        t.2.clear();
                        t.3 = false;
                    }
                }
                Tag::TableCell => b.inline = Some(Inline::default()),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => b.flush_inline(),
                TagEnd::Heading(level) => {
                    let inline = b.inline.take().unwrap_or_default();
                    let level = match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        _ => 4,
                    };
                    b.push_block(Block::Heading(level, inline));
                }
                TagEnd::BlockQuote(_) => {
                    b.flush_inline();
                    if let Some(Container::Quote(blocks)) = b.stack.pop() {
                        b.push_block(Block::Quote(blocks));
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, text)) = b.code.take() {
                        b.push_block(Block::Code {
                            lang,
                            text: text.trim_end_matches('\n').to_owned(),
                        });
                    }
                }
                TagEnd::List(_) => {
                    b.flush_inline();
                    if let Some(Container::List(start, items)) = b.stack.pop() {
                        b.push_block(Block::List { start, items });
                    }
                }
                TagEnd::Item => {
                    b.flush_inline();
                    if let Some(Container::Item(item)) = b.stack.pop() {
                        if let Some(Container::List(_, items)) = b.stack.last_mut() {
                            items.push(item);
                        }
                    }
                }
                TagEnd::Emphasis => b.style.italic = false,
                TagEnd::Strong => b.style.bold = false,
                TagEnd::Strikethrough => b.style.strike = false,
                TagEnd::Link => {
                    b.style.link = false;
                    if let Some((start, url)) = b.link.take() {
                        let inline = b.inline();
                        let end = inline.text.len();
                        if end > start {
                            inline.links.push((start..end, url));
                        }
                    }
                }
                TagEnd::TableCell => {
                    let cell = b.inline.take().unwrap_or_default();
                    if let Some(t) = b.table.as_mut() {
                        if t.3 {
                            t.0.push(cell);
                        } else {
                            t.2.push(cell);
                        }
                    }
                }
                TagEnd::TableRow => {
                    if let Some(t) = b.table.as_mut() {
                        let row = std::mem::take(&mut t.2);
                        t.1.push(row);
                    }
                }
                TagEnd::TableHead => {
                    if let Some(t) = b.table.as_mut() {
                        t.3 = false;
                    }
                }
                TagEnd::Table => {
                    if let Some((head, rows, _, _)) = b.table.take() {
                        b.push_block(Block::Table { head, rows });
                    }
                }
                _ => {}
            },
            Event::Text(text) => {
                if let Some((_, code)) = b.code.as_mut() {
                    code.push_str(&text);
                } else {
                    let style = b.style;
                    b.inline().push(&text, style);
                }
            }
            Event::Code(code) => {
                let style = SpanStyle { code: true, ..b.style };
                b.inline().push(&code, style);
            }
            Event::SoftBreak => {
                let style = b.style;
                b.inline().push(" ", style);
            }
            Event::HardBreak => {
                let style = b.style;
                b.inline().push("\n", style);
            }
            Event::Rule => {
                b.flush_inline();
                b.push_block(Block::Rule);
            }
            Event::TaskListMarker(checked) => {
                if let Some(Container::Item(item)) = b.stack.last_mut() {
                    item.checked = Some(checked);
                }
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                let style = b.style;
                b.inline().push(&html, style);
            }
            _ => {}
        }
    }
    b.flush_inline();
    match b.stack.into_iter().next() {
        Some(Container::Root(blocks)) => blocks,
        _ => Vec::new(),
    }
}

/// How a piece of markdown reads: the web's `.chat-markdown` at 14 px on 22.75 (`leading-
/// relaxed`), in the text color it is given (the assistant writes at 80% of the foreground).
#[derive(Clone, Copy)]
pub struct MdStyle {
    pub size: f32,
    pub line_height: f32,
    pub color: Hsla,
    /// How deep in lists (disc, then circle, then square, as the web's CSS).
    pub list_depth: u8,
}

impl MdStyle {
    pub fn web(color: Hsla) -> Self {
        Self {
            size: 14.,
            line_height: 22.75,
            color,
            list_depth: 0,
        }
    }
}

/// Renders `markdown` at `size` px in the text color; `id` keeps clickable links and code
/// blocks apart.
pub fn render(id: impl Into<SharedString>, markdown: &str, size: f32, cx: &App) -> AnyElement {
    let color = cx.theme().colors.text;
    render_styled(
        id,
        markdown,
        MdStyle {
            size,
            line_height: (size * 1.625 * 4.).round() / 4.,
            color,
            list_depth: 0,
        },
        cx,
    )
}

/// The margins around a block (`index.css`, `.chat-markdown`): 0.65rem for paragraphs, lists,
/// quotes, code and tables; 1.25rem above and 0.5rem below a heading. Siblings' margins
/// collapse, and the first and last blocks have none outside.
fn margins(block: &Block) -> (f32, f32) {
    match block {
        Block::Heading(..) => (20., 8.),
        Block::Rule => (16., 16.),
        _ => (10.4, 10.4),
    }
}

/// Blocks stacked with their collapsed margins.
fn stack(id: &str, blocks: &[Block], style: MdStyle, cx: &App) -> gpui::Div {
    let mut previous_bottom: Option<f32> = None;
    div().flex().flex_col().w_full().min_w_0().children(blocks.iter().enumerate().map(|(i, block)| {
        let (top, bottom) = margins(block);
        let gap = previous_bottom.map(|b| b.max(top)).unwrap_or(0.);
        previous_bottom = Some(bottom);
        div().w_full().min_w_0().mt(px(gap)).child(render_block(&format!("{id}-{i}"), block, style, cx))
    }))
}

pub fn render_styled(id: impl Into<SharedString>, markdown: &str, style: MdStyle, cx: &App) -> AnyElement {
    let id: SharedString = id.into();
    let blocks = parse(markdown);
    stack(&id, &blocks, style, cx).into_any_element()
}

fn render_inline(id: &str, inline: &Inline, size: f32, line_height: f32, weight: FontWeight, color: Hsla, cx: &App) -> AnyElement {
    let c = &cx.theme().colors;
    let runs: Vec<TextRun> = inline
        .spans
        .iter()
        .map(|(range, style)| {
            let mut f = font(if style.code { MONO_FONT } else { UI_FONT });
            // `strong` is `bolder`: 700 over 400.
            f.weight = if style.bold { FontWeight::BOLD } else { weight };
            if style.italic {
                f.style = FontStyle::Italic;
            }
            TextRun {
                len: range.len(),
                font: f,
                // Links in `--info-foreground`, code in the full foreground on `--muted`.
                color: if style.link {
                    c.info_text
                } else if style.code {
                    c.text
                } else {
                    color
                },
                background_color: style.code.then_some(c.muted),
                underline: None,
                strikethrough: style.strike.then_some(StrikethroughStyle {
                    color: Some(color),
                    thickness: px(1.),
                }),
            }
        })
        .collect();
    let styled = StyledText::new(inline.text.clone()).with_runs(runs);
    let links = inline.links.clone();
    let ranges: Vec<Range<usize>> = links.iter().map(|(r, _)| r.clone()).collect();
    let text = InteractiveText::new(ElementId::Name(SharedString::from(format!("{id}-text"))), styled).on_click(ranges, move |index, _, cx| {
        if let Some((_, url)) = links.get(index) {
            cx.open_url(url);
        }
    });
    div()
        .w_full()
        .min_w_0()
        .text_size(px(size))
        .line_height(px(line_height))
        .text_color(color)
        .child(text)
        .into_any_element()
}

fn render_block(id: &str, block: &Block, style: MdStyle, cx: &App) -> AnyElement {
    let theme = cx.theme().clone();
    let c = theme.colors.clone();
    let MdStyle {
        size,
        line_height,
        color,
        list_depth,
    } = style;
    match block {
        Block::Paragraph(inline) => render_inline(id, inline, size, line_height, FontWeight::NORMAL, color, cx),
        Block::Heading(level, inline) => {
            // 600, line-height 1.3, in the full foreground; h6 muted.
            let heading_size = match level {
                1 => 20.,
                2 => 18.,
                3 => 16.,
                _ => 14.,
            };
            let heading_color = if *level >= 6 { c.text_2 } else { c.text };
            render_inline(
                id,
                inline,
                heading_size,
                (heading_size * 1.3 * 4.).round() / 4.,
                FontWeight::SEMIBOLD,
                heading_color,
                cx,
            )
        }
        Block::Code { lang, text: code } => {
            let copy = code.clone();
            let dark = theme.mode == crate::theme::Mode::Dark;
            // `bg-secondary` with a 70% border in light; `bg-input/32` and no border in dark.
            let bg = if dark { c.line_strong.opacity(c.line_strong.a * 0.32) } else { c.muted };
            div()
                .flex()
                .flex_col()
                .w_full()
                .rounded(px(radius::LG))
                .overflow_hidden()
                .bg(bg)
                .when(!dark, |el| el.border_1().border_color(c.line.opacity(c.line.a * 0.7)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(8.))
                        .pt(px(6.))
                        .pr(px(6.))
                        .pl(px(12.))
                        .child(
                            div()
                                .font_family(MONO_FONT)
                                .text_size(px(11.))
                                .text_color(c.text.opacity(0.72))
                                .child(SharedString::from(lang.clone().unwrap_or_default())),
                        )
                        .child(
                            Button::new(SharedString::from(format!("{id}-copy")))
                                .icon(Icon::Copy)
                                .small()
                                .tooltip("Copy")
                                .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))),
                        ),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("{id}-scroll")))
                        .overflow_x_scroll()
                        .px(px(14.4))
                        .py(px(12.8))
                        .font_family(MONO_FONT)
                        .text_size(px(13.))
                        .line_height(px(17.875))
                        .text_color(c.text)
                        .whitespace_nowrap()
                        .child(SharedString::from(code.clone())),
                )
                .into_any_element()
        }
        Block::List { start, items } => div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .children(items.iter().enumerate().map(|(i, item)| {
                let marker: SharedString = match (item.checked, start) {
                    (Some(true), _) => "☑".into(),
                    (Some(false), _) => "☐".into(),
                    (None, Some(n)) => format!("{}.", n + i as u64).into(),
                    (None, None) => match list_depth {
                        0 => "•".into(),
                        1 => "◦".into(),
                        _ => "▪".into(),
                    },
                };
                // The marker hangs in the list's 20 px gutter.
                div()
                    .flex()
                    .child(
                        div()
                            .flex_none()
                            .w(px(20.))
                            .pr(px(6.))
                            .flex()
                            .justify_end()
                            .text_size(px(size))
                            .line_height(px(line_height))
                            .text_color(color)
                            .child(marker),
                    )
                    .child(
                        stack(
                            &format!("{id}-{i}"),
                            &item.blocks,
                            MdStyle {
                                list_depth: list_depth + 1,
                                ..style
                            },
                            cx,
                        )
                        .flex_1(),
                    )
            }))
            .into_any_element(),
        Block::Quote(blocks) => div()
            .pl(px(12.8))
            .border_l_2()
            .border_color(c.line)
            .child(stack(&format!("{id}-q"), blocks, MdStyle { color: c.text_2, ..style }, cx))
            .into_any_element(),
        Block::Rule => div().h(px(1.)).w_full().bg(c.line).into_any_element(),
        Block::Table { head, rows } => {
            let cell = |key: String, inline: &Inline, bold: bool| {
                div().flex_1().min_w(px(60.)).px(px(12.)).py(px(7.2)).child(render_inline(
                    &key,
                    inline,
                    12.,
                    16.,
                    if bold { FontWeight::SEMIBOLD } else { FontWeight::NORMAL },
                    color,
                    cx,
                ))
            };
            let rule = c.line.opacity(c.line.a * 0.6);
            div()
                .id(SharedString::from(format!("{id}-table")))
                .overflow_x_scroll()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .border_b_1()
                                .border_color(rule)
                                .children(head.iter().enumerate().map(|(i, h)| cell(format!("{id}-h{i}"), h, true))),
                        )
                        .children(rows.iter().enumerate().map(|(r, row)| {
                            div()
                                .flex()
                                .border_b_1()
                                .border_color(rule)
                                .children(row.iter().enumerate().map(|(i, v)| cell(format!("{id}-r{r}-{i}"), v, false)))
                        })),
                )
                .into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks() {
        let blocks = parse("# Title\n\nSome **bold** and `code` with a [link](https://lsuite.xyz).\n\n- [x] done\n- [ ] todo\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> quoted\n");
        assert!(matches!(blocks[0], Block::Heading(1, _)));
        let Block::Paragraph(p) = &blocks[1] else { panic!() };
        assert_eq!(p.text, "Some bold and code with a link.");
        assert_eq!(p.links.len(), 1);
        assert_eq!(&p.text[p.links[0].0.clone()], "link");
        let Block::List { items, .. } = &blocks[2] else { panic!() };
        assert_eq!(items[0].checked, Some(true));
        assert!(matches!(&blocks[3], Block::Code { lang: Some(l), text } if l == "rust" && text == "fn main() {}"));
        let Block::Table { head, rows } = &blocks[4] else { panic!() };
        assert_eq!(head.len(), 2);
        assert_eq!(rows[0][1].text, "2");
        assert!(matches!(&blocks[5], Block::Quote(_)));
    }
}
