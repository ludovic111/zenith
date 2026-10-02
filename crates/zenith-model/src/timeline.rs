//! A thread's timeline: messages, proposed plans, the work log and turn diffs merged by time
//! (`deriveTimelineEntriesWithState` and `deriveMessagesTimelineRows` in `apps/web`).
//! Consecutive work entries form one group, shown as a sentence that opens into its entries.

use zc_contracts::{OrchestrationCheckpointSummary, OrchestrationMessage, OrchestrationMessageRole, OrchestrationProposedPlan, OrchestrationThread};

use crate::worklog::{self, WorkEntry};

#[derive(Clone, Debug)]
pub enum TimelineItem {
    Message(OrchestrationMessage),
    Plan(OrchestrationProposedPlan),
    Work(WorkGroup),
    /// The files a turn changed, after the assistant message that ended it.
    Diff(OrchestrationCheckpointSummary),
}

impl TimelineItem {
    /// A stable id for the view (list keys, expanded state).
    pub fn key(&self) -> String {
        match self {
            Self::Message(m) => format!("m:{}", m.id.as_str()),
            Self::Plan(p) => format!("p:{}", p.id),
            Self::Work(g) => format!("w:{}", g.entries.first().map(|e| e.id.as_str()).unwrap_or("")),
            Self::Diff(d) => format!("d:{}", d.turn_id.as_str()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorkGroup {
    pub entries: Vec<WorkEntry>,
    pub summary: String,
    /// Some entry still runs.
    pub running: bool,
    pub failed: usize,
}

struct Pending {
    at: String,
    rank: u8,
    item: TimelineItem,
}

pub fn timeline(thread: &OrchestrationThread) -> Vec<TimelineItem> {
    let mut items: Vec<Pending> = Vec::new();
    for message in &thread.messages {
        // Empty assistant placeholders and blank reasoning say nothing.
        if message.text.trim().is_empty() && !message.streaming && message.attachments.as_ref().is_none_or(Vec::is_empty) {
            continue;
        }
        items.push(Pending {
            at: message.created_at.clone(),
            rank: if message.role == OrchestrationMessageRole::User { 0 } else { 2 },
            item: TimelineItem::Message(message.clone()),
        });
    }
    for plan in &thread.proposed_plans {
        items.push(Pending {
            at: plan.created_at.clone(),
            rank: 3,
            item: TimelineItem::Plan(plan.clone()),
        });
    }
    for entry in worklog::work_entries(&thread.activities) {
        items.push(Pending {
            at: entry.created_at.clone(),
            rank: 1,
            item: TimelineItem::Work(WorkGroup {
                entries: vec![entry],
                summary: String::new(),
                running: false,
                failed: 0,
            }),
        });
    }
    items.sort_by(|a, b| a.at.cmp(&b.at).then(a.rank.cmp(&b.rank)));

    // Group consecutive work entries.
    let mut merged: Vec<TimelineItem> = Vec::new();
    for pending in items {
        match (merged.last_mut(), pending.item) {
            (Some(TimelineItem::Work(group)), TimelineItem::Work(next)) => group.entries.extend(next.entries),
            (_, item) => merged.push(item),
        }
    }
    for item in &mut merged {
        if let TimelineItem::Work(group) = item {
            let refs: Vec<&WorkEntry> = group.entries.iter().filter(|e| e.is_tool_like()).collect();
            group.summary = if refs.is_empty() {
                group.entries.last().map(|e| e.label.clone()).unwrap_or_default()
            } else {
                worklog::summarize(&refs)
            };
            group.running = group.entries.iter().any(WorkEntry::running);
            group.failed = group.entries.iter().filter(|e| e.failed()).count();
        }
    }

    // Each turn's diff goes after its assistant message, else at the end of its turn.
    let mut checkpoints: Vec<&OrchestrationCheckpointSummary> = thread.checkpoints.iter().filter(|c| !c.files.is_empty()).collect();
    checkpoints.sort_by(|a, b| a.completed_at.cmp(&b.completed_at));
    for checkpoint in checkpoints {
        let after_message = checkpoint
            .assistant_message_id
            .as_ref()
            .and_then(|id| merged.iter().position(|item| matches!(item, TimelineItem::Message(m) if &m.id == id)));
        let index = after_message.map(|i| i + 1).unwrap_or_else(|| {
            merged
                .iter()
                .position(|item| match item {
                    TimelineItem::Message(m) => m.created_at > checkpoint.completed_at,
                    TimelineItem::Plan(p) => p.created_at > checkpoint.completed_at,
                    TimelineItem::Work(g) => g.entries.first().is_some_and(|e| e.created_at > checkpoint.completed_at),
                    TimelineItem::Diff(_) => false,
                })
                .unwrap_or(merged.len())
        });
        merged.insert(index, TimelineItem::Diff(checkpoint.clone()));
    }
    merged
}

/// The latest plan still waiting to be implemented.
pub fn actionable_plan(thread: &OrchestrationThread) -> Option<&OrchestrationProposedPlan> {
    thread
        .proposed_plans
        .iter()
        .filter(|p| p.implemented_at.is_none())
        .max_by(|a, b| a.created_at.cmp(&b.created_at))
}

/// The text that asks the agent to implement a plan (`apps/web/src/proposedPlan.ts`).
pub fn implement_plan_prompt(plan_markdown: &str) -> String {
    format!("PLEASE IMPLEMENT THIS PLAN:\n{}", plan_markdown.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_work_and_diffs_in_order() {
        let thread: OrchestrationThread = serde_json::from_value(json!({
            "id": "t1", "projectId": "p1", "title": "Made-up thread",
            "modelSelection": {"instanceId": "claudeAgent", "model": "made-up-model"},
            "runtimeMode": "full-access", "interactionMode": "default",
            "branch": null, "worktreePath": null, "pullRequests": [], "latestTurn": null,
            "createdAt": "2026-10-01T10:00:00.000Z", "updatedAt": "2026-10-01T10:00:00.000Z",
            "archivedAt": null, "settledAt": null, "deletedAt": null, "session": null, "proposedPlans": [],
            "messages": [
                {"id": "u1", "role": "user", "text": "Fix the build", "turnId": "turn-1", "streaming": false, "createdAt": "2026-10-01T10:00:00.000Z", "updatedAt": "2026-10-01T10:00:00.000Z"},
                {"id": "a1", "role": "assistant", "text": "Fixed.", "turnId": "turn-1", "streaming": false, "createdAt": "2026-10-01T10:00:05.000Z", "updatedAt": "2026-10-01T10:00:05.000Z"}
            ],
            "activities": [
                {"id": "x1", "tone": "tool", "kind": "tool.completed", "summary": "Ran command", "payload": {"toolCallId": "c1", "itemType": "command_execution", "data": {"command": "cargo build"}}, "turnId": "turn-1", "sequence": 3, "createdAt": "2026-10-01T10:00:01.000Z"},
                {"id": "x2", "tone": "tool", "kind": "tool.completed", "summary": "Read file", "payload": {"toolCallId": "c2", "requestKind": "file-read"}, "turnId": "turn-1", "sequence": 4, "createdAt": "2026-10-01T10:00:02.000Z"}
            ],
            "checkpoints": [
                {"turnId": "turn-1", "checkpointTurnCount": 1, "checkpointRef": "refs/t3/checkpoints/x/turn/1", "status": "ready", "files": [{"path": "src/lib.rs", "kind": "modified", "additions": 3, "deletions": 1}], "assistantMessageId": "a1", "completedAt": "2026-10-01T10:00:06.000Z"}
            ]
        }))
        .unwrap();
        let items = timeline(&thread);
        let kinds: Vec<&str> = items
            .iter()
            .map(|i| match i {
                TimelineItem::Message(_) => "message",
                TimelineItem::Plan(_) => "plan",
                TimelineItem::Work(_) => "work",
                TimelineItem::Diff(_) => "diff",
            })
            .collect();
        assert_eq!(kinds, vec!["message", "work", "message", "diff"]);
        let TimelineItem::Work(group) = &items[1] else { unreachable!() };
        assert_eq!(group.summary, "Ran 1 command and read 1 file");
    }
}
