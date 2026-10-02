//! The work log: what the agent did, from a thread's activities (`deriveWorkLogEntries` in
//! `apps/web/src/session-logic.ts`, `client-runtime/src/work-log/presentation.ts`).
//!
//! Bookkeeping activities are left out (tool starts, progress ticks, context-window updates,
//! plan updates, checkpoints, worktree setup unless it failed, subagent internals); the
//! updates of one tool call collapse into one entry; a group of entries reads as one
//! sentence ("Read 3 files and ran 2 commands").

use std::collections::HashMap;

use serde_json::Value;
use zc_contracts::{OrchestrationThreadActivity, OrchestrationThreadActivityTone};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Info,
    Tool,
    Error,
    /// A subagent's progress.
    Thinking,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
    Stopped,
}

impl ToolStatus {
    fn parse(payload: &Value) -> Option<Self> {
        Some(match payload.get("status")?.as_str()? {
            "pending" | "running" | "waiting" | "inProgress" => Self::InProgress,
            "cancelled" | "interrupted" | "stopped" => Self::Stopped,
            "idle" if payload.get("taskType").and_then(Value::as_str) == Some("subagent_batch") => Self::Stopped,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "declined" => Self::Declined,
            _ => return None,
        })
    }
}

/// What a tool did, for group summaries and icons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Action {
    Read,
    Edit,
    Command,
    CodeSearch,
    WebSearch,
    Other,
    Update,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkEntry {
    pub id: String,
    pub created_at: String,
    pub turn_id: Option<String>,
    pub kind: String,
    pub label: String,
    pub tone: Tone,
    pub detail: Option<String>,
    pub command: Option<String>,
    pub changed_files: Vec<String>,
    pub item_type: Option<String>,
    pub request_kind: Option<String>,
    pub tool_call_id: Option<String>,
    pub status: Option<ToolStatus>,
    pub exit_code: Option<i64>,
}

impl WorkEntry {
    pub fn action(&self) -> Action {
        if matches!(
            self.kind.as_str(),
            "approval.requested" | "approval.resolved" | "provider.approval.respond.failed"
        ) {
            return Action::Update;
        }
        let item = self.item_type.as_deref();
        let request = self.request_kind.as_deref();
        if request == Some("file-read") || item == Some("image_view") {
            return Action::Read;
        }
        if request == Some("file-change") || item == Some("file_change") || !self.changed_files.is_empty() {
            return Action::Edit;
        }
        if request == Some("command") || item == Some("command_execution") || self.command.is_some() {
            return Action::Command;
        }
        if item == Some("web_search") {
            return Action::WebSearch;
        }
        let label = self.label.to_lowercase();
        if label.starts_with("read") {
            return Action::Read;
        }
        if label.starts_with("grep") || label.starts_with("glob") || label.starts_with("search") || label.starts_with("find") {
            return Action::CodeSearch;
        }
        if label.starts_with("edit") || label.starts_with("write") {
            return Action::Edit;
        }
        if self.is_tool_like() {
            Action::Other
        } else {
            Action::Update
        }
    }

    pub fn is_tool_like(&self) -> bool {
        self.tone == Tone::Tool || self.item_type.is_some() || self.command.is_some() || self.tool_call_id.is_some()
    }

    /// Failed, by status or by what its output says (`workEntryIndicatesToolFailure`).
    pub fn failed(&self) -> bool {
        if self.tone == Tone::Error || matches!(self.status, Some(ToolStatus::Failed | ToolStatus::Declined)) {
            return true;
        }
        if self.exit_code.is_some_and(|c| c != 0) {
            return true;
        }
        self.detail.as_deref().is_some_and(|d| {
            let d = d.to_lowercase();
            d.contains("command not found") || d.contains("permission denied") || d.starts_with("error:")
        })
    }

    pub fn running(&self) -> bool {
        self.status == Some(ToolStatus::InProgress)
    }

    /// The text to show after the label: the command, else the detail's first line.
    pub fn preview(&self) -> Option<String> {
        if let Some(command) = &self.command {
            return Some(first_line(command));
        }
        self.detail.as_deref().map(first_line).filter(|d| !d.is_empty())
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty() && *l != "```").unwrap_or("");
    truncate(line, 120)
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let cut: String = text.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}

fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// `"output <exited with exit code 2>"` → (`"output"`, 2).
pub fn strip_exit_code(value: &str) -> (Option<String>, Option<i64>) {
    let trimmed = value.trim();
    let lower = trimmed.to_lowercase();
    if let Some(start) = lower.rfind("<exited with exit code ") {
        if trimmed.ends_with('>') {
            let code = trimmed[start + "<exited with exit code ".len()..trimmed.len() - 1].trim().parse().ok();
            let output = trimmed[..start].trim();
            return ((!output.is_empty()).then(|| output.to_owned()), code);
        }
    }
    ((!trimmed.is_empty()).then(|| trimmed.to_owned()), None)
}

fn command_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
        // `["bash", "-lc", "npm test"]`: the shell's script, else the words.
        Value::Array(parts) => {
            let words: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            if words.len() >= 3 && matches!(words[1], "-lc" | "-c") {
                Some(words[2..].join(" "))
            } else if words.is_empty() {
                None
            } else {
                Some(words.join(" "))
            }
        }
        _ => None,
    }
}

fn extract_command(payload: &Value) -> Option<String> {
    let data = payload.get("data");
    let item = data.and_then(|d| d.get("item"));
    let candidates = [
        item.and_then(|i| i.get("command")),
        item.and_then(|i| i.get("input")).and_then(|i| i.get("command")),
        item.and_then(|i| i.get("result")).and_then(|r| r.get("command")),
        data.and_then(|d| d.get("command")),
    ];
    for candidate in candidates.into_iter().flatten() {
        if let Some(command) = command_text(candidate) {
            return Some(command);
        }
    }
    if payload.get("itemType").and_then(Value::as_str) == Some("command_execution") {
        if let Some(detail) = text(payload.get("detail")) {
            return strip_exit_code(&detail).0;
        }
    }
    None
}

fn command_output(payload: &Value) -> Option<String> {
    let data = payload.get("data")?;
    let raw = data.get("rawOutput");
    let candidates = [
        raw.and_then(|r| r.get("stdout")),
        raw.and_then(|r| r.get("content")),
        data.get("item").and_then(|i| i.get("aggregatedOutput")),
        data.get("item").and_then(|i| i.get("result")).and_then(|r| r.get("stdout")),
    ];
    candidates.into_iter().flatten().find_map(|v| text(Some(v)))
}

fn collect_files(value: &Value, out: &mut Vec<String>, depth: usize) {
    if depth > 4 || out.len() >= 12 {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                collect_files(item, out, depth + 1);
            }
        }
        Value::Object(record) => {
            for key in ["path", "filePath", "relativePath", "filename", "newPath", "oldPath"] {
                if let Some(path) = text(record.get(key)) {
                    if !out.contains(&path) && out.len() < 12 {
                        out.push(path);
                    }
                }
            }
            for key in ["item", "result", "input", "data", "changes", "files", "edits", "patch", "patches", "operations"] {
                if let Some(nested) = record.get(key) {
                    collect_files(nested, out, depth + 1);
                }
            }
        }
        _ => {}
    }
}

fn skipped(activity: &OrchestrationThreadActivity) -> bool {
    let kind = activity.kind.as_str();
    let payload = &activity.payload;
    if matches!(kind, "setup-script.requested" | "setup-script.started") {
        return true;
    }
    if kind == "worktree-setup" {
        return true;
    }
    if matches!(
        kind,
        "tool.started" | "task.started" | "task.updated" | "tool.progress" | "context-window.updated" | "turn.plan.updated"
    ) {
        return true;
    }
    if activity.summary == "Checkpoint captured" {
        return true;
    }
    // Requests show as their own panel while open, and leave nothing to read once answered.
    if matches!(
        kind,
        "approval.requested" | "approval.resolved" | "user-input.requested" | "user-input.resolved"
    ) {
        return true;
    }
    if kind == "runtime.warning" && activity.summary.ends_with("(no displayable text content)") {
        return true;
    }
    // The Claude adapter's summaries of its own bookkeeping (`describeUnknownSdkMessage`):
    // nothing a person can act on.
    if kind == "runtime.warning"
        && activity.summary.starts_with("Claude system message '")
        && (activity.summary.contains("'post_turn_summary'") || activity.summary.contains("'task_summary'"))
    {
        return true;
    }
    if matches!(kind, "tool.updated" | "tool.completed") && payload.get("detail").and_then(Value::as_str).is_some_and(|d| d.starts_with("ExitPlanMode:")) {
        return true;
    }
    if payload.get("timelineBypass").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    let is_task = matches!(kind, "task.progress" | "task.completed");
    !is_task && payload.get("agentId").and_then(Value::as_str).is_some_and(|a| !a.trim().is_empty())
}

