//! The command palette (⌘K): every command of the window, a new thread in any project, and
//! every thread, found by typing a few letters of them.

use gpui::prelude::*;
use gpui::{div, px, Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, SharedString, Subscription, Window};
use zc_contracts::{ProjectId, ThreadId};
use zenith_model::shell::{self, ThreadStatus};

use crate::actions;
use crate::assets::Icon;
use crate::store;
use crate::theme::{radius, text, ActiveTheme, Appearance};
use crate::ui::text_area::{TextArea, TextAreaEvent};
use crate::ui::{icon, kbd};
use crate::workspace::Route;

pub struct PaletteContext {
    pub thread: Option<ThreadId>,
    pub project: Option<ProjectId>,
}

pub enum PaletteEvent {
    Dismissed,
    Navigate(Route),
    NewThread(Option<ProjectId>),
    AddProject,
    Appearance(Appearance),
    Action(Box<dyn Action>),
    Script(String),
}

enum Run {
    Navigate(Route),
    NewThread(Option<ProjectId>),
    AddProject,
    Appearance(Appearance),
    Action(Box<dyn Action>),
    Script(String),
}

struct Item {
    group: &'static str,
    label: SharedString,
    detail: Option<SharedString>,
    icon: Icon,
    keys: Option<&'static str>,
    run: Run,
}

pub struct Palette {
    input: Entity<TextArea>,
    items: Vec<Item>,
    matches: Vec<usize>,
    selected: usize,
    focus_handle: FocusHandle,
    _subscription: Subscription,
}

impl EventEmitter<PaletteEvent> for Palette {}

