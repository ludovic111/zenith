//! The approvals and questions a thread waits on (`client-runtime/src/pendingRequests.ts`):
//! `approval.requested` / `user-input.requested` activities not yet closed by their
//! `.resolved` activity, or by a reply that failed because the request went stale.

use std::collections::{BTreeSet, HashMap};

use serde_json::Value;
use zc_contracts::OrchestrationThreadActivity;

#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalOption {
    /// `accept`, `acceptForSession`, `acceptAlways`, `decline`, `cancel`.
    pub decision: String,
    pub label: String,
    pub warning: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingApproval {
    pub request_id: String,
    /// `command`, `file-read`, `file-change`, `mcp-elicitation`, `permission`.
    pub request_kind: String,
    pub created_at: String,
    pub detail: Option<String>,
    pub app_name: Option<String>,
    /// The provider's own choices; empty means the default four.
    pub options: Vec<ApprovalOption>,
}

impl PendingApproval {
    /// The choices to offer: the provider's, else Cancel, Decline, Always allow this
    /// session, Approve.
    pub fn choices(&self) -> Vec<ApprovalOption> {
        if !self.options.is_empty() {
            return self.options.clone();
        }
        [
            ("cancel", "Cancel"),
            ("decline", "Decline"),
            ("acceptForSession", "Always allow this session"),
            ("accept", "Approve"),
        ]
        .into_iter()
        .map(|(decision, label)| ApprovalOption {
            decision: decision.into(),
            label: label.into(),
            warning: None,
        })
        .collect()
    }

    pub fn title(&self) -> &'static str {
        match self.request_kind.as_str() {
            "command" => "Run this command?",
            "file-read" => "Read this file?",
            "file-change" => "Apply these changes?",
            "mcp-elicitation" => "Answer this tool's request?",
            _ => "Allow this?",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
    pub allow_custom_answer: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingUserInput {
    pub request_id: String,
    pub created_at: String,
    pub questions: Vec<Question>,
    /// Can be dismissed without an answer.
    pub dismissible: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PendingRequests {
    pub approvals: Vec<PendingApproval>,
    pub user_inputs: Vec<PendingUserInput>,
}

impl PendingRequests {
    pub fn is_empty(&self) -> bool {
        self.approvals.is_empty() && self.user_inputs.is_empty()
    }
}

/// Older activities carry a native request type instead of a kind.
pub fn request_kind_from_type(request_type: &str) -> Option<&'static str> {
    Some(match request_type {
        "command_execution_approval" | "exec_command_approval" | "dynamic_tool_call" => "command",
        "file_read_approval" => "file-read",
        "file_change_approval" | "apply_patch_approval" => "file-change",
        "mcp_elicitation_approval" => "mcp-elicitation",
        "permission_approval" => "permission",
        _ => return None,
    })
}

const STALE_APPROVAL: &[&str] = &[
    "stale pending approval request",
    "unknown pending approval request",
    "unknown pending permission request",
    "unknown pending codex approval request",
];
const STALE_USER_INPUT: &[&str] = &[
    "stale pending user-input request",
    "unknown pending user-input request",
    "unknown pending user input request",
    "unknown pending codex user input request",
];

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from)
}

fn stale(payload: &Value, fragments: &[&str]) -> bool {
    let detail = payload.get("detail").and_then(Value::as_str).unwrap_or("").to_lowercase();
    fragments.iter().any(|f| detail.contains(f))
}

fn parse_questions(value: Option<&Value>) -> Vec<Question> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|q| {
            let options: Vec<QuestionOption> = q
                .get("options")?
                .as_array()?
                .iter()
                .filter_map(|o| {
                    Some(QuestionOption {
                        label: o.get("label")?.as_str()?.to_owned(),
                        description: text(o, "description"),
                    })
                })
                .collect();
            let allow_custom_answer = q.get("allowCustomAnswer").and_then(Value::as_bool).unwrap_or(true);
            if options.is_empty() && !allow_custom_answer {
                return None;
            }
            Some(Question {
                id: q.get("id")?.as_str()?.to_owned(),
                header: q.get("header").and_then(Value::as_str).unwrap_or("").to_owned(),
                question: q.get("question")?.as_str()?.to_owned(),
                options,
                multi_select: q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
                allow_custom_answer,
            })
        })
        .collect()
}

