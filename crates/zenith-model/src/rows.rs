//! A thread's timeline as the web shows it, row by row (`deriveMessagesTimelineRows` in
//! `apps/web/src/components/chat/MessagesTimeline.logic.ts`):
//!
//! - a settled turn folds its work before the final answer behind "Worked for 7m 7s";
//! - thinking interleaved with tool calls of one turn is an activity group;
//! - consecutive work entries are one tool call, or a toggle ("Ran 3 commands",
//!   "Received 2 updates") that opens into them;
//! - while the agent works: "Working for …" after the last message, the running tool, and
//!   "Thinking" at the end;
//! - an answer followed by tool calls of its turn gets its time and copy button after them.

use std::collections::{HashMap, HashSet};

use zc_contracts::{
    OrchestrationCheckpointSummary, OrchestrationLatestTurnState, OrchestrationMessage, OrchestrationMessageRole, OrchestrationProposedPlan,
    OrchestrationSessionStatus, OrchestrationThread,
};

use crate::time::{format_duration, millis};
use crate::worklog::{self, Action, Tone, ToolStatus, WorkEntry};

/// A message, a proposed plan or a work entry, in time order (`TimelineEntry`).
#[derive(Clone, Debug)]
pub enum Entry {
    Message(OrchestrationMessage),
    Plan(OrchestrationProposedPlan),
    Work(WorkEntry),
}

impl Entry {
    pub fn id(&self) -> String {
        match self {
            Self::Message(m) => m.id.as_str().to_owned(),
            Self::Plan(p) => p.id.to_string(),
            Self::Work(w) => w.id.clone(),
        }
    }

    pub fn created_at(&self) -> &str {
        match self {
            Self::Message(m) => &m.created_at,
            Self::Plan(p) => &p.created_at,
            Self::Work(w) => &w.created_at,
        }
    }

    fn turn_id(&self) -> Option<&str> {
        match self {
            Self::Message(m) if matches!(m.role, OrchestrationMessageRole::Assistant | OrchestrationMessageRole::Reasoning) => {
                m.turn_id.as_ref().map(|t| t.as_str())
            }
            Self::Message(_) => None,
            Self::Plan(p) => p.turn_id.as_ref().map(|t| t.as_str()),
            Self::Work(w) => w.turn_id.as_deref(),
        }
    }

    fn is_user(&self) -> bool {
        matches!(self, Self::Message(m) if m.role == OrchestrationMessageRole::User)
    }

    fn is_reasoning(&self) -> bool {
        matches!(self, Self::Message(m) if m.role == OrchestrationMessageRole::Reasoning)
    }

    fn work(&self) -> Option<&WorkEntry> {
        match self {
            Self::Work(w) => Some(w),
            _ => None,
        }
    }

    /// Thinking, and work that is neither an answer to a question, a compaction nor an error.
    fn is_activity(&self) -> bool {
        match self {
            Self::Message(m) => m.role == OrchestrationMessageRole::Reasoning,
            Self::Work(w) => !is_compaction(w) && w.tone != Tone::Error,
            Self::Plan(_) => false,
        }
    }
}

fn is_compaction(entry: &WorkEntry) -> bool {
    entry.kind == "context-compaction"
}

/// What a group of work says it did, for its icon (`toolGroupSummaryKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SummaryKind {
    Action(Action),
    Mixed,
    AgentTool,
    DynamicTool,
    ToneTool,
}

pub fn summary_kind(entries: &[&WorkEntry]) -> SummaryKind {
    let actions: HashSet<Action> = entries.iter().map(|e| e.action()).collect();
    if actions.len() != 1 {
        return SummaryKind::Mixed;
    }
    let action = *actions.iter().next().unwrap();
    if action != Action::Other {
        return SummaryKind::Action(action);
    }
    let kinds: HashSet<_> = entries
        .iter()
        .map(|e| match e.item_type.as_deref() {
            Some("mcp_tool_call") => SummaryKind::Action(Action::Other),
            Some("dynamic_tool_call") => SummaryKind::DynamicTool,
            Some("collab_agent_tool_call") => SummaryKind::AgentTool,
            _ if e.tone == Tone::Thinking => SummaryKind::AgentTool,
            _ if e.tone == Tone::Tool => SummaryKind::ToneTool,
            _ => SummaryKind::Action(Action::Other),
        })
        .collect();
    if kinds.len() == 1 {
        *kinds.iter().next().unwrap()
    } else {
        SummaryKind::Mixed
    }
}