impl Focusable for Palette {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Subsequence match, with a bonus for word starts and consecutive letters; `None` if the
/// query's letters are not all there in order.
pub fn score(query: &str, target: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let target_lower: Vec<char> = target.to_lowercase().chars().collect();
    let mut score = 0i64;
    let mut position = 0usize;
    let mut previous: Option<usize> = None;
    for q in query.to_lowercase().chars().filter(|c| !c.is_whitespace()) {
        let found = (position..target_lower.len()).find(|&i| target_lower[i] == q)?;
        score += 10;
        if found == 0 || !target_lower[found - 1].is_alphanumeric() {
            score += 8;
        }
        if previous.is_some_and(|p| p + 1 == found) {
            score += 6;
        }
        score -= (found - position) as i64;
        previous = Some(found);
        position = found + 1;
    }
    Some(score - target_lower.len() as i64 / 8)
}

impl Palette {
    pub fn new(context: PaletteContext, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            TextArea::single_line(cx)
                .with_placeholder("Type a command, a project or a thread")
                .with_font(text::MD, 22.)
        });
        let subscription = cx.subscribe_in(&input, window, |this, _, event: &TextAreaEvent, _, cx| match event {
            TextAreaEvent::Changed => this.filter(cx),
            TextAreaEvent::MoveDown => {
                if !this.matches.is_empty() {
                    this.selected = (this.selected + 1) % this.matches.len();
                    cx.notify();
                }
            }
            TextAreaEvent::MoveUp => {
                if !this.matches.is_empty() {
                    this.selected = (this.selected + this.matches.len() - 1) % this.matches.len();
                    cx.notify();
                }
            }
            TextAreaEvent::Submit => this.confirm(this.selected, cx),
            TextAreaEvent::Cancel => cx.emit(PaletteEvent::Dismissed),
            TextAreaEvent::PastedImage { .. } => {}
        });
        let mut this = Self {
            input,
            items: Self::items(&context, cx),
            matches: Vec::new(),
            selected: 0,
            focus_handle: cx.focus_handle(),
            _subscription: subscription,
        };
        this.filter(cx);
        this
    }

    fn items(context: &PaletteContext, cx: &App) -> Vec<Item> {
        let store = store::store(cx);
        let store = store.read(cx);
        let mut items = Vec::new();
        let mut command = |label: &str, icon: Icon, keys: Option<&'static str>, run: Run| {
            items.push(Item {
                group: "Commands",
                label: label.to_owned().into(),
                detail: None,
                icon,
                keys,
                run,
            });
        };
        command("New thread", Icon::NewThread, Some("⌘N"), Run::NewThread(context.project.clone()));
        command("Add project…", Icon::FolderPlus, Some("⌘O"), Run::AddProject);
        command("Settings", Icon::Settings, Some("⌘,"), Run::Navigate(Route::Settings));
        command("Sessions & costs", Icon::Coins, Some("⌘U"), Run::Navigate(Route::Sessions));
        command("Toggle the sidebar", Icon::PanelLeft, Some("⌘B"), Run::Action(Box::new(actions::ToggleSidebar)));
        if context.thread.is_some() {
            command("Stop the agent", Icon::CircleStop, Some("⌘."), Run::Action(Box::new(actions::StopTurn)));
            command("Pin or unpin this thread", Icon::Pin, Some("⌘⇧I"), Run::Action(Box::new(actions::PinThread)));
            command(
                "Settle or reopen this thread",
                Icon::CircleCheck,
                Some("⌘⇧S"),
                Run::Action(Box::new(actions::SettleThread)),
            );
            command("Archive this thread", Icon::Archive, None, Run::Action(Box::new(actions::ArchiveThread)));
            command(
                "Show or hide the terminal",
                Icon::SquareTerminal,
                Some("⌘J"),
                Run::Action(Box::new(actions::ToggleTerminal)),
            );
            command("Git: commit", Icon::GitCommit, Some("⌥⌘C"), Run::Action(Box::new(actions::GitCommit)));
            command("Git: commit and push", Icon::Push, None, Run::Action(Box::new(actions::GitCommitPush)));
            command(
                "Git: commit, push and open a pull request",
                Icon::PullRequest,
                None,
                Run::Action(Box::new(actions::GitCommitPushPr)),
            );
            command("Git: push", Icon::Push, None, Run::Action(Box::new(actions::GitPush)));
            command("Git: pull", Icon::Pull, None, Run::Action(Box::new(actions::GitPull)));
            let scripts = context
                .thread
                .as_ref()
                .and_then(|id| store.shell.thread(id))
                .and_then(|t| store.shell.project(&t.project_id))
                .map(|p| p.scripts.clone())
                .unwrap_or_default();
            for script in scripts {
                command(&format!("Run: {}", script.name), Icon::Play, None, Run::Script(script.id.to_string()));
            }
        }
        command("Appearance: follow the system", Icon::MoonStar, None, Run::Appearance(Appearance::System));
        command("Appearance: light", Icon::Sun, None, Run::Appearance(Appearance::Light));
        command("Appearance: dark", Icon::Moon, None, Run::Appearance(Appearance::Dark));
        command(
            "Open zenith in the browser",
            Icon::Globe,
            Some("⌥⌘O"),
            Run::Action(Box::new(actions::OpenInBrowser)),
        );
        command("Check for updates…", Icon::Download, None, Run::Action(Box::new(actions::CheckForUpdates)));
        command("Show the server log", Icon::FileText, None, Run::Action(Box::new(actions::ShowServerLog)));
        command(
            "Reconnect to the server",
            Icon::Refresh,
            Some("⌘⇧R"),
            Run::Action(Box::new(actions::ReloadConnection)),
        );

        for project in store.shell.projects_sorted() {
            items.push(Item {
                group: "New thread in",
                label: project.title.clone().into(),
                detail: Some(project.workspace_root.clone().into()),
                icon: Icon::Folder,
                keys: None,
                run: Run::NewThread(Some(project.id.clone())),
            });
        }
        let mut threads: Vec<_> = store.shell.threads.iter().filter(|t| t.archived_at.is_none()).collect();
        threads.sort_by_key(|t| std::cmp::Reverse(shell::activity_at(t)));
        for thread in threads {
            let project = store.shell.project(&thread.project_id).map(|p| p.title.clone()).unwrap_or_default();
            let status = shell::status(thread);
            items.push(Item {
                group: "Threads",
                label: thread.title.clone().into(),
                detail: Some(if status == ThreadStatus::Ready {
                    project.into()
                } else {
                    format!("{project} · {}", status.label()).into()
                }),
                icon: Icon::Message,
                keys: None,
                run: Run::Navigate(Route::Thread(thread.id.clone())),
            });
        }
        items
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).text().trim().to_owned();
        let mut scored: Vec<(i64, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let haystack = match &item.detail {
                    Some(detail) if item.group != "Commands" => format!("{} {}", item.label, detail),
                    _ => item.label.to_string(),
                };
                // Threads only show once something is typed (the list stays short).
                if query.is_empty() && item.group == "Threads" {
                    return None;
                }
                score(&query, &haystack).map(|s| (s, i))
            })
            .collect();
        if !query.is_empty() {
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        self.matches = scored.into_iter().map(|(_, i)| i).take(60).collect();
        self.selected = 0;
        cx.notify();
    }

    fn confirm(&mut self, position: usize, cx: &mut Context<Self>) {
        let Some(&index) = self.matches.get(position) else { return };
        let event = match &self.items[index].run {
            Run::Navigate(route) => PaletteEvent::Navigate(route.clone()),
            Run::NewThread(project) => PaletteEvent::NewThread(project.clone()),
            Run::AddProject => PaletteEvent::AddProject,
            Run::Appearance(a) => PaletteEvent::Appearance(*a),
            Run::Action(action) => PaletteEvent::Action(action.boxed_clone()),
            Run::Script(id) => PaletteEvent::Script(id.clone()),
        };
        cx.emit(event);
    }

    pub fn focus(&self, window: &mut Window, cx: &App) {
        self.input.read(cx).focus(window);
    }
}

