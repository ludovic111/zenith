//! A text field drawn with GPUI's text system: one line (search fields, the palette) or
//! several (the composer), with wrapping, selection with the mouse and the keyboard, word and
//! line moves, input methods (`EntityInputHandler`), the clipboard, undo, and a height that
//! grows with the text up to a limit, then scrolls.
//!
//! Enter emits [`TextAreaEvent::Submit`] (Shift-Enter or Option-Enter insert a line), Escape
//! [`TextAreaEvent::Cancel`]; in a one-line field, Up and Down emit `MoveUp`/`MoveDown` so a
//! list under it can follow. Started from GPUI's `examples/input.rs` (Apache-2.0).

use std::ops::Range;

use gpui::prelude::*;
use gpui::{
    actions, div, fill, point, px, size, App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, GlobalElementId, Hsla, KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad,
    Pixels, Point, ScrollWheelEvent, SharedString, Style, TextAlign, TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme::ActiveTheme;

actions!(
    text_area,
    [
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteToLineStart,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        LineStart,
        LineEnd,
        SelectLineStart,
        SelectLineEnd,
        DocStart,
        DocEnd,
        SelectDocStart,
        SelectDocEnd,
        SelectAll,
        Newline,
        Submit,
        Cancel,
        Paste,
        Copy,
        Cut,
        Undo,
        Redo,
        ShowCharacterPalette,
    ]
);

const CONTEXT: &str = "TextArea";

/// The keys a focused field answers to.
pub fn bind_keys(cx: &mut App) {
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new("shift-backspace", Backspace, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("alt-backspace", DeleteWordLeft, c),
        KeyBinding::new("cmd-backspace", DeleteToLineStart, c),
        KeyBinding::new("left", Left, c),
        KeyBinding::new("right", Right, c),
        KeyBinding::new("up", Up, c),
        KeyBinding::new("down", Down, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        KeyBinding::new("shift-up", SelectUp, c),
        KeyBinding::new("shift-down", SelectDown, c),
        KeyBinding::new("alt-left", WordLeft, c),
        KeyBinding::new("alt-right", WordRight, c),
        KeyBinding::new("alt-shift-left", SelectWordLeft, c),
        KeyBinding::new("alt-shift-right", SelectWordRight, c),
        KeyBinding::new("cmd-left", LineStart, c),
        KeyBinding::new("cmd-right", LineEnd, c),
        KeyBinding::new("home", LineStart, c),
        KeyBinding::new("end", LineEnd, c),
        KeyBinding::new("ctrl-a", LineStart, c),
        KeyBinding::new("ctrl-e", LineEnd, c),
        KeyBinding::new("cmd-shift-left", SelectLineStart, c),
        KeyBinding::new("cmd-shift-right", SelectLineEnd, c),
        KeyBinding::new("cmd-up", DocStart, c),
        KeyBinding::new("cmd-down", DocEnd, c),
        KeyBinding::new("cmd-shift-up", SelectDocStart, c),
        KeyBinding::new("cmd-shift-down", SelectDocEnd, c),
        KeyBinding::new("cmd-a", SelectAll, c),
        KeyBinding::new("enter", Submit, c),
        KeyBinding::new("shift-enter", Newline, c),
        KeyBinding::new("alt-enter", Newline, c),
        KeyBinding::new("escape", Cancel, c),
        KeyBinding::new("cmd-v", Paste, c),
        KeyBinding::new("cmd-c", Copy, c),
        KeyBinding::new("cmd-x", Cut, c),
        KeyBinding::new("cmd-z", Undo, c),
        KeyBinding::new("cmd-shift-z", Redo, c),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, c),
    ]);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextAreaEvent {
    Changed,
    Submit,
    Cancel,
    /// Up in a one-line field (or on the first line).
    MoveUp,
    /// Down in a one-line field (or on the last line).
    MoveDown,
    /// An image was pasted (its file extension and bytes); fields that take images keep it.
    PastedImage {
        extension: &'static str,
        bytes: Vec<u8>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Insert,
    Delete,
    Other,
}

struct Snapshot {
    content: String,
    selected: Range<usize>,
}

struct LayoutCache {
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    lines: Vec<WrappedLine>,
    starts: Vec<usize>,
    tops: Vec<Pixels>,
    height: Pixels,
}

pub struct TextArea {
    focus_handle: FocusHandle,
    content: String,
    placeholder: SharedString,
    selected: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    single_line: bool,
    min_lines: usize,
    max_lines: usize,
    font_size: f32,
    line_height: f32,
    mono: bool,
    layout: Option<LayoutCache>,
    /// The width of the last frame, to wrap at when laying out the next one.
    last_width: Option<Pixels>,
    scroll_y: Pixels,
    reveal_cursor: bool,
    goal_x: Option<Pixels>,
    is_selecting: bool,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last_edit: EditKind,
    disabled: bool,
}

impl EventEmitter<TextAreaEvent> for TextArea {}

impl Focusable for TextArea {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TextArea {
    /// A field of several lines, growing from `min_lines` to `max_lines`.
    pub fn multi_line(min_lines: usize, max_lines: usize, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            placeholder: SharedString::default(),
            selected: 0..0,
            reversed: false,
            marked: None,
            single_line: false,
            min_lines: min_lines.max(1),
            max_lines: max_lines.max(min_lines.max(1)),
            font_size: crate::theme::text::MD,
            line_height: 22.,
            mono: false,
            layout: None,
            last_width: None,
            scroll_y: px(0.),
            reveal_cursor: false,
            goal_x: None,
            is_selecting: false,
            undo: Vec::new(),
            redo: Vec::new(),
            last_edit: EditKind::Other,
            disabled: false,
        }
    }

    pub fn single_line(cx: &mut Context<Self>) -> Self {
        Self {
            single_line: true,
            font_size: crate::theme::text::BASE,
            line_height: 20.,
            ..Self::multi_line(1, 1, cx)
        }
    }

    pub fn with_placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    pub fn set_placeholder(&mut self, placeholder: impl Into<SharedString>, cx: &mut Context<Self>) {
        let placeholder = placeholder.into();
        if placeholder != self.placeholder {
            self.placeholder = placeholder;
            cx.notify();
        }
    }

    pub fn with_font(mut self, font_size: f32, line_height: f32) -> Self {
        self.font_size = font_size;
        self.line_height = line_height;
        self
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    /// Replaces the whole text (undoable), cursor at the end.
    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        let text = text.into();
        if text == self.content {
            return;
        }
        self.push_undo(EditKind::Other);
        self.content = if self.single_line { text.replace('\n', " ") } else { text };
        self.selected = self.content.len()..self.content.len();
        self.reversed = false;
        self.marked = None;
        self.reveal_cursor = true;
        cx.emit(TextAreaEvent::Changed);
        cx.notify();
    }

    /// Empties the field (after sending), keeping undo so the text can come back.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set_text("", cx);
        self.scroll_y = px(0.);
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }

    fn cursor(&self) -> usize {
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }

    fn push_undo(&mut self, kind: EditKind) {
        // Typing (or deleting) in a row is one undo step.
        if kind != EditKind::Other && kind == self.last_edit && !self.undo.is_empty() {
            return;
        }
        self.last_edit = kind;
        self.undo.push(Snapshot {
            content: self.content.clone(),
            selected: self.selected.clone(),
        });
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected = offset..offset;
        self.reversed = false;
        self.reveal_cursor = true;
        self.last_edit = EditKind::Other;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.reversed {
            self.selected.start = offset;
        } else {
            self.selected.end = offset;
        }
        if self.selected.end < self.selected.start {
            self.reversed = !self.reversed;
            self.selected = self.selected.end..self.selected.start;
        }
        self.reveal_cursor = true;
        self.last_edit = EditKind::Other;
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(i, _)| (i < offset).then_some(i))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(i, _)| (i > offset).then_some(i))
            .unwrap_or(self.content.len())
    }

    fn previous_word(&self, offset: usize) -> usize {
        self.content
            .unicode_word_indices()
            .rev()
            .find_map(|(i, _)| (i < offset).then_some(i))
            .unwrap_or(0)
    }

    fn next_word(&self, offset: usize) -> usize {
        self.content
            .unicode_word_indices()
            .find_map(|(i, w)| (i + w.len() > offset).then_some(i + w.len()))
            .unwrap_or(self.content.len())
    }

    fn line_start(&self, offset: usize) -> usize {
        self.content[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }

    fn line_end(&self, offset: usize) -> usize {
        self.content[offset..].find('\n').map(|i| offset + i).unwrap_or(self.content.len())
    }

    /// The offset one visual row up or down from the cursor, or `None` past the edge.
    fn vertical(&mut self, rows: f32) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        let cursor = self.cursor();
        let at = point_for_offset(layout, cursor)?;
        let x = *self.goal_x.get_or_insert(at.x);
        let y = at.y + layout.line_height * rows + layout.line_height / 2.;
        if y < px(0.) || y > layout.height {
            return None;
        }
        Some(offset_for_point(layout, point(x, y)))
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        if self.selected.is_empty() {
            self.move_to(self.previous_boundary(self.cursor()), cx);
        } else {
            self.move_to(self.selected.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        if self.selected.is_empty() {
            self.move_to(self.next_boundary(self.selected.end), cx);
        } else {
            self.move_to(self.selected.end, cx);
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if self.single_line {
            cx.emit(TextAreaEvent::MoveUp);
            return;
        }
        match self.vertical(-1.) {
            Some(offset) => {
                let goal = self.goal_x;
                self.move_to(offset, cx);
                self.goal_x = goal;
            }
            None if self.cursor() == 0 => cx.emit(TextAreaEvent::MoveUp),
            None => self.move_to(0, cx),
        }
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if self.single_line {
            cx.emit(TextAreaEvent::MoveDown);
            return;
        }
        match self.vertical(1.) {
            Some(offset) => {
                let goal = self.goal_x;
                self.move_to(offset, cx);
                self.goal_x = goal;
            }
            None if self.cursor() == self.content.len() => cx.emit(TextAreaEvent::MoveDown),
            None => self.move_to(self.content.len(), cx),
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.select_to(self.previous_boundary(self.cursor()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.select_to(self.next_boundary(self.cursor()), cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical(-1.).unwrap_or(0);
        let goal = self.goal_x;
        self.select_to(target, cx);
        self.goal_x = goal;
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let target = self.vertical(1.).unwrap_or(self.content.len());
        let goal = self.goal_x;
        self.select_to(target, cx);
        self.goal_x = goal;
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.move_to(self.previous_word(self.cursor()), cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.move_to(self.next_word(self.cursor()), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_word(self.cursor()), cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_word(self.cursor()), cx);
    }

    fn line_start_action(&mut self, _: &LineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.move_to(self.line_start(self.cursor()), cx);
    }

    fn line_end_action(&mut self, _: &LineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.move_to(self.line_end(self.cursor()), cx);
    }

    fn select_line_start(&mut self, _: &SelectLineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.line_start(self.cursor()), cx);
    }

    fn select_line_end(&mut self, _: &SelectLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.line_end(self.cursor()), cx);
    }

    fn doc_start(&mut self, _: &DocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn doc_end(&mut self, _: &DocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn select_doc_start(&mut self, _: &SelectDocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_doc_end(&mut self, _: &SelectDocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected = 0..self.content.len();
        self.reversed = false;
        cx.notify();
    }

    fn edit(&mut self, range: Range<usize>, text: &str, kind: EditKind, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        let text = if self.single_line { text.replace('\n', " ") } else { text.to_owned() };
        self.push_undo(kind);
        self.content.replace_range(range.clone(), &text);
        let cursor = range.start + text.len();
        self.selected = cursor..cursor;
        self.reversed = false;
        self.marked = None;
        self.goal_x = None;
        self.reveal_cursor = true;
        cx.emit(TextAreaEvent::Changed);
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected.is_empty() {
            self.previous_boundary(self.cursor())..self.cursor()
        } else {
            self.selected.clone()
        };
        if !range.is_empty() {
            self.edit(range, "", EditKind::Delete, cx);
        }
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected.is_empty() {
            self.cursor()..self.next_boundary(self.cursor())
        } else {
            self.selected.clone()
        };
        if !range.is_empty() {
            self.edit(range, "", EditKind::Delete, cx);
        }
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected.is_empty() {
            self.previous_word(self.cursor())..self.cursor()
        } else {
            self.selected.clone()
        };
        if !range.is_empty() {
            self.edit(range, "", EditKind::Other, cx);
        }
    }

    fn delete_to_line_start(&mut self, _: &DeleteToLineStart, _: &mut Window, cx: &mut Context<Self>) {
        let cursor = self.cursor();
        let start = self.line_start(cursor);
        let range = if start == cursor {
            self.previous_boundary(cursor)..cursor
        } else {
            start..cursor
        };
        if !range.is_empty() {
            self.edit(range, "", EditKind::Other, cx);
        }
    }

    fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        if self.single_line {
            cx.emit(TextAreaEvent::Submit);
            return;
        }
        self.edit(self.selected.clone(), "\n", EditKind::Other, cx);
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        // While an input method composes, Enter confirms the composition.
        if self.marked.is_some() {
            return;
        }
        cx.emit(TextAreaEvent::Submit);
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TextAreaEvent::Cancel);
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else { return };
        if let Some(text) = item.text() {
            self.edit(self.selected.clone(), &text, EditKind::Other, cx);
            return;
        }
        for entry in item.entries() {
            if let gpui::ClipboardEntry::Image(image) = entry {
                let extension = match image.format {
                    gpui::ImageFormat::Png => "png",
                    gpui::ImageFormat::Jpeg => "jpg",
                    gpui::ImageFormat::Gif => "gif",
                    gpui::ImageFormat::Webp => "webp",
                    _ => continue,
                };
                cx.emit(TextAreaEvent::PastedImage {
                    extension,
                    bytes: image.bytes.clone(),
                });
            }
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[self.selected.clone()].to_owned()));
        }
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[self.selected.clone()].to_owned()));
            self.edit(self.selected.clone(), "", EditKind::Other, cx);
        }
    }

    fn undo_action(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(snapshot) = self.undo.pop() {
            self.redo.push(Snapshot {
                content: std::mem::replace(&mut self.content, snapshot.content),
                selected: std::mem::replace(&mut self.selected, snapshot.selected),
            });
            self.last_edit = EditKind::Other;
            self.reveal_cursor = true;
            cx.emit(TextAreaEvent::Changed);
            cx.notify();
        }
    }

    fn redo_action(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(snapshot) = self.redo.pop() {
            self.undo.push(Snapshot {
                content: std::mem::replace(&mut self.content, snapshot.content),
                selected: std::mem::replace(&mut self.selected, snapshot.selected),
            });
            self.last_edit = EditKind::Other;
            self.reveal_cursor = true;
            cx.emit(TextAreaEvent::Changed);
            cx.notify();
        }
    }

    fn show_character_palette(&mut self, _: &ShowCharacterPalette, window: &mut Window, _: &mut Context<Self>) {
        window.show_character_palette();
    }

    fn index_for_mouse(&self, position: Point<Pixels>) -> usize {
        let Some(layout) = self.layout.as_ref() else {
            return 0;
        };
        let local = point(position.x - layout.bounds.left(), position.y - layout.bounds.top() + self.scroll_y);
        offset_for_point(layout, local)
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        self.is_selecting = true;
        self.goal_x = None;
        let offset = self.index_for_mouse(event.position);
        if event.click_count >= 3 {
            self.selected = self.line_start(offset)..self.line_end(offset);
            self.reversed = false;
            cx.notify();
        } else if event.click_count == 2 {
            let start = self.previous_word(self.next_boundary(offset).min(self.content.len()));
            let end = self.next_word(start);
            self.selected = start..end.max(start);
            self.reversed = false;
            cx.notify();
        } else if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse(event.position), cx);
        }
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = self.layout.as_ref() else {
            return;
        };
        let max = (layout.height - layout.bounds.size.height).max(px(0.));
        if max <= px(0.) {
            return;
        }
        let delta = event.delta.pixel_delta(layout.line_height).y;
        self.scroll_y = (self.scroll_y - delta).clamp(px(0.), max);
        cx.stop_propagation();
        cx.notify();
    }

    fn utf16_to_offset(&self, utf16: usize) -> usize {
        let mut utf8 = 0;
        let mut count = 0;
        for ch in self.content.chars() {
            if count >= utf16 {
                break;
            }
            count += ch.len_utf16();
            utf8 += ch.len_utf8();
        }
        utf8
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16 = 0;
        let mut count = 0;
        for ch in self.content.chars() {
            if count >= offset {
                break;
            }
            count += ch.len_utf8();
            utf16 += ch.len_utf16();
        }
        utf16
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.utf16_to_offset(range.start)..self.utf16_to_offset(range.end)
    }
}

