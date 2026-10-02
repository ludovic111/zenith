//! A thread's terminal: a shell the server runs where the thread works (`terminal.open`),
//! its output followed through `terminal.attach` into a `vt100` screen drawn here in the
//! mono font, and what you type sent back in order (`terminal.write`). Project scripts run
//! in their own terminal of the thread.

use gpui::prelude::*;
use gpui::{
    canvas, div, font, px, App, Bounds, ClipboardItem, Context, Entity, FocusHandle, Focusable, FontWeight, HighlightStyle, Hsla, KeyDownEvent, Pixels,
    ScrollWheelEvent, SharedString, StyledText, Task, Window,
};
use serde_json::{json, Value};
use zc_contracts::ThreadId;
use zenith_client::StreamEvent;

use crate::assets::MONO_FONT;
use crate::store::{self, Store};
use crate::theme::{text, ActiveTheme, Mode};

const FONT_SIZE: f32 = 12.;
const LINE_HEIGHT: f32 = 17.;
const SCROLLBACK: usize = 5000;

#[derive(Clone, PartialEq)]
pub enum TermStatus {
    Starting,
    Running,
    Exited(Option<i64>),
    Failed(String),
}

pub struct TerminalPanel {
    store: Entity<Store>,
    pub thread: ThreadId,
    pub terminal_id: String,
    pub title: SharedString,
    pub status: TermStatus,
    parser: vt100::Parser,
    focus: FocusHandle,
    /// Columns and rows that fit the panel, once laid out.
    fitted: Option<(u16, u16)>,
    writer: futures::channel::mpsc::UnboundedSender<String>,
    _stream: Option<Task<()>>,
}

impl TerminalPanel {
    /// Opens (or reuses) the thread's terminal `terminal_id`; `command` is typed into it once open.
    pub fn new(thread: ThreadId, terminal_id: String, title: impl Into<SharedString>, command: Option<String>, cx: &mut Context<Self>) -> Self {
        let store = store::store(cx);
        let client = store.read(cx).client.clone();
        let (writer, mut queue) = futures::channel::mpsc::unbounded::<String>();
        let (thread_for_writer, terminal_for_writer) = (thread.as_str().to_owned(), terminal_id.clone());
        // One writer, so keystrokes reach the shell in the order typed.
        crate::runtime::spawn(async move {
            use futures::StreamExt;
            while let Some(mut data) = queue.next().await {
                while let Ok(Some(more)) = queue.try_recv().map(Some) {
                    data.push_str(&more);
                }
                let payload = json!({"threadId": thread_for_writer, "terminalId": terminal_for_writer, "data": data});
                if let Err(error) = client.call("terminal.write", payload).await {
                    tracing::warn!("terminal.write: {error}");
                }
            }
        });
        let mut this = Self {
            store,
            thread,
            terminal_id,
            title: title.into(),
            status: TermStatus::Starting,
            parser: vt100::Parser::new(32, 120, SCROLLBACK),
            focus: cx.focus_handle(),
            fitted: None,
            writer,
            _stream: None,
        };
        this.open(command, cx);
        this
    }