#[derive(Clone, Debug)]
pub enum Row {
    Message {
        id: String,
        message: OrchestrationMessage,
        /// The answer's time and copy button under it (once its turn is settled).
        show_meta: bool,
        /// The files the turn changed, under its last answer.
        diff: Option<OrchestrationCheckpointSummary>,
    },
    /// An answer's time and copy button, after the tool calls that followed it.
    AssistantMeta {
        id: String,
        message: OrchestrationMessage,
    },
    /// "Worked for 7m 7s ›".
    TurnFold {
        id: String,
        turn_id: String,
        label: String,
        created_at: String,
        expanded: bool,
    },
    /// Thinking interleaved with tool calls.
    ActivityGroup {
        id: String,
        group_id: String,
        entries: Vec<Entry>,
        expanded: bool,
        active: bool,
    },
    /// One work entry, or the entries of an open toggle (`expanded_group`).
    Work {
        id: String,
        entries: Vec<WorkEntry>,
        expanded_group: bool,
        label: Option<String>,
    },
    /// The tool running now.
    WorkLive {
        id: String,
        entry: WorkEntry,
        entries: Vec<WorkEntry>,
        group_id: String,
        expanded: bool,
        active: bool,
    },
    /// "Ran 3 commands", "Received 2 updates".
    WorkToggle {
        id: String,
        group_id: String,
        turn_id: Option<String>,
        summary: String,
        kind: SummaryKind,
        expanded: bool,
        failed: bool,
        created_at: String,
    },
    Compaction {
        id: String,
        label: String,
    },
    Plan {
        id: String,
        plan: OrchestrationProposedPlan,
    },
    /// "Working for 2m 3s" (since when).
    Working {
        since: Option<String>,
    },
    Thinking,
}

impl Row {
    pub fn id(&self) -> String {
        match self {
            Self::Message { id, .. }
            | Self::AssistantMeta { id, .. }
            | Self::TurnFold { id, .. }
            | Self::ActivityGroup { id, .. }
            | Self::Work { id, .. }
            | Self::WorkLive { id, .. }
            | Self::WorkToggle { id, .. }
            | Self::Compaction { id, .. }
            | Self::Plan { id, .. } => id.clone(),
            Self::Working { .. } => "working-indicator-row".into(),
            Self::Thinking => LIVE_ROW.into(),
        }
    }

    /// The space under the row (`TimelineRowContent`), in px.
    pub fn bottom_padding(&self) -> f32 {
        match self {
            Self::Work { expanded_group: true, .. } => 4.,
            Self::WorkLive { expanded: true, .. } | Self::WorkToggle { expanded: true, .. } => 0.,
            Self::TurnFold { .. } | Self::Working { .. } => 6.,
            Self::Message { message, show_meta: false, .. } if message.role == OrchestrationMessageRole::Assistant => 8.,
            Self::Message { message, .. } if message.role == OrchestrationMessageRole::Reasoning => 8.,
            Self::Work { .. } | Self::WorkLive { .. } | Self::WorkToggle { .. } | Self::ActivityGroup { .. } | Self::Thinking => 8.,
            _ => 16.,
        }
    }
}

const LIVE_ROW: &str = "live-activity-row";

/// The thread's entries in time order (`deriveTimelineEntries`).
pub fn entries(thread: &OrchestrationThread) -> Vec<Entry> {
    let mut entries: Vec<Entry> = thread.messages.iter().cloned().map(Entry::Message).collect();
    entries.extend(thread.proposed_plans.iter().cloned().map(Entry::Plan));
    entries.extend(worklog::work_entries(&thread.activities).into_iter().map(Entry::Work));
    // Stable: on equal times, messages, then plans, then work.
    entries.sort_by(|a, b| a.created_at().cmp(b.created_at()));
    entries
}

/// What the view keeps open.
#[derive(Clone, Debug, Default)]
pub struct Expanded {
    pub turns: HashSet<String>,
    pub groups: HashSet<String>,
}

fn group_identity(entry_id: &str, entry: &WorkEntry) -> String {
    match &entry.tool_call_id {
        Some(call) => format!("tool:{}:{call}", entry.turn_id.as_deref().unwrap_or("no-turn")),
        None => entry_id.to_owned(),
    }
}

fn group_id(entry_id: &str, entry: &WorkEntry) -> String {
    format!("work-group:{}", group_identity(entry_id, entry))
}