fn same_text(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    norm(a) == norm(b)
}

fn to_entry(activity: &OrchestrationThreadActivity) -> WorkEntry {
    let payload = &activity.payload;
    let kind = activity.kind.clone();
    let is_task = matches!(kind.as_str(), "task.progress" | "task.completed");
    let label = if is_task {
        text(payload.get("summary"))
            .or_else(|| text(payload.get("detail")))
            .unwrap_or_else(|| activity.summary.clone())
    } else {
        activity.summary.clone()
    };
    let tone = match (kind.as_str(), activity.tone) {
        ("task.progress", _) => Tone::Thinking,
        (_, OrchestrationThreadActivityTone::Error) => Tone::Error,
        (_, OrchestrationThreadActivityTone::Tool) => Tone::Tool,
        _ => Tone::Info,
    };
    let command = extract_command(payload);
    let item_type = text(payload.get("itemType"));
    let is_command_tool =
        item_type.as_deref() == Some("command_execution") || payload.get("data").and_then(|d| d.get("kind")).and_then(Value::as_str) == Some("execute");
    let (raw_detail, mut exit_code) = text(payload.get("detail")).map(|d| strip_exit_code(&d)).unwrap_or((None, None));
    let detail = if is_command_tool && command.is_some() {
        command_output(payload)
            .and_then(|o| {
                let (output, code) = strip_exit_code(&o);
                exit_code = exit_code.or(code);
                output
            })
            .or_else(|| raw_detail.filter(|d| Some(d.as_str()) != command.as_deref()))
    } else {
        raw_detail.filter(|d| !same_text(d, &label)).or_else(|| match kind.as_str() {
            "runtime.error" | "runtime.warning" => text(payload.get("message")).filter(|m| !same_text(m, &label)),
            _ => None,
        })
    };
    let mut changed_files = Vec::new();
    if let Some(data) = payload.get("data") {
        collect_files(data, &mut changed_files, 0);
    }
    let request_kind = text(payload.get("requestKind")).or_else(|| {
        payload
            .get("requestType")
            .and_then(Value::as_str)
            .and_then(crate::requests::request_kind_from_type)
            .map(String::from)
    });
    let tool_call_id = if is_task {
        None
    } else {
        text(payload.get("toolCallId")).or_else(|| text(payload.get("data").and_then(|d| d.get("toolCallId"))))
    };
    let mut status = ToolStatus::parse(payload);
    if status.is_none() && kind == "tool.completed" {
        status = Some(ToolStatus::Completed);
    }
    WorkEntry {
        id: activity.id.as_str().to_owned(),
        created_at: activity.created_at.clone(),
        turn_id: activity.turn_id.as_ref().map(|t| t.as_str().to_owned()),
        kind,
        label,
        tone,
        detail,
        command,
        changed_files,
        item_type,
        request_kind,
        tool_call_id,
        status,
        exit_code,
    }
}

/// `.started` before `.progress`/`.updated` before `.completed`/`.resolved` at equal times.
fn lifecycle_rank(kind: &str) -> u8 {
    if kind.ends_with(".started") {
        0
    } else if kind.ends_with(".progress") || kind.ends_with(".updated") {
        1
    } else if kind.ends_with(".completed") || kind.ends_with(".resolved") {
        2
    } else {
        1
    }
}

/// Activities in the order they happened.
pub fn ordered(activities: &[OrchestrationThreadActivity]) -> Vec<&OrchestrationThreadActivity> {
    let mut ordered: Vec<_> = activities.iter().collect();
    ordered.sort_by(|a, b| {
        let seq = |x: &OrchestrationThreadActivity| x.sequence;
        let by_sequence = match (seq(a), seq(b)) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Greater,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (None, None) => std::cmp::Ordering::Equal,
        };
        by_sequence
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| lifecycle_rank(&a.kind).cmp(&lifecycle_rank(&b.kind)))
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
    ordered
}