fn line_index(layout: &LayoutCache, offset: usize) -> usize {
    match layout.starts.binary_search(&offset) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    }
}

fn point_for_offset(layout: &LayoutCache, offset: usize) -> Option<Point<Pixels>> {
    let i = line_index(layout, offset);
    let line = layout.lines.get(i)?;
    let local = offset.saturating_sub(layout.starts[i]).min(line.len());
    let p = line.position_for_index(local, layout.line_height)?;
    Some(point(p.x, layout.tops[i] + p.y))
}

fn offset_for_point(layout: &LayoutCache, p: Point<Pixels>) -> usize {
    if layout.lines.is_empty() {
        return 0;
    }
    let mut i = 0;
    for (index, top) in layout.tops.iter().enumerate() {
        if p.y >= *top {
            i = index;
        }
    }
    let line = &layout.lines[i];
    let rows = line.wrap_boundaries().len() + 1;
    let line_height = layout.line_height;
    let y = (p.y - layout.tops[i]).clamp(px(0.), line_height * rows as f32 - px(1.));
    let local = match line.closest_index_for_position(point(p.x.max(px(0.)), y), line_height) {
        Ok(ix) | Err(ix) => ix,
    };
    layout.starts[i] + local.min(line.len())
}

impl EntityInputHandler for TextArea {
    fn text_for_range(&mut self, range: Range<usize>, actual: &mut Option<Range<usize>>, _: &mut Window, _: &mut Context<Self>) -> Option<String> {
        let range = self.range_from_utf16(&range);
        actual.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_owned())
    }

    fn selected_text_range(&mut self, _ignore_disabled: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected),
            reversed: self.reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(&mut self, range: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        let range = range
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        let kind = if text.contains(char::is_whitespace) {
            EditKind::Other
        } else {
            EditKind::Insert
        };
        self.edit(range, text, kind, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        new_selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        let range = range
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        self.content.replace_range(range.clone(), text);
        self.marked = (!text.is_empty()).then(|| range.start..range.start + text.len());
        self.selected = new_selected
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|r| r.start + range.start..r.end + range.start)
            .unwrap_or_else(|| range.start + text.len()..range.start + text.len());
        self.reveal_cursor = true;
        cx.emit(TextAreaEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(&mut self, range: Range<usize>, bounds: Bounds<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        let range = self.range_from_utf16(&range);
        let start = point_for_offset(layout, range.start)?;
        let end = point_for_offset(layout, range.end)?;
        let top = bounds.top() - self.scroll_y;
        Some(Bounds::from_corners(
            point(bounds.left() + start.x, top + start.y),
            point(bounds.left() + end.x, top + end.y + layout.line_height),
        ))
    }

    fn character_index_for_point(&mut self, p: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        let offset = self.index_for_mouse(p);
        Some(self.offset_to_utf16(offset))
    }
}

struct TextAreaElement {
    input: Entity<TextArea>,
}

struct Prepaint {
    lines: Vec<(Point<Pixels>, WrappedLine)>,
    selections: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
}

impl IntoElement for TextAreaElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

fn runs_for(text: &str, color: Hsla, marked: Option<&Range<usize>>, window: &Window, mono: bool) -> Vec<TextRun> {
    let mut font = window.text_style().font();
    if mono {
        font.family = crate::assets::MONO_FONT.into();
    }
    let run = TextRun {
        len: text.len(),
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    match marked {
        Some(marked) if marked.end <= text.len() => [
            TextRun {
                len: marked.start,
                ..run.clone()
            },
            TextRun {
                len: marked.end - marked.start,
                underline: Some(UnderlineStyle {
                    color: Some(color),
                    thickness: px(1.),
                    wavy: false,
                }),
                ..run.clone()
            },
            TextRun {
                len: text.len() - marked.end,
                ..run
            },
        ]
        .into_iter()
        .filter(|r| r.len > 0)
        .collect(),
        _ => vec![run],
    }
}

impl Element for TextAreaElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&gpui::InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        // The height follows the text wrapped at the width of the last frame (the first frame
        // guesses one row per line); prepaint asks for another frame when the width changed.
        let input = self.input.read(cx);
        let text: SharedString = if input.content.is_empty() {
            input.placeholder.clone()
        } else {
            input.content.clone().into()
        };
        let font_size = px(input.font_size);
        let line_height = px(input.line_height);
        let rows: usize = match input.last_width {
            Some(width) => {
                let runs = runs_for(&text, gpui::black(), None, window, input.mono);
                text.split('\n')
                    .map(|line| {
                        let runs = [TextRun {
                            len: line.len(),
                            ..runs[0].clone()
                        }];
                        window
                            .text_system()
                            .shape_text(line.to_owned().into(), font_size, &runs, Some(width), None)
                            .map(|lines| lines.iter().map(|l| l.wrap_boundaries().len() + 1).sum::<usize>())
                            .unwrap_or(1)
                    })
                    .sum()
            }
            None => text.split('\n').count(),
        };
        let rows = rows.clamp(input.min_lines, input.max_lines);
        // The field stretches across its column (the root is a flex column), whatever its
        // content's own width.
        let mut style = Style::default();
        style.size.height = (line_height * rows as f32).into();
        style.min_size.width = px(0.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let theme = cx.theme().colors.clone();
        let input = self.input.read(cx);
        let line_height = px(input.line_height);
        let font_size = px(input.font_size);
        let showing_placeholder = input.content.is_empty();
        let text = if showing_placeholder {
            input.placeholder.to_string()
        } else {
            input.content.clone()
        };
        let color = if showing_placeholder {
            // The web's `--placeholder` at 75% (`text-placeholder/75`).
            theme.text_2.opacity(0.75)
        } else if input.disabled {
            theme.text_2
        } else {
            theme.text
        };
        let marked = input.marked.clone();
        let selected = input.selected.clone();
        let cursor = input.cursor();
        let mono = input.mono;
        let focused = input.focus_handle.is_focused(window);

        let mut lines = Vec::new();
        let mut starts = Vec::new();
        let mut tops = Vec::new();
        let mut y = px(0.);
        let mut offset = 0;
        for line_text in text.split('\n') {
            let line_marked = marked
                .as_ref()
                .filter(|m| m.start >= offset && m.end <= offset + line_text.len())
                .map(|m| m.start - offset..m.end - offset);
            let runs = runs_for(line_text, color, line_marked.as_ref(), window, mono);
            let shaped = window
                .text_system()
                .shape_text(line_text.to_owned().into(), font_size, &runs, Some(bounds.size.width), None)
                .ok()
                .and_then(|mut l| (!l.is_empty()).then(|| l.remove(0)));
            if let Some(shaped) = shaped {
                let rows = shaped.wrap_boundaries().len() + 1;
                starts.push(offset);
                tops.push(y);
                y += line_height * rows as f32;
                lines.push(shaped);
            }
            offset += line_text.len() + 1;
        }
        let layout = LayoutCache {
            bounds,
            line_height,
            lines: lines.clone(),
            starts,
            tops,
            height: y,
        };

        // Keep the cursor in view.
        let mut scroll_y = input.scroll_y;
        let max_scroll = (layout.height - bounds.size.height).max(px(0.));
        if input.reveal_cursor && !showing_placeholder {
            if let Some(at) = point_for_offset(&layout, cursor) {
                if at.y < scroll_y {
                    scroll_y = at.y;
                } else if at.y + line_height > scroll_y + bounds.size.height {
                    scroll_y = at.y + line_height - bounds.size.height;
                }
            }
        }
        scroll_y = scroll_y.clamp(px(0.), max_scroll);
        let origin = point(bounds.left(), bounds.top() - scroll_y);

        let mut selections = Vec::new();
        let mut cursor_quad = None;
        if !showing_placeholder {
            if selected.is_empty() {
                if focused {
                    if let Some(at) = point_for_offset(&layout, cursor) {
                        cursor_quad = Some(fill(
                            Bounds::new(point(origin.x + at.x, origin.y + at.y + px(2.)), size(px(1.5), line_height - px(4.))),
                            theme.accent,
                        ));
                    }
                }
            } else if let (Some(start), Some(end)) = (point_for_offset(&layout, selected.start), point_for_offset(&layout, selected.end)) {
                let selection = if focused { theme.accent_soft } else { theme.hover };
                let mut row_y = start.y;
                while row_y <= end.y {
                    let left = if row_y == start.y { start.x } else { px(0.) };
                    let right = if row_y == end.y { end.x } else { bounds.size.width };
                    if right > left || row_y != end.y {
                        selections.push(fill(
                            Bounds::from_corners(
                                point(origin.x + left, origin.y + row_y),
                                point(origin.x + right.max(left + px(4.)), origin.y + row_y + line_height),
                            ),
                            selection,
                        ));
                    }
                    row_y += line_height;
                }
            }
        }

        let positioned = lines
            .into_iter()
            .zip(layout.tops.iter())
            .map(|(line, top)| (point(origin.x, origin.y + *top), line))
            .collect();
        let width = bounds.size.width;
        self.input.update(cx, |input, cx| {
            input.scroll_y = scroll_y;
            input.reveal_cursor = false;
            input.layout = Some(layout);
            if width > px(0.) && input.last_width != Some(width) {
                input.last_width = Some(width);
                cx.notify();
            }
        });
        Prepaint {
            lines: positioned,
            selections,
            cursor: cursor_quad,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus_handle.clone();
        let line_height = px(self.input.read(cx).line_height);
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.input.clone()), cx);
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for quad in prepaint.selections.drain(..) {
                window.paint_quad(quad);
            }
            for (origin, line) in &prepaint.lines {
                let _ = line.paint(*origin, line_height, TextAlign::Left, None, window, cx);
            }
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        });
    }
}