fn elapsed(start: &str, end: &str) -> Option<i64> {
    let (a, b) = (millis(start)?, millis(end)?);
    (b >= a).then_some(b - a)
}

/// Whether the agent works on the thread now (the session runs or starts).
pub fn is_working(thread: &OrchestrationThread) -> bool {
    matches!(
        thread.session.as_ref().map(|s| s.status),
        Some(OrchestrationSessionStatus::Running | OrchestrationSessionStatus::Starting)
    )
}

fn is_in_progress_activity(entry: &WorkEntry) -> bool {
    entry.status == Some(ToolStatus::InProgress) || (entry.status.is_none() && (entry.kind == "task.progress" || entry.is_tool_like()))
}

/// The rows of a thread (`deriveMessagesTimelineRows`).
pub fn rows(thread: &OrchestrationThread, expanded: &Expanded) -> Vec<Row> {
    let entries = entries(thread);
    let working = is_working(thread);
    let running_turn = thread
        .session
        .as_ref()
        .and_then(|s| s.active_turn_id.as_ref())
        .map(|t| t.as_str().to_owned())
        .filter(|_| working);
    let latest = thread.latest_turn.as_ref();
    let active_turn_started_at = latest
        .filter(|t| t.completed_at.is_none())
        .map(|t| t.started_at.clone().unwrap_or_else(|| t.requested_at.clone()));

    let diffs: HashMap<String, OrchestrationCheckpointSummary> = thread
        .checkpoints
        .iter()
        .filter_map(|c| c.assistant_message_id.as_ref().map(|m| (m.as_str().to_owned(), c.clone())))
        .collect();

    // The last answer of each response.
    let mut terminal_by_key: HashMap<String, String> = HashMap::new();
    let mut unkeyed = 0;
    for entry in &entries {
        if let Entry::Message(m) = entry {
            match m.role {
                OrchestrationMessageRole::User => unkeyed += 1,
                OrchestrationMessageRole::Assistant => {
                    let key = m
                        .turn_id
                        .as_ref()
                        .map(|t| format!("turn:{}", t.as_str()))
                        .unwrap_or_else(|| format!("unkeyed:{unkeyed}"));
                    terminal_by_key.insert(key, m.id.as_str().to_owned());
                }
                _ => {}
            }
        }
    }
    let terminal: HashSet<String> = terminal_by_key.into_values().collect();

    // The unsettled turn: the running one, else the latest while it has not completed.
    let unsettled = running_turn
        .clone()
        .or_else(|| latest.and_then(|t| (t.completed_at.is_none() || t.state == OrchestrationLatestTurnState::Running).then(|| t.turn_id.as_str().to_owned())));
    let last_user = entries.iter().rposition(Entry::is_user);
    let mut active_turns: HashSet<String> = HashSet::new();
    if let Some(unsettled) = &unsettled {
        active_turns.insert(unsettled.clone());
        if working {
            let start = last_user.map(|i| i + 1).unwrap_or(0);
            active_turns.extend(entries[start..].iter().filter_map(|e| e.turn_id().map(String::from)));
        }
    }

    let folds = turn_folds(&entries, &terminal, latest, &active_turns);
    let mut collapsed: HashSet<String> = HashSet::new();
    for fold in folds.values() {
        if !expanded.turns.contains(&fold.turn_id) {
            collapsed.extend(fold.hidden.iter().cloned());
        }
    }

    let header_index = if working { last_user.map(|i| i + 1).unwrap_or(0) } else { entries.len() };
    let belongs_to_active = |entry: &Entry, index: usize| working && index >= header_index && unsettled.as_deref().is_none_or(|u| entry.turn_id() == Some(u));
    let in_active_run =
        |w: &WorkEntry| working && unsettled.is_some() && w.status == Some(ToolStatus::InProgress) && w.turn_id.as_deref() == unsettled.as_deref();

    // The tail of the active turn: its tools, back to the last non-tool.
    let mut active_tools: Vec<usize> = Vec::new();
    if working {
        let mut index = entries.len();
        while index > header_index {
            index -= 1;
            let entry = &entries[index];
            let Some(w) = entry.work() else { break };
            if !belongs_to_active(entry, index) || is_compaction(w) || w.tone == Tone::Error {
                break;
            }
            active_tools.insert(0, index);
            if w.display_failed() {
                break;
            }
        }
    }
    let visible_active: Vec<&WorkEntry> = {
        let candidates: Vec<&WorkEntry> = active_tools
            .iter()
            .filter_map(|&i| entries[i].work())
            .filter(|w| w.visible_in_group(true))
            .collect();
        worklog::omit_superseded(&candidates)
    };
    let latest_visible = visible_active.last().copied();
    let latest_running = visible_active.iter().rev().copied().find(|w| is_in_progress_activity(w));
    let latest_failed = latest_running.is_none() && latest_visible.is_some_and(|w| w.status != Some(ToolStatus::Declined) && w.display_failed());
    let keeps_live =
        latest_running.is_some() || latest_visible.is_some_and(|w| w.succeeded() || (w.status == Some(ToolStatus::Completed) && !w.display_failed()));
    let anchor = active_tools.first().map(|&i| (&entries[i], entries[i].work().unwrap()));
    let placement = latest_visible.map(|w| w.id.clone());
    let active_row = match (anchor, latest_visible) {
        (Some((anchor_entry, anchor_work)), Some(latest_visible)) if !latest_failed => {
            let gid = group_id(&anchor_entry.id(), anchor_work);
            Some(Row::WorkLive {
                id: if keeps_live {
                    LIVE_ROW.into()
                } else {
                    format!("work-live:{}", group_identity(&anchor_entry.id(), anchor_work))
                },
                entry: latest_running.unwrap_or(latest_visible).clone(),
                entries: visible_active.iter().map(|w| (*w).clone()).collect(),
                expanded: expanded.groups.contains(&gid),
                group_id: gid,
                active: keeps_live,
            })
        }
        _ => None,
    };
    let active_ids: HashSet<String> = if active_row.is_some() || latest_failed {
        active_tools.iter().map(|&i| entries[i].id()).collect()
    } else {
        HashSet::new()
    };

    let mut out: Vec<Row> = Vec::new();
    let mut has_activity = false;
    let working_since = || {
        let visual_start = (active_turns.len() > 1)
            .then(|| {
                last_user.and_then(|i| match &entries[i] {
                    Entry::Message(m) => Some(m.created_at.clone()),
                    _ => None,
                })
            })
            .flatten();
        Row::Working {
            since: visual_start.or_else(|| active_turn_started_at.clone()),
        }
    };
    let push_active = |out: &mut Vec<Row>, has_activity: &mut bool| {
        if let Some(Row::WorkLive {
            group_id,
            entries,
            expanded,
            active,
            ..
        }) = &active_row
        {
            out.push(active_row.clone().unwrap());
            *has_activity |= *active;
            if *expanded {
                out.push(Row::Work {
                    id: format!("{group_id}:details"),
                    entries: entries.clone(),
                    expanded_group: true,
                    label: None,
                });
            }
        }
    };

    let mut scanned_through: Option<usize> = None;
    let mut index = 0;
    while index < entries.len() {
        let entry = &entries[index];
        let id = entry.id();
        if working && index == header_index {
            out.push(working_since());
        }
        if Some(&id) == placement.as_ref() {
            push_active(&mut out, &mut has_activity);
        }
        if let Some(fold) = folds.get(&id) {
            out.push(Row::TurnFold {
                id: format!("turn-fold:{}", fold.turn_id),
                turn_id: fold.turn_id.clone(),
                label: fold.label.clone(),
                created_at: fold.created_at.clone(),
                expanded: expanded.turns.contains(&fold.turn_id),
            });
        }
        if collapsed.contains(&id) {
            index += 1;
            continue;
        }

        // Thinking with tools: one activity group.
        if scanned_through.is_none_or(|s| index > s) && entry.turn_id().is_some() && entry.is_activity() {
            let turn = entry.turn_id().unwrap().to_owned();
            let mut cursor = index + 1;
            while cursor < entries.len() {
                let next = &entries[cursor];
                let nid = next.id();
                if !next.is_activity() || next.turn_id() != Some(turn.as_str()) || collapsed.contains(&nid) || folds.contains_key(&nid) {
                    break;
                }
                cursor += 1;
            }
            scanned_through = Some(cursor - 1);
            let group: Vec<Entry> = entries[index..cursor].to_vec();
            if group.iter().any(|e| matches!(e, Entry::Message(_))) {
                let active = working
                    && Some(turn.as_str()) == unsettled.as_deref()
                    && cursor == entries.len()
                    && !latest_failed
                    && (latest_visible.is_none() || keeps_live);
                let gid = match entry {
                    Entry::Work(w) => group_id(&id, w),
                    _ => format!("activity-group:{id}"),
                };
                out.push(Row::ActivityGroup {
                    id: if active { LIVE_ROW.into() } else { gid.clone() },
                    expanded: expanded.groups.contains(&gid),
                    group_id: gid,
                    entries: group,
                    active,
                });
                has_activity |= active;
                index = cursor;
                continue;
            }
        }

        if active_ids.contains(&id) {
            index += 1;
            continue;
        }

        match entry {
            Entry::Work(w) if is_compaction(w) => {
                out.push(Row::Compaction {
                    id: id.clone(),
                    label: w.label.clone(),
                });
                index += 1;
            }
            Entry::Work(w) if w.tone == Tone::Error => {
                out.push(Row::Work {
                    id: id.clone(),
                    entries: vec![w.clone()],
                    expanded_group: false,
                    label: None,
                });
                index += 1;
            }
            Entry::Work(w) => {
                let mut grouped = vec![w];
                let mut cursor = index + 1;
                while cursor < entries.len() {
                    let next = &entries[cursor];
                    let nid = next.id();
                    match next.work() {
                        Some(nw)
                            if !is_compaction(nw)
                                && nw.tone != Tone::Error
                                && !active_ids.contains(&nid)
                                && !collapsed.contains(&nid)
                                && !folds.contains_key(&nid) =>
                        {
                            grouped.push(nw)
                        }
                        _ => break,
                    }
                    cursor += 1;
                }
                let filtered: Vec<&WorkEntry> = grouped.into_iter().filter(|e| e.visible_in_group(in_active_run(e))).collect();
                let visible = worklog::omit_superseded(&filtered);
                if !visible.is_empty() {
                    let live: Vec<&WorkEntry> = visible.iter().copied().filter(|e| in_active_run(e)).collect();
                    let gid = group_id(&id, w);
                    let open = expanded.groups.contains(&gid);
                    if let Some(last_live) = live.last() {
                        out.push(Row::WorkLive {
                            id: format!("work-live:{}", group_identity(&id, w)),
                            entry: (*last_live).clone(),
                            entries: visible.iter().map(|e| (*e).clone()).collect(),
                            group_id: gid.clone(),
                            expanded: open,
                            active: true,
                        });
                        has_activity = true;
                        if open {
                            out.push(Row::Work {
                                id: format!("{gid}:details"),
                                entries: visible.iter().map(|e| (*e).clone()).collect(),
                                expanded_group: true,
                                label: None,
                            });
                        }
                    } else if visible.len() == 1 && visible[0].is_tool_like() {
                        let single = visible[0];
                        out.push(Row::Work {
                            id: id.clone(),
                            entries: vec![single.clone()],
                            expanded_group: false,
                            label: Some(if single.action() == Action::Edit {
                                worklog::summarize(&visible)
                            } else {
                                single.single_label()
                            }),
                        });
                    } else {
                        let single = (visible.len() == 1).then(|| visible[0]);
                        let summary = match single {
                            Some(s) if s.is_tool_like() && s.action() != Action::Edit => s.single_label(),
                            Some(s) if !s.is_tool_like() => s.label.clone(),
                            _ => worklog::summarize(&visible),
                        };
                        out.push(Row::WorkToggle {
                            id: format!("work-toggle:{id}"),
                            group_id: gid.clone(),
                            turn_id: w.turn_id.clone(),
                            summary,
                            kind: summary_kind(&visible),
                            expanded: open,
                            failed: visible.iter().rev().find(|e| e.is_tool_like()).is_some_and(|e| e.display_failed()),
                            created_at: w.created_at.clone(),
                        });
                        if open {
                            out.push(Row::Work {
                                id: format!("{gid}:details"),
                                entries: visible.iter().map(|e| (*e).clone()).collect(),
                                expanded_group: true,
                                label: None,
                            });
                        }
                    }
                }
                index = cursor;
            }
            Entry::Plan(plan) => {
                out.push(Row::Plan {
                    id: id.clone(),
                    plan: plan.clone(),
                });
                index += 1;
            }
            Entry::Message(message) => {
                let in_progress =
                    message.role == OrchestrationMessageRole::Assistant && message.turn_id.as_ref().is_some_and(|t| active_turns.contains(t.as_str()));
                let show_meta = message.role == OrchestrationMessageRole::Assistant && terminal.contains(message.id.as_str()) && !in_progress;
                out.push(Row::Message {
                    id: id.clone(),
                    diff: (message.role == OrchestrationMessageRole::Assistant)
                        .then(|| diffs.get(message.id.as_str()).cloned())
                        .flatten(),
                    message: message.clone(),
                    show_meta,
                });
                index += 1;
            }
        }
    }

    let has_working = out.iter().any(|r| matches!(r, Row::Working { .. }));
    if working && !has_working && header_index == entries.len() {
        out.push(working_since());
    }
    if working && (!has_activity || latest_failed) {
        out.push(Row::Thinking);
    }
    attach_trailing_tools(out)
}