fn merge(previous: &WorkEntry, next: WorkEntry) -> WorkEntry {
    WorkEntry {
        id: previous.id.clone(),
        created_at: previous.created_at.clone(),
        turn_id: next.turn_id.or_else(|| previous.turn_id.clone()),
        detail: next.detail.or_else(|| previous.detail.clone()),
        // A tool call runs one command; later updates may only echo their output as "detail".
        command: previous.command.clone().or(next.command),
        changed_files: if next.changed_files.is_empty() {
            previous.changed_files.clone()
        } else {
            next.changed_files
        },
        item_type: next.item_type.or_else(|| previous.item_type.clone()),
        request_kind: next.request_kind.or_else(|| previous.request_kind.clone()),
        status: next.status.or(previous.status),
        exit_code: next.exit_code.or(previous.exit_code),
        ..next
    }
}

pub fn work_entries(activities: &[OrchestrationThreadActivity]) -> Vec<WorkEntry> {
    let mut entries: Vec<WorkEntry> = Vec::new();
    let mut by_tool_call: HashMap<String, usize> = HashMap::new();
    for activity in ordered(activities) {
        if skipped(activity) {
            continue;
        }
        let entry = to_entry(activity);
        if matches!(entry.kind.as_str(), "tool.updated" | "tool.completed") {
            if let Some(call) = &entry.tool_call_id {
                let key = format!("{}:{call}", entry.turn_id.as_deref().unwrap_or("no-turn"));
                if let Some(&index) = by_tool_call.get(&key) {
                    entries[index] = merge(&entries[index], entry);
                    continue;
                }
                by_tool_call.insert(key, entries.len());
            }
        }
        entries.push(entry);
    }
    entries
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// One sentence for a group of entries (`summarizeToolGroup`).
pub fn summarize(entries: &[&WorkEntry]) -> String {
    let mut groups: Vec<(Action, Vec<&WorkEntry>)> = Vec::new();
    for entry in entries {
        let action = entry.action();
        match groups.iter_mut().find(|(a, _)| *a == action) {
            Some((_, list)) => list.push(entry),
            None => groups.push((action, vec![entry])),
        }
    }
    let labels: Vec<String> = groups
        .iter()
        .map(|(action, list)| {
            let count = if *action == Action::Edit {
                let mut files: Vec<&String> = list.iter().flat_map(|e| e.changed_files.iter()).collect();
                files.sort();
                files.dedup();
                files.len() + list.iter().filter(|e| e.changed_files.is_empty()).count()
            } else {
                list.len()
            };
            match action {
                Action::Read => format!("Read {}", plural(count, "file", "files")),
                Action::Edit => format!("Changed {}", plural(count, "file", "files")),
                Action::Command => format!("Ran {}", plural(count, "command", "commands")),
                Action::CodeSearch => format!("Searched code {}", plural(count, "time", "times")),
                Action::WebSearch => format!("Searched the web {}", plural(count, "time", "times")),
                Action::Other => format!("Used {}", plural(count, "tool", "tools")),
                Action::Update => format!("Received {}", plural(count, "update", "updates")),
            }
        })
        .collect();
    let sentence: Vec<String> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            if i == 0 {
                label.clone()
            } else {
                let mut chars = label.chars();
                chars.next().map(|c| c.to_lowercase().chain(chars).collect()).unwrap_or_default()
            }
        })
        .collect();
    match sentence.len() {
        0 => String::new(),
        1 => sentence[0].clone(),
        2 => sentence.join(" and "),
        n => format!("{}, and {}", sentence[..n - 1].join(", "), sentence[n - 1]),
    }
}

