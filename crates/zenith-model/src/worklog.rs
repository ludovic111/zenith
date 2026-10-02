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
    /// The tool's own title (`payload.title`).
    pub tool_title: Option<String>,
}

impl WorkEntry {
    /// What the entry did, for summaries and icons (`toolGroupAction`).
    pub fn action(&self) -> Action {
        if matches!(
            self.kind.as_str(),
            "approval.requested" | "approval.resolved" | "provider.approval.respond.failed"
        ) {
            return Action::Update;
        }
        let item = self.item_type.as_deref();
        let request = self.request_kind.as_deref();
        if request == Some("file-read")
            || item == Some("image_view")
            || (item == Some("dynamic_tool_call") && self.tool_title.as_deref().is_some_and(|t| t.trim().eq_ignore_ascii_case("read file")))
        {
            return Action::Read;
        }
        if request == Some("file-change") || item == Some("file_change") || !self.changed_files.is_empty() {
            return Action::Edit;
        }
        if request == Some("command") || item == Some("command_execution") || self.command.is_some() {
            return Action::Command;
        }
        if item == Some("web_search") {
            let label = compact_label(self.tool_title.as_deref().unwrap_or(&self.label)).to_lowercase();
            return if label.split(|c: char| !c.is_alphanumeric()).any(|w| w == "grep") {
                Action::CodeSearch
            } else {
                Action::WebSearch
            };
        }
        if self.is_tool_like() {
            Action::Other
        } else {
            Action::Update
        }
    }

    /// `workLogEntryIsToolLike`: tool, thinking or error tone, a command, a request, or a tool's
    /// own item type.
    pub fn is_tool_like(&self) -> bool {
        matches!(self.tone, Tone::Tool | Tone::Thinking | Tone::Error)
            || self.command.as_deref().is_some_and(|c| !c.trim().is_empty())
            || self.request_kind.is_some()
            || self.item_type.as_deref().is_some_and(is_tool_item_type)
    }

    fn failed_with(&self, include_command: bool) -> bool {
        if self.tone == Tone::Error || matches!(self.status, Some(ToolStatus::Failed | ToolStatus::Declined)) {
            return true;
        }
        if !self.is_tool_like() {
            return false;
        }
        let mut output = self.detail.clone().unwrap_or_default();
        if let Some(code) = self.exit_code.filter(|c| *c != 0) {
            output.push_str(&format!("\n<exited with exit code {code}>"));
        }
        if include_command {
            if let Some(command) = &self.command {
                output.push('\n');
                output.push_str(command);
            }
        }
        !output.trim().is_empty() && looks_like_failure(&output)
    }

    /// Failed, by status or by what its output and command say (`workEntryIndicatesToolFailure`).
    pub fn failed(&self) -> bool {
        self.failed_with(true)
    }

    /// Failed by what is shown, not counting the command itself
    /// (`workEntryDisplayIndicatesToolFailure`).
    pub fn display_failed(&self) -> bool {
        self.failed_with(false)
    }

    /// `workEntryIndicatesToolSuccess`.
    pub fn succeeded(&self) -> bool {
        self.is_tool_like() && !self.failed() && self.tone != Tone::Thinking && !matches!(self.status, Some(ToolStatus::InProgress | ToolStatus::Stopped))
    }

    /// A tool-like entry with neither success nor failure (`workEntryIndicatesToolNeutralStatus`).
    pub fn neutral(&self) -> bool {
        self.is_tool_like() && !self.failed() && !self.succeeded()
    }

    /// Whether a group shows it (`workEntryIsVisibleInGroup`): neutral entries only while their
    /// run is live.
    pub fn visible_in_group(&self, live: bool) -> bool {
        (live && (self.status == Some(ToolStatus::InProgress) || self.kind == "task.progress")) || !self.neutral()
    }

    /// The label of a lone tool call (`singleToolCallLabel`): its command, else its title.
    pub fn single_label(&self) -> String {
        if let Some(command) = self.command.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            return command.to_owned();
        }
        capitalize(&compact_label(self.tool_title.as_deref().unwrap_or(&self.label)))
    }

    /// The label of an entry in an open group (`workEntryDisplayLabel`).
    pub fn display_label(&self, workspace_root: Option<&str>) -> String {
        if let Some(command) = &self.command {
            return command.clone();
        }
        if let Some(detail) = &self.detail {
            return detail.clone();
        }
        if let Some(first) = self.changed_files.first() {
            let path = relative_path(first, workspace_root);
            return if self.changed_files.len() == 1 {
                path
            } else {
                format!("{path} +{} more", self.changed_files.len() - 1)
            };
        }
        capitalize(&compact_label(self.tool_title.as_deref().unwrap_or(&self.label)))
    }

    /// "Running npm", "Ran npm", "Failed npm"… (`liveWorkEntryLabel`).
    pub fn live_label(&self, workspace_root: Option<&str>, active: bool) -> String {
        if let Some(command) = self.command.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            let status = if active {
                self.status.unwrap_or(ToolStatus::InProgress)
            } else {
                self.status.unwrap_or(ToolStatus::Completed)
            };
            let verb = match status {
                ToolStatus::InProgress => "Running",
                ToolStatus::Failed => "Failed",
                ToolStatus::Declined => "Declined",
                ToolStatus::Stopped => "Stopped",
                ToolStatus::Completed => "Ran",
            };
            let program = command.split_whitespace().next().map(|p| p.rsplit('/').next().unwrap_or(p).to_owned());
            return format!("{verb} {}", program.unwrap_or_else(|| "command".into()));
        }
        self.display_label(workspace_root)
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