struct Fold {
    turn_id: String,
    created_at: String,
    hidden: HashSet<String>,
    label: String,
}

/// `deriveTurnFolds`, keyed by the entry the fold sits before.
fn turn_folds(
    entries: &[Entry],
    terminal: &HashSet<String>,
    latest: Option<&zc_contracts::OrchestrationLatestTurn>,
    unfolded: &HashSet<String>,
) -> HashMap<String, Fold> {
    struct Group<'a> {
        entries: Vec<&'a Entry>,
        terminal: Option<&'a OrchestrationMessage>,
        streaming: bool,
        start: Option<String>,
    }
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Group> = HashMap::new();
    let mut pending_user: Option<String> = None;
    for entry in entries {
        if let Entry::Message(m) = entry {
            if m.role == OrchestrationMessageRole::User {
                pending_user = Some(m.created_at.clone());
                continue;
            }
        }
        let turn = match entry {
            Entry::Message(m) if matches!(m.role, OrchestrationMessageRole::Assistant | OrchestrationMessageRole::Reasoning) => {
                m.turn_id.as_ref().map(|t| t.as_str().to_owned())
            }
            Entry::Work(w) => w.turn_id.clone(),
            _ => None,
        };
        let Some(turn) = turn else { continue };
        let group = groups.entry(turn.clone()).or_insert_with(|| {
            order.push(turn.clone());
            Group {
                entries: Vec::new(),
                terminal: None,
                streaming: false,
                start: pending_user.take(),
            }
        });
        group.entries.push(entry);
        if let Entry::Message(m) = entry {
            if terminal.contains(m.id.as_str()) {
                group.terminal = Some(m);
            }
            if m.streaming && m.role != OrchestrationMessageRole::Reasoning {
                group.streaming = true;
            }
        }
    }

    let mut folds = HashMap::new();
    for turn in order {
        let group = &groups[&turn];
        if unfolded.contains(&turn) || group.streaming {
            continue;
        }
        let terminal_index = group
            .terminal
            .and_then(|t| group.entries.iter().position(|e| e.id() == t.id.as_str()))
            .unwrap_or(group.entries.len());
        let trailing = group
            .entries
            .iter()
            .enumerate()
            .filter(|(i, e)| *i > terminal_index && !e.is_reasoning())
            .count();
        let mut hidden: HashSet<String> = HashSet::new();
        for (i, entry) in group.entries.iter().enumerate() {
            if group.terminal.is_some_and(|t| entry.id() == t.id.as_str()) {
                continue;
            }
            let compaction = entry.work().is_some_and(is_compaction);
            let single_trailing = trailing == 1 && entry.work().is_some_and(|w| !w.display_failed());
            if !compaction && !entry.is_reasoning() && i > terminal_index && !single_trailing {
                continue;
            }
            hidden.insert(entry.id());
        }
        let hides_work = group
            .entries
            .iter()
            .any(|e| hidden.contains(&e.id()) && !e.work().is_some_and(is_compaction) && !e.is_reasoning());
        if hidden.is_empty() || !hides_work {
            continue;
        }
        let (Some(first), Some(first_hidden), Some(last)) = (
            group.entries.first(),
            group.entries.iter().find(|e| hidden.contains(&e.id())),
            group.entries.last(),
        ) else {
            continue;
        };
        let interrupted = latest.is_some_and(|t| t.turn_id.as_str() == turn && t.state == OrchestrationLatestTurnState::Interrupted);
        let last_end = match last {
            Entry::Message(m) => m.updated_at.clone(),
            other => other.created_at().to_owned(),
        };
        let ms = match latest.filter(|t| t.turn_id.as_str() == turn) {
            Some(t) if t.started_at.is_some() && t.completed_at.is_some() => elapsed(t.started_at.as_deref().unwrap(), t.completed_at.as_deref().unwrap()),
            _ => {
                let end = match group.terminal.map(|t| t.updated_at.clone()) {
                    Some(t) if t > last_end => t,
                    _ => last_end.clone(),
                };
                elapsed(group.start.as_deref().unwrap_or(first.created_at()), &end)
            }
        };
        let duration = ms.map(format_duration);
        let label = match (interrupted, duration) {
            (true, Some(d)) => format!("You stopped after {d}"),
            (true, None) => "You stopped this response".into(),
            (false, Some(d)) => format!("Worked for {d}"),
            (false, None) => "Worked".into(),
        };
        folds.insert(
            first_hidden.id(),
            Fold {
                turn_id: turn.clone(),
                created_at: first_hidden.created_at().to_owned(),
                hidden,
                label,
            },
        );
    }
    folds
}