/// A step of the agent's plan (`turn.plan.updated`).
#[derive(Clone, Debug, PartialEq)]
pub struct PlanStep {
    pub step: String,
    /// `pending`, `inProgress`, `completed`.
    pub status: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActivePlan {
    pub turn_id: Option<String>,
    pub explanation: Option<String>,
    pub steps: Vec<PlanStep>,
}

impl ActivePlan {
    pub fn completed(&self) -> usize {
        self.steps.iter().filter(|s| s.status == "completed").count()
    }
}

/// The latest plan the agent reported, preferring the running turn's.
pub fn active_plan(activities: &[OrchestrationThreadActivity], current_turn: Option<&str>) -> Option<ActivePlan> {
    let plans: Vec<&OrchestrationThreadActivity> = ordered(activities).into_iter().filter(|a| a.kind == "turn.plan.updated").collect();
    let chosen = current_turn
        .and_then(|turn| plans.iter().rev().find(|a| a.turn_id.as_ref().map(|t| t.as_str()) == Some(turn)))
        .or_else(|| plans.last())?;
    let steps = chosen
        .payload
        .get("plan")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|s| {
            Some(PlanStep {
                step: s.get("step")?.as_str()?.to_owned(),
                status: s.get("status").and_then(Value::as_str).unwrap_or("pending").to_owned(),
            })
        })
        .collect::<Vec<_>>();
    if steps.is_empty() {
        return None;
    }
    Some(ActivePlan {
        turn_id: chosen.turn_id.as_ref().map(|t| t.as_str().to_owned()),
        explanation: text(chosen.payload.get("explanation")),
        steps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn activity(id: &str, kind: &str, tone: &str, summary: &str, payload: Value, seq: i64) -> OrchestrationThreadActivity {
        serde_json::from_value(json!({
            "id": id, "tone": tone, "kind": kind, "summary": summary, "payload": payload,
            "turnId": "turn-1", "sequence": seq, "createdAt": "2026-10-01T10:00:00.000Z"
        }))
        .unwrap()
    }

    #[test]
    fn tool_updates_collapse_and_bookkeeping_is_left_out() {
        let activities = vec![
            activity("1", "tool.started", "tool", "Ran command", json!({"toolCallId": "c1"}), 1),
            activity(
                "2",
                "tool.updated",
                "tool",
                "Ran command",
                json!({"toolCallId": "c1", "itemType": "command_execution", "status": "inProgress", "data": {"item": {"command": ["bash", "-lc", "npm test"]}}}),
                2,
            ),
            activity(
                "3",
                "tool.completed",
                "tool",
                "Ran command",
                json!({"toolCallId": "c1", "itemType": "command_execution", "status": "completed", "detail": "1 failing <exited with exit code 1>"}),
                3,
            ),
            activity("4", "context-window.updated", "info", "Context", json!({}), 4),
            activity(
                "5",
                "tool.completed",
                "tool",
                "Read file",
                json!({"toolCallId": "c2", "requestKind": "file-read", "data": {"input": {"filePath": "src/main.rs"}}}),
                5,
            ),
            activity(
                "6",
                "tool.completed",
                "tool",
                "Edit",
                json!({"toolCallId": "c3", "itemType": "file_change", "data": {"changes": [{"path": "a.rs"}, {"path": "b.rs"}]}}),
                6,
            ),
            activity("7", "runtime.error", "error", "Provider crashed", json!({"message": "socket hang up"}), 7),
        ];
        let entries = work_entries(&activities);
        assert_eq!(entries.len(), 4, "{entries:#?}");
        let command = &entries[0];
        assert_eq!(command.command.as_deref(), Some("npm test"));
        assert_eq!(command.status, Some(ToolStatus::Completed));
        assert_eq!(command.exit_code, Some(1));
        assert!(command.failed());
        assert_eq!(entries[1].action(), Action::Read);
        assert_eq!(entries[2].changed_files, vec!["a.rs", "b.rs"]);
        assert_eq!(entries[3].detail.as_deref(), Some("socket hang up"));
        let tools: Vec<&WorkEntry> = entries[..3].iter().collect();
        assert_eq!(summarize(&tools), "Ran 1 command, read 1 file, and changed 2 files");
    }

    #[test]
    fn plans() {
        let activities = vec![activity(
            "1",
            "turn.plan.updated",
            "info",
            "Plan",
            json!({"plan": [{"step": "Write the code", "status": "completed"}, {"step": "Test it", "status": "inProgress"}]}),
            1,
        )];
        let plan = active_plan(&activities, Some("turn-1")).unwrap();
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.completed(), 1);
    }

    #[test]
    fn exit_codes() {
        assert_eq!(strip_exit_code("ok <exited with exit code 0>"), (Some("ok".into()), Some(0)));
        assert_eq!(strip_exit_code("plain"), (Some("plain".into()), None));
    }
}
