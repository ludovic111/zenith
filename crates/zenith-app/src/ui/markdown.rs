//! Markdown (what agents write) as GPUI elements: paragraphs and headings with bold, italic,
//! inline code, strikethrough and links that open in the browser; fenced code in the mono
//! font with a copy button; lists (with task boxes), quotes, tables and rules.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    div, font, px, AnyElement, App, ClipboardItem, ElementId, FontStyle, FontWeight, Hsla, InteractiveText, SharedString, StrikethroughStyle, StyledText,
    TextRun, UnderlineStyle,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::assets::{Icon, MONO_FONT, UI_FONT};
use crate::theme::{radius, text, ActiveTheme};
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

/// Renders `markdown` at `size` px; `id` keeps clickable links and code blocks apart.
pub fn render(id: impl Into<SharedString>, markdown: &str, size: f32, cx: &App) -> AnyElement {
    let id: SharedString = id.into();
    let blocks = parse(markdown);
    div()
        .flex()
        .flex_col()
        .gap(px(10.))
        .w_full()
        .min_w_0()
        .children(blocks.iter().enumerate().map(|(i, block)| render_block(&format!("{id}-{i}"), block, size, cx)))
        .into_any_element()
}

fn render_inline(id: &str, inline: &Inline, size: f32, weight: FontWeight, color: Hsla, cx: &App) -> AnyElement {
    let c = &cx.theme().colors;
    let runs: Vec<TextRun> = inline
        .spans
        .iter()
        .map(|(range, style)| {
            let mut f = font(if style.code { MONO_FONT } else { UI_FONT });
            f.weight = if style.bold { FontWeight::BOLD } else { weight };
            if style.italic {
                f.style = FontStyle::Italic;
            }
            TextRun {
                len: range.len(),
                font: f,
                color: if style.link {
                    c.accent_text
                } else if style.code {
                    c.text
                } else {
                    color
                },
                background_color: style.code.then_some(c.hover),
                underline: style.link.then_some(UnderlineStyle {
                    color: Some(Hsla { a: 0.5, ..c.accent_text }),
                    thickness: px(1.),
                    wavy: false,
                }),
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
        .line_height(px((size * 1.55).round()))
        .text_color(color)
        .child(text)
        .into_any_element()
}

fn render_block(id: &str, block: &Block, size: f32, cx: &App) -> AnyElement {
    let c = cx.theme().colors.clone();
    match block {
        Block::Paragraph(inline) => render_inline(id, inline, size, FontWeight::NORMAL, c.text, cx),
        Block::Heading(level, inline) => {
            let (heading_size, weight) = match level {
                1 => (text::XL, FontWeight::BOLD),
                2 => (text::LG, FontWeight::BOLD),
                3 => (size + 1., FontWeight::SEMIBOLD),
                _ => (size, FontWeight::SEMIBOLD),
            };
            div()
                .pt(px(4.))
                .child(render_inline(id, inline, heading_size, weight, c.text, cx))
                .into_any_element()
        }
        Block::Code { lang, text: code } => {
            let copy = code.clone();
            div()
                .flex()
                .flex_col()
                .w_full()
                .rounded(px(radius::SM))
                .bg(c.bg_sunken)
                .border_1()
                .border_color(c.line)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .pl(px(12.))
                        .pr(px(4.))
                        .h(px(28.))
                        .border_b_1()
                        .border_color(c.line)
                        .child(
                            div()
                                .font_family(MONO_FONT)
                                .text_size(px(text::XS))
                                .text_color(c.text_3)
                                .child(SharedString::from(lang.clone().unwrap_or_else(|| "text".into()))),
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
                        .px(px(12.))
                        .py(px(10.))
                        .font_family(MONO_FONT)
                        .text_size(px(text::SM))
                        .line_height(px(19.))
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
                    (None, None) => "•".into(),
                };
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(14.))
                            .text_size(px(size))
                            .line_height(px((size * 1.55).round()))
                            .text_color(c.text_3)
                            .child(marker),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .flex_1()
                            .min_w_0()
                            .children(item.blocks.iter().enumerate().map(|(j, b)| render_block(&format!("{id}-{i}-{j}"), b, size, cx))),
                    )
            }))
            .into_any_element(),
        Block::Quote(blocks) => div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .pl(px(12.))
            .border_l_2()
            .border_color(c.line_strong)
            .children(blocks.iter().enumerate().map(|(i, b)| render_block(&format!("{id}-q{i}"), b, size, cx)))
            .into_any_element(),
        Block::Rule => div().h(px(1.)).w_full().bg(c.line).into_any_element(),
        Block::Table { head, rows } => {
            let cell = |key: String, inline: &Inline, bold: bool| {
                div().flex_1().min_w(px(60.)).px(px(10.)).py(px(6.)).child(render_inline(
                    &key,
                    inline,
                    text::BASE,
                    if bold { FontWeight::SEMIBOLD } else { FontWeight::NORMAL },
                    c.text,
                    cx,
                ))
            };
            div()
                .id(SharedString::from(format!("{id}-table")))
                .overflow_x_scroll()
                .rounded(px(radius::SM))
                .border_1()
                .border_color(c.line)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .flex()
                                .bg(c.hover)
                                .children(head.iter().enumerate().map(|(i, h)| cell(format!("{id}-h{i}"), h, true))),
                        )
                        .children(rows.iter().enumerate().map(|(r, row)| {
                            div()
                                .flex()
                                .border_t_1()
                                .border_color(c.line)
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