impl Render for Palette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let c = theme.colors.clone();
        let mut rows = Vec::new();
        let mut last_group = "";
        for (position, &index) in self.matches.iter().enumerate() {
            let item = &self.items[index];
            if item.group != last_group {
                last_group = item.group;
                rows.push(
                    div()
                        .px(px(12.))
                        .pt(px(10.))
                        .pb(px(4.))
                        .text_size(px(text::XS))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(c.text_3)
                        .child(item.group)
                        .into_any_element(),
                );
            }
            let selected = position == self.selected;
            rows.push(
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
                    .child(icon(item.icon, if selected { c.accent_text } else { c.text_3 }))
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(380.))
                            .truncate()
                            .text_size(px(text::BASE))
                            .text_color(c.text)
                            .child(item.label.clone()),
                    )
                    .when_some(item.detail.clone(), |this, detail| {
                        this.child(div().flex_1().min_w_0().truncate().text_size(px(text::SM)).text_color(c.text_3).child(detail))
                    })
                    .when(item.detail.is_none(), |this| this.child(div().flex_1()))
                    .when_some(item.keys, |this, keys| this.child(kbd(keys, cx)))
                    .into_any_element(),
            );
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
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(16.))
                    .h(px(52.))
                    .border_b_1()
                    .border_color(c.line)
                    .child(icon(Icon::Command, c.text_3).size(px(16.)))
                    .child(div().flex_1().child(self.input.clone())),
            )
            .child(
                div()
                    .id("palette-results")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .pb(px(8.))
                    .children(rows)
                    .when(self.matches.is_empty(), |this| {
                        this.child(div().p(px(16.)).text_size(px(text::BASE)).text_color(c.text_3).child("Nothing matches."))
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn fuzzy_scores() {
        assert!(score("nt", "New thread").is_some());
        assert!(score("zz", "New thread").is_none());
        assert!(score("set", "Settings").unwrap() > score("set", "Sessions & costs, settle").unwrap_or(i64::MIN));
    }
}