/// Item types that are a tool's own lifecycle (`isToolLifecycleItemType`).
fn is_tool_item_type(item: &str) -> bool {
    matches!(
        item,
        "command_execution" | "file_change" | "mcp_tool_call" | "dynamic_tool_call" | "collab_agent_tool_call" | "web_search" | "image_view"
    )
}

/// `normalizeCompactToolLabel`: without a trailing "complete(d)".
pub fn compact_label(value: &str) -> String {
    let trimmed = value.trim();
    let lower = trimmed.to_lowercase();
    for suffix in [" completed", " complete"] {
        if lower.ends_with(suffix) {
            return trimmed[..trimmed.len() - suffix.len()].trim().to_owned();
        }
    }
    trimmed.to_owned()
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

fn relative_path(path: &str, root: Option<&str>) -> String {
    match root.and_then(|r| path.strip_prefix(r.trim_end_matches('/'))) {
        Some(rest) if rest.starts_with('/') => rest[1..].to_owned(),
        _ => path.to_owned(),
    }
}

/// Some providers report success while the output says it failed
/// (`toolDetailTextLooksLikeFailure`).
fn looks_like_failure(text: &str) -> bool {
    let n = text.to_lowercase();
    let exit_code = |prefix: &str| {
        n.match_indices(prefix).any(|(i, m)| {
            let rest = n[i + m.len()..].trim_start_matches([' ', ':']);
            rest.chars().next().is_some_and(|c| ('1'..='9').contains(&c))
        })
    };
    n.contains("file not found")
        || n.contains("no files found")
        || n.contains("enoent")
        || n.contains("no such file")
        || n.contains("commandnotfoundexception")
        || n.contains("command not found")
        || (n.contains("cannot find path") && n.contains("because it does not exist"))
        || (n.contains("is not recognized") && n.contains("the term '"))
        || n.contains("is not recognized as the name of a cmdlet")
        || n.contains("a parameter cannot be found that matches parameter name")
        || exit_code("exit code")
}

/// `omitSupersededLifecycleMarkers`: a marker without status or id gives way to a later finished
/// entry of the same tool.
pub fn omit_superseded<'a>(entries: &[&'a WorkEntry]) -> Vec<&'a WorkEntry> {
    let mut later_terminal: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut kept = Vec::new();
    for entry in entries.iter().rev() {
        let identity = format!(
            "{}\u{1f}{}\u{1f}{}",
            entry.turn_id.as_deref().unwrap_or("no-turn"),
            entry.item_type.as_deref().unwrap_or(""),
            compact_label(entry.tool_title.as_deref().unwrap_or(&entry.label))
        );
        let marker = entry.tool_call_id.is_none() && entry.status.is_none() && matches!(entry.kind.as_str(), "tool.started" | "tool.updated");
        if marker && later_terminal.contains(&identity) {
            continue;
        }
        kept.push(*entry);
        if entry.kind == "tool.completed" || entry.status.is_some_and(|s| s != ToolStatus::InProgress) {
            later_terminal.insert(identity);
        }
    }
    kept.reverse();
    kept
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
    // Questions show as their own panel while open. Approvals stay: the web lists them as
    // "Received N updates".
    if matches!(kind, "user-input.requested" | "user-input.resolved") {
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
    // Approvals read as information (`toDerivedWorkLogEntry`).
    let tone = match (kind.as_str(), activity.tone) {
        ("task.progress", _) => Tone::Thinking,
        (_, OrchestrationThreadActivityTone::Error) => Tone::Error,
        (_, OrchestrationThreadActivityTone::Tool) => Tone::Tool,
        _ => Tone::Info,
    };
    let tool_title = text(payload.get("title"));
    let command = extract_command(payload);
    let item_type = text(payload.get("itemType")).filter(|t| is_tool_item_type(t));
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
        tool_title,
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
        tool_title: next.tool_title.or_else(|| previous.tool_title.clone()),
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
    for entry in omit_superseded(entries) {
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