    fn open(&mut self, command: Option<String>, cx: &mut Context<Self>) {
        let (rows, cols) = self.fitted.map(|(c, r)| (r, c)).unwrap_or((32, 120));
        let params = json!({"threadId": self.thread.as_str(), "terminalId": self.terminal_id, "cols": cols, "rows": rows});
        let opened = self.store.update(cx, |s, cx| s.run_command("terminal.open", params, cx));
        cx.spawn(async move |this, cx| {
            let result = opened.await;
            this.update(cx, |this, cx| match result {
                Ok(_) => {
                    this.attach(cx);
                    if let Some(command) = command {
                        this.write(format!("{command}\r"));
                    }
                }
                Err(error) => {
                    this.status = TermStatus::Failed(error);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn attach(&mut self, cx: &mut Context<Self>) {
        let client = self.store.read(cx).client.clone();
        let mut stream = client.stream("terminal.attach", json!({"threadId": self.thread.as_str(), "terminalId": self.terminal_id}));
        self._stream = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = stream.next().await {
                let StreamEvent::Item(item) = event else { break };
                if this.update(cx, |this, cx| this.apply(item, cx)).is_err() {
                    return;
                }
            }
        }));
    }

    fn apply(&mut self, event: Value, cx: &mut Context<Self>) {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or_default();
        match kind {
            "snapshot" | "started" | "restarted" => {
                let snapshot = event.get("snapshot").cloned().unwrap_or(Value::Null);
                let (rows, cols) = self.parser.screen().size();
                self.parser = vt100::Parser::new(rows, cols, SCROLLBACK);
                if let Some(history) = snapshot.get("history").and_then(Value::as_str) {
                    self.parser.process(history.as_bytes());
                }
                self.status = match snapshot.get("status").and_then(Value::as_str) {
                    Some("exited") => TermStatus::Exited(snapshot.get("exitCode").and_then(Value::as_i64)),
                    Some("error") => TermStatus::Failed("the terminal failed to start".into()),
                    Some("starting") => TermStatus::Starting,
                    _ => TermStatus::Running,
                };
            }
            "output" => {
                if let Some(data) = event.get("data").and_then(Value::as_str) {
                    self.parser.process(data.as_bytes());
                }
                if self.status == TermStatus::Starting {
                    self.status = TermStatus::Running;
                }
            }
            "cleared" => {
                let (rows, cols) = self.parser.screen().size();
                self.parser = vt100::Parser::new(rows, cols, SCROLLBACK);
            }
            "exited" => self.status = TermStatus::Exited(event.get("exitCode").and_then(Value::as_i64)),
            "closed" => self.status = TermStatus::Exited(None),
            "error" => self.status = TermStatus::Failed(event.get("message").and_then(Value::as_str).unwrap_or("error").to_owned()),
            _ => return,
        }
        cx.notify();
    }

    pub fn write(&self, data: String) {
        let _ = self.writer.unbounded_send(data);
    }

    /// Closes the shell on the server (and what runs in it).
    pub fn close(&self, cx: &mut App) {
        let params = json!({"threadId": self.thread.as_str(), "terminalId": self.terminal_id});
        self.store.update(cx, |s, cx| s.run_command("terminal.close", params, cx)).detach();
    }

    pub fn restart(&mut self, cx: &mut Context<Self>) {
        self.status = TermStatus::Starting;
        self.open(None, cx);
        cx.notify();
    }

    pub fn running(&self) -> bool {
        matches!(self.status, TermStatus::Running | TermStatus::Starting)
    }

    /// What the screen shows, as plain text.
    pub fn contents(&self) -> String {
        self.parser.screen().contents()
    }

    fn fit(&mut self, bounds: Bounds<Pixels>, cell_width: Pixels, cx: &mut Context<Self>) {
        let cols = ((bounds.size.width / cell_width).floor() as u16).max(20);
        let rows = ((bounds.size.height / px(LINE_HEIGHT)).floor() as u16).max(4);
        if self.fitted == Some((cols, rows)) {
            return;
        }
        self.fitted = Some((cols, rows));
        self.parser.screen_mut().set_size(rows, cols);
        let params = json!({"threadId": self.thread.as_str(), "terminalId": self.terminal_id, "cols": cols, "rows": rows});
        self.store.update(cx, |s, cx| s.call("terminal.resize", params, cx)).detach();
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let m = keystroke.modifiers;
        if m.platform {
            if keystroke.key == "v" {
                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    let text = text.replace("\r\n", "\r").replace('\n', "\r");
                    self.write(if self.parser.screen().bracketed_paste() {
                        format!("\x1b[200~{text}\x1b[201~")
                    } else {
                        text
                    });
                }
                cx.stop_propagation();
            } else if keystroke.key == "c" {
                cx.write_to_clipboard(ClipboardItem::new_string(self.contents()));
                cx.stop_propagation();
            }
            // Other ⌘ shortcuts stay the app's.
            return;
        }
        let app_cursor = self.parser.screen().application_cursor();
        let arrow = |c: char| if app_cursor { format!("\x1bO{c}") } else { format!("\x1b[{c}") };
        let data = match keystroke.key.as_str() {
            "enter" => Some("\r".to_owned()),
            "backspace" => Some(if m.alt { "\x1b\x7f".to_owned() } else { "\x7f".to_owned() }),
            "tab" => Some(if m.shift { "\x1b[Z".to_owned() } else { "\t".to_owned() }),
            "escape" => Some("\x1b".to_owned()),
            "up" => Some(arrow('A')),
            "down" => Some(arrow('B')),
            "right" => Some(if m.alt { "\x1bf".to_owned() } else { arrow('C') }),
            "left" => Some(if m.alt { "\x1bb".to_owned() } else { arrow('D') }),
            "home" => Some("\x1b[H".to_owned()),
            "end" => Some("\x1b[F".to_owned()),
            "delete" => Some("\x1b[3~".to_owned()),
            "pageup" => Some("\x1b[5~".to_owned()),
            "pagedown" => Some("\x1b[6~".to_owned()),
            key if m.control && key.chars().count() == 1 => {
                let c = key.chars().next().unwrap_or_default().to_ascii_lowercase();
                match c {
                    'a'..='z' => Some(((c as u8 - b'a' + 1) as char).to_string()),
                    '[' => Some("\x1b".into()),
                    '\\' => Some("\x1c".into()),
                    ']' => Some("\x1d".into()),
                    _ => None,
                }
            }
            "space" if m.control => Some("\0".to_owned()),
            _ => keystroke.key_char.clone().map(|c| if m.alt { format!("\x1b{c}") } else { c }),
        };
        if let Some(data) = data {
            // Typing goes to the bottom of the output.
            self.parser.screen_mut().set_scrollback(0);
            self.write(data);
            cx.stop_propagation();
            window.prevent_default();
            cx.notify();
        }
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let lines = match event.delta {
            gpui::ScrollDelta::Lines(p) => p.y,
            gpui::ScrollDelta::Pixels(p) => f32::from(p.y) / LINE_HEIGHT,
        };
        let current = self.parser.screen().scrollback() as f32;
        let next = (current + lines).max(0.).round() as usize;
        self.parser.screen_mut().set_scrollback(next);
        cx.notify();
    }
}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The 16 ANSI colors, then the xterm cube and grays.
fn ansi(index: u8, dark: bool, cx: &App) -> Hsla {
    let c = &cx.theme().colors;
    let rgb = |r: u8, g: u8, b: u8| -> Hsla {
        gpui::Rgba {
            r: r as f32 / 255.,
            g: g as f32 / 255.,
            b: b as f32 / 255.,
            a: 1.,
        }
        .into()
    };
    match index {
        0 => {
            if dark {
                rgb(0x3b, 0x3f, 0x4a)
            } else {
                rgb(0x1f, 0x23, 0x2b)
            }
        }
        1 | 9 => c.danger,
        2 | 10 => c.success,
        3 | 11 => c.warning,
        4 | 12 => c.accent_text,
        5 => rgb(0xb3, 0x6b, 0xd6),
        13 => rgb(0xc8, 0x8a, 0xe6),
        6 => rgb(0x2f, 0xa8, 0xb8),
        14 => rgb(0x4c, 0xc4, 0xd2),
        7 => c.text_2,
        8 => c.text_3,
        15 => c.text,
        16..=231 => {
            let i = index - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            rgb(level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        _ => {
            let v = 8 + (index - 232) * 10;
            rgb(v, v, v)
        }
    }
}

fn color(value: vt100::Color, dark: bool, cx: &App) -> Option<Hsla> {
    match value {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(ansi(i, dark, cx)),
        vt100::Color::Rgb(r, g, b) => Some(
            gpui::Rgba {
                r: r as f32 / 255.,
                g: g as f32 / 255.,
                b: b as f32 / 255.,
                a: 1.,
            }
            .into(),
        ),
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = cx.theme().colors.clone();
        let dark = cx.theme().mode == Mode::Dark;
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&font(MONO_FONT));
        let cell_width = text_system.advance(font_id, px(FONT_SIZE), 'm').map(|s| s.width).unwrap_or(px(7.2));

        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let scrolled = screen.scrollback() > 0;
        let cursor = (!screen.hide_cursor() && !scrolled && self.running()).then(|| screen.cursor_position());
        let focused = self.focus.is_focused(window);
        let mut lines = Vec::with_capacity(rows as usize);
        for row in 0..rows {
            let mut line = String::new();
            let mut highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = Vec::new();
            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else { break };
                if cell.is_wide_continuation() {
                    continue;
                }
                let start = line.len();
                if cell.has_contents() {
                    line.push_str(cell.contents());
                } else {
                    line.push(' ');
                }
                let (mut fg, mut bg) = (color(cell.fgcolor(), dark, cx), color(cell.bgcolor(), dark, cx));
                if cell.inverse() {
                    (fg, bg) = (Some(bg.unwrap_or(c.bg_sunken)), Some(fg.unwrap_or(c.text)));
                }
                let mut style = HighlightStyle {
                    color: fg,
                    background_color: bg,
                    ..Default::default()
                };
                if cell.bold() {
                    style.font_weight = Some(FontWeight::BOLD);
                }
                if cell.dim() {
                    style.fade_out = Some(0.4);
                }
                if cursor == Some((row, col)) && focused {
                    style.color = Some(c.bg_sunken);
                    style.background_color = Some(c.text);
                }
                if style != HighlightStyle::default() {
                    let range = start..line.len();
                    match highlights.last_mut() {
                        Some((last, last_style)) if last.end == range.start && *last_style == style => last.end = range.end,
                        _ => highlights.push((range, style)),
                    }
                }
            }
            let trimmed = line.trim_end().len().max(highlights.last().map(|(r, _)| r.end).unwrap_or(0));
            line.truncate(trimmed);
            lines.push(
                div()
                    .h(px(LINE_HEIGHT))
                    .whitespace_nowrap()
                    .child(StyledText::new(SharedString::from(line)).with_highlights(highlights)),
            );
        }
        let view = cx.entity().downgrade();
        let unfocused_cursor = cursor.filter(|_| !focused);

        div()
            .id(SharedString::from(format!("terminal-{}", self.terminal_id)))
            .key_context("Terminal")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus);
                    cx.notify();
                }),
            )
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(c.bg_sunken)
            .px(px(10.))
            .py(px(6.))
            .font_family(MONO_FONT)
            .text_size(px(FONT_SIZE))
            .line_height(px(LINE_HEIGHT))
            .text_color(c.text)
            .child(
                div()
                    .size_full()
                    .relative()
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                view.update(cx, |this, cx| this.fit(bounds, cell_width, cx)).ok();
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .children(lines)
                    .when_some(unfocused_cursor, |this, (row, col)| {
                        this.child(
                            div()
                                .absolute()
                                .top(px(row as f32 * LINE_HEIGHT))
                                .left(cell_width * col as f32)
                                .w(cell_width)
                                .h(px(LINE_HEIGHT))
                                .border_1()
                                .border_color(c.text_3),
                        )
                    }),
            )
            .when(scrolled, |this| {
                this.child(
                    div()
                        .absolute()
                        .bottom(px(8.))
                        .right(px(12.))
                        .px(px(8.))
                        .py(px(2.))
                        .rounded(px(6.))
                        .bg(c.glass_opaque)
                        .text_size(px(text::XS))
                        .text_color(c.text_2)
                        .font_family(crate::assets::UI_FONT)
                        .child("Scrolled up — type to go back down"),
                )
            })
    }
}