pub fn pending_requests(activities: &[OrchestrationThreadActivity]) -> PendingRequests {
    let mut approvals: HashMap<String, PendingApproval> = HashMap::new();
    let mut inputs: HashMap<String, PendingUserInput> = HashMap::new();
    let mut closed_approvals = BTreeSet::new();
    let mut closed_inputs = BTreeSet::new();
    for activity in activities {
        let payload = &activity.payload;
        let Some(request_id) = payload.get("requestId").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            continue;
        };
        let request_id = request_id.to_owned();
        match activity.kind.as_str() {
            "approval.requested" => {
                let request_type = payload.get("requestType").and_then(Value::as_str).unwrap_or("");
                if closed_approvals.contains(&request_id) || matches!(request_type, "tool_user_input" | "auth_tokens_refresh") {
                    continue;
                }
                let request_kind = payload
                    .get("requestKind")
                    .and_then(Value::as_str)
                    .filter(|k| matches!(*k, "command" | "file-read" | "file-change" | "mcp-elicitation" | "permission"))
                    .or_else(|| request_kind_from_type(request_type))
                    .unwrap_or("command")
                    .to_owned();
                let options = payload
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|o| {
                                Some(ApprovalOption {
                                    decision: o.get("decision")?.as_str()?.to_owned(),
                                    label: o.get("label")?.as_str()?.to_owned(),
                                    warning: text(o, "warning"),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                approvals.insert(
                    request_id.clone(),
                    PendingApproval {
                        request_id,
                        request_kind,
                        created_at: activity.created_at.clone(),
                        detail: text(payload, "detail"),
                        app_name: text(payload, "appName"),
                        options,
                    },
                );
            }
            "user-input.requested" => {
                if closed_inputs.contains(&request_id) {
                    continue;
                }
                let questions = parse_questions(payload.get("questions"));
                if questions.is_empty() {
                    continue;
                }
                inputs.insert(
                    request_id.clone(),
                    PendingUserInput {
                        request_id,
                        created_at: activity.created_at.clone(),
                        questions,
                        dismissible: payload.get("responseMode").and_then(Value::as_str) == Some("message"),
                    },
                );
            }
            "approval.resolved" => {
                approvals.remove(&request_id);
                closed_approvals.insert(request_id);
            }
            "provider.approval.respond.failed" if stale(payload, STALE_APPROVAL) => {
                approvals.remove(&request_id);
                closed_approvals.insert(request_id);
            }
            "user-input.resolved" => {
                inputs.remove(&request_id);
                closed_inputs.insert(request_id);
            }
            "provider.user-input.respond.failed" if stale(payload, STALE_USER_INPUT) => {
                inputs.remove(&request_id);
                closed_inputs.insert(request_id);
            }
            _ => {}
        }
    }
    let mut approvals: Vec<_> = approvals.into_values().collect();
    approvals.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    let mut user_inputs: Vec<_> = inputs.into_values().collect();
    user_inputs.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    PendingRequests { approvals, user_inputs }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn activity(id: &str, kind: &str, payload: Value, at: &str) -> OrchestrationThreadActivity {
        serde_json::from_value(json!({
            "id": id, "tone": "approval", "kind": kind, "summary": kind, "payload": payload,
            "turnId": "turn-1", "createdAt": at
        }))
        .unwrap()
    }

    #[test]
    fn requests_open_and_close() {
        let activities = vec![
            activity(
                "1",
                "approval.requested",
                json!({"requestId": "r1", "requestType": "exec_command_approval", "detail": "rm -rf build"}),
                "2026-10-01T10:00:00.000Z",
            ),
            activity(
                "2",
                "approval.requested",
                json!({"requestId": "r2", "requestKind": "file-change"}),
                "2026-10-01T10:00:01.000Z",
            ),
            activity("3", "approval.resolved", json!({"requestId": "r2"}), "2026-10-01T10:00:02.000Z"),
            activity(
                "4",
                "user-input.requested",
                json!({"requestId": "q1", "responseMode": "message", "questions": [
                    {"id": "color", "header": "Color", "question": "Which color?", "options": [{"label": "Blue", "description": "zenith's"}], "multiSelect": false}
                ]}),
                "2026-10-01T10:00:03.000Z",
            ),
            activity(
                "5",
                "user-input.requested",
                json!({"requestId": "q2", "questions": [{"id": "x", "header": "", "question": "?", "options": [], "allowCustomAnswer": false}]}),
                "2026-10-01T10:00:04.000Z",
            ),
        ];
        let pending = pending_requests(&activities);
        assert_eq!(pending.approvals.len(), 1);
        assert_eq!(pending.approvals[0].request_kind, "command");
        assert_eq!(pending.approvals[0].detail.as_deref(), Some("rm -rf build"));
        assert_eq!(pending.approvals[0].choices().len(), 4);
        assert_eq!(pending.user_inputs.len(), 1);
        assert!(pending.user_inputs[0].dismissible);
        assert_eq!(pending.user_inputs[0].questions[0].options[0].label, "Blue");
    }

    #[test]
    fn a_stale_reply_closes_the_request() {
        let activities = vec![
            activity("1", "approval.requested", json!({"requestId": "r1"}), "2026-10-01T10:00:00.000Z"),
            activity(
                "2",
                "provider.approval.respond.failed",
                json!({"requestId": "r1", "detail": "Stale pending approval request"}),
                "2026-10-01T10:00:01.000Z",
            ),
        ];
        assert!(pending_requests(&activities).is_empty());
    }
}