/// `attachTrailingToolGroupsToAssistant`: an answer followed by tool calls of its turn shows its
/// time and copy button after them.
fn attach_trailing_tools(rows: Vec<Row>) -> Vec<Row> {
    let mut without_meta: HashSet<String> = HashSet::new();
    let mut meta_after: HashMap<usize, Row> = HashMap::new();
    for (index, row) in rows.iter().enumerate() {
        let Row::Message {
            id, message, show_meta: true, ..
        } = row
        else {
            continue;
        };
        let Some(turn) = message
            .turn_id
            .as_ref()
            .map(|t| t.as_str())
            .filter(|_| message.role == OrchestrationMessageRole::Assistant)
        else {
            continue;
        };
        let mut last_trailing: Option<usize> = None;
        let mut trailing_group = false;
        for (j, candidate) in rows.iter().enumerate().skip(index + 1) {
            match candidate {
                Row::Message { message: m, .. } if m.role == OrchestrationMessageRole::Reasoning => continue,
                Row::Message { .. } => break,
                Row::WorkToggle { turn_id, .. } if turn_id.as_deref() == Some(turn) => {
                    trailing_group = true;
                    last_trailing = Some(j);
                }
                Row::ActivityGroup { entries, .. } if entries.iter().any(|e| matches!(e, Entry::Work(_)) && e.turn_id() == Some(turn)) => {
                    trailing_group = true;
                    last_trailing = Some(j);
                }
                Row::Work { entries, expanded_group, .. } if entries.iter().any(|e| e.turn_id.as_deref() == Some(turn)) => {
                    if !expanded_group && entries.iter().any(|e| e.is_tool_like()) {
                        trailing_group = true;
                    }
                    if trailing_group {
                        last_trailing = Some(j);
                    }
                }
                _ => {}
            }
        }
        if let Some(j) = last_trailing {
            without_meta.insert(id.clone());
            meta_after.insert(
                j,
                Row::AssistantMeta {
                    id: format!("assistant-meta:{}", message.id.as_str()),
                    message: message.clone(),
                },
            );
        }
    }
    let mut out = Vec::with_capacity(rows.len());
    for (index, row) in rows.into_iter().enumerate() {
        match row {
            Row::Message { id, message, diff, .. } if without_meta.contains(&id) => out.push(Row::Message {
                id,
                message,
                show_meta: false,
                diff,
            }),
            other => out.push(other),
        }
        if let Some(meta) = meta_after.remove(&index) {
            out.push(meta);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn thread(messages: serde_json::Value, activities: serde_json::Value, latest: serde_json::Value, session: serde_json::Value) -> OrchestrationThread {
        serde_json::from_value(json!({
            "id": "t", "projectId": "p", "title": "A thread", "modelSelection": {"instanceId": "claudeAgent", "model": "m"},
            "runtimeMode": "full-access", "interactionMode": "default", "branch": null, "worktreePath": null,
            "latestTurn": latest, "createdAt": "2026-10-01T10:00:00.000Z", "updatedAt": "2026-10-01T10:10:00.000Z",
            "archivedAt": null, "deletedAt": null, "messages": messages, "proposedPlans": [], "activities": activities,
            "checkpoints": [], "session": session
        }))
        .expect("a thread")
    }

    fn message(id: &str, role: &str, text: &str, turn: Option<&str>, at: &str) -> serde_json::Value {
        json!({"id": id, "role": role, "text": text, "turnId": turn, "streaming": false, "createdAt": at, "updatedAt": at})
    }

    fn tool(id: &str, at: &str, seq: i64) -> serde_json::Value {
        json!({"id": id, "tone": "tool", "kind": "tool.completed", "summary": "Ran command", "turnId": "turn-1", "sequence": seq, "createdAt": at,
               "payload": {"toolCallId": id, "itemType": "command_execution", "status": "completed", "data": {"item": {"command": "npm test"}}, "detail": "ok"}})
    }

    #[test]
    fn a_settled_turn_folds_its_work() {
        let t = thread(
            json!([
                message("u1", "user", "Do it", None, "2026-10-01T10:00:00.000Z"),
                message("a1", "assistant", "Looking", Some("turn-1"), "2026-10-01T10:00:05.000Z"),
                message("a2", "assistant", "Done", Some("turn-1"), "2026-10-01T10:07:07.000Z"),
            ]),
            json!([tool("c1", "2026-10-01T10:00:10.000Z", 1), tool("c2", "2026-10-01T10:00:20.000Z", 2)]),
            json!({"turnId": "turn-1", "assistantMessageId": null, "state": "completed", "requestedAt": "2026-10-01T10:00:00.000Z", "startedAt": "2026-10-01T10:00:00.000Z", "completedAt": "2026-10-01T10:07:07.000Z"}),
            serde_json::Value::Null,
        );
        let rows = rows(&t, &Expanded::default());
        let kinds: Vec<&str> = rows
            .iter()
            .map(|r| match r {
                Row::Message { message, .. } if message.role == OrchestrationMessageRole::User => "user",
                Row::TurnFold { label, .. } => {
                    assert_eq!(label, "Worked for 7m 7s");
                    "fold"
                }
                Row::Message { show_meta: true, .. } => "answer",
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(kinds, ["user", "fold", "answer"]);
        let mut open = Expanded::default();
        open.turns.insert("turn-1".into());
        let opened = super::rows(&t, &open);
        assert!(opened
            .iter()
            .any(|r| matches!(r, Row::WorkToggle { summary, .. } if summary == "Ran 2 commands")));
    }

    #[test]
    fn approvals_read_as_updates() {
        let approval = |id: &str, kind: &str, seq: i64| {
            json!({"id": id, "tone": "approval", "kind": kind, "summary": "Command approval", "turnId": "turn-1", "sequence": seq, "createdAt": "2026-10-01T10:08:00.000Z",
                   "payload": {"requestId": "r1", "requestKind": "command"}})
        };
        let t = thread(
            json!([
                message("u1", "user", "Do it", None, "2026-10-01T10:00:00.000Z"),
                message("a1", "assistant", "Done", Some("turn-1"), "2026-10-01T10:07:00.000Z")
            ]),
            json!([approval("p1", "approval.requested", 1), approval("p2", "approval.resolved", 2)]),
            json!({"turnId": "turn-1", "assistantMessageId": null, "state": "completed", "requestedAt": "2026-10-01T10:00:00.000Z", "startedAt": "2026-10-01T10:00:00.000Z", "completedAt": "2026-10-01T10:08:00.000Z"}),
            serde_json::Value::Null,
        );
        let rows = rows(&t, &Expanded::default());
        assert!(
            rows.iter()
                .any(|r| matches!(r, Row::WorkToggle { summary, kind: SummaryKind::Action(Action::Update), .. } if summary == "Received 2 updates")),
            "{rows:#?}"
        );
    }

    #[test]
    fn a_working_turn_shows_working_and_thinking() {
        let t = thread(
            json!([message("u1", "user", "Do it", None, "2026-10-01T10:00:00.000Z")]),
            json!([]),
            json!({"turnId": "turn-1", "assistantMessageId": null, "state": "running", "requestedAt": "2026-10-01T10:00:00.000Z", "startedAt": "2026-10-01T10:00:01.000Z", "completedAt": null}),
            json!({"threadId": "t", "status": "running", "providerName": null, "runtimeMode": "full-access", "activeTurnId": "turn-1", "lastError": null, "updatedAt": "2026-10-01T10:00:01.000Z"}),
        );
        let rows = rows(&t, &Expanded::default());
        assert!(
            matches!(rows[1], Row::Working { since: Some(ref s) } if s == "2026-10-01T10:00:01.000Z"),
            "{rows:#?}"
        );
        assert!(matches!(rows.last(), Some(Row::Thinking)));
    }
}