impl Render for TextArea {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .w_full()
            .min_w_0()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .text_size(px(self.font_size))
            .line_height(px(self.line_height))
            .when(self.mono, |this| this.font_family(crate::assets::MONO_FONT))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_to_line_start))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::line_start_action))
            .on_action(cx.listener(Self::line_end_action))
            .on_action(cx.listener(Self::select_line_start))
            .on_action(cx.listener(Self::select_line_end))
            .on_action(cx.listener(Self::doc_start))
            .on_action(cx.listener(Self::doc_end))
            .on_action(cx.listener(Self::select_doc_start))
            .on_action(cx.listener(Self::select_doc_end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::undo_action))
            .on_action(cx.listener(Self::redo_action))
            .on_action(cx.listener(Self::show_character_palette))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(TextAreaElement { input: cx.entity() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Subscription, TestAppContext};

    struct Host {
        field: Entity<TextArea>,
        events: Vec<TextAreaEvent>,
        _subscription: Subscription,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.field.clone())
        }
    }

    fn host(cx: &mut TestAppContext, single_line: bool) -> (Entity<Host>, &mut gpui::VisualTestContext) {
        cx.update(|cx| {
            cx.set_global(crate::theme::Theme::new(crate::theme::Mode::Dark, false));
            bind_keys(cx);
        });
        let (host, cx) = cx.add_window_view(|window, cx| {
            let field = cx.new(|cx| {
                if single_line {
                    TextArea::single_line(cx)
                } else {
                    TextArea::multi_line(1, 8, cx)
                }
            });
            let subscription = cx.subscribe(&field, |host: &mut Host, _, event: &TextAreaEvent, _| host.events.push(event.clone()));
            field.read(cx).focus(window);
            Host {
                field,
                events: Vec::new(),
                _subscription: subscription,
            }
        });
        cx.run_until_parked();
        (host, cx)
    }

    fn text(host: &Entity<Host>, cx: &mut gpui::VisualTestContext) -> String {
        host.read_with(cx, |h, cx| h.field.read(cx).text().to_owned())
    }

    #[gpui::test]
    fn typing_editing_and_undo(cx: &mut TestAppContext) {
        let (host, cx) = host(cx, false);
        cx.simulate_input("hello world");
        assert_eq!(text(&host, cx), "hello world");
        cx.simulate_keystrokes("alt-backspace");
        assert_eq!(text(&host, cx), "hello ");
        cx.simulate_keystrokes("shift-enter");
        cx.simulate_input("second line");
        assert_eq!(text(&host, cx), "hello \nsecond line");
        cx.simulate_keystrokes("cmd-left");
        cx.simulate_input(">");
        assert_eq!(text(&host, cx), "hello \n>second line");
        cx.simulate_keystrokes("cmd-z");
        assert_eq!(text(&host, cx), "hello \nsecond line");
        cx.simulate_keystrokes("cmd-a backspace");
        assert_eq!(text(&host, cx), "");
        cx.simulate_keystrokes("enter");
        assert!(host.read_with(cx, |h, _| h.events.contains(&TextAreaEvent::Submit)));
    }

    #[gpui::test]
    fn one_line_fields_hand_up_and_down_to_their_list(cx: &mut TestAppContext) {
        let (host, cx) = host(cx, true);
        cx.simulate_input("pal");
        cx.simulate_keystrokes("down down up shift-enter escape");
        let events = host.read_with(cx, |h, _| h.events.clone());
        assert_eq!(text(&host, cx), "pal");
        assert!(events.contains(&TextAreaEvent::MoveDown));
        assert!(events.contains(&TextAreaEvent::MoveUp));
        // Shift-Enter in a one-line field submits instead of breaking the line.
        assert!(events.contains(&TextAreaEvent::Submit));
        assert!(events.contains(&TextAreaEvent::Cancel));
    }
}
