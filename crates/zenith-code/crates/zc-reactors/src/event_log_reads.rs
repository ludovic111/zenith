//! [`ReactorReads`] folded from the event store, for an engine without projection tables.
//!
//! Every read first catches up with the events stored since the last one (`readEvents` from
//! its cursor), so a read issued after a dispatch returned sees that dispatch, as a read of the
//! SQL projections would. The fold keeps:
//! - the command read model (`projector.ts` via [`zc_orchestration::project_event`]): threads,
//!   messages, proposed plans, sessions, checkpoints, projects;
//! - the `projection_turns` rows, by a port of `applyThreadTurnsProjection`
//!   (`Layers/ProjectionPipeline.ts`): pending turn starts and per-turn state;
//! - the `projection_thread_activities` rows (every activity, upserted by id; the read model
//!   only keeps the recent ones).
//!
//! Shells are the read model's threads without their heavy arrays, plus the summary fields the
//! SQL shell adds (`latestUserMessageAt`, `hasPendingApprovals`, `hasPendingUserInput`,
//! `hasActionableProposedPlan`) computed from the same data.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use zc_contracts::{MessageId, OrchestrationMessage, OrchestrationReadModel, OrchestrationThread, ProjectId, ThreadId, TurnId};
use zc_db::repos::proposed_plans::ProjectionThreadProposedPlan;
use zc_db::repos::thread_activities::ProjectionThreadActivity;
use zc_db::repos::thread_messages::ProjectionThreadMessage;
use zc_db::repos::turns::{PendingTurnStart, ProjectionTurn};
use zc_orchestration::{create_empty_read_model, project_event};
use zc_ports::{OrchestrationDispatch, TaggedError};

use crate::js::{str_of, trim};
use crate::reads::{ReactorReads, ReadResult, TurnStartMessage};

struct Fold {
    cursor: i64,
    model: OrchestrationReadModel,
    turns: HashMap<String, Vec<ProjectionTurn>>,
    activities: HashMap<String, Vec<ProjectionThreadActivity>>,
    /// `projection_threads.latest_turn_id`.
    latest_turn_ids: HashMap<String, Option<String>>,
}

impl Default for Fold {
    fn default() -> Self {
        Self {
            cursor: 0,
            model: create_empty_read_model("1970-01-01T00:00:00.000Z"),
            turns: HashMap::new(),
            activities: HashMap::new(),
            latest_turn_ids: HashMap::new(),
        }
    }
}

/// [`ReactorReads`] from the event log (see the module docs).
pub struct EventLogReactorReads {
    source: Arc<dyn OrchestrationDispatch>,
    fold: Mutex<Fold>,
}

fn s(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

impl EventLogReactorReads {
    pub fn new(source: Arc<dyn OrchestrationDispatch>) -> Self {
        Self {
            source,
            fold: Mutex::new(Fold::default()),
        }
    }

    /// Catches up with the log and runs `read` on the fold.
    async fn with_fold<R>(&self, read: impl FnOnce(&mut Fold) -> R) -> ReadResult<R> {
        let mut fold = self.fold.lock().await;
        let mut events = self.source.read_events(fold.cursor, None);
        while let Some(event) = events.next().await {
            let event = event?;
            let value = serde_json::to_value(&event).map_err(|error| TaggedError::new("ReactorReadDecodeError", error.to_string()))?;
            project_event(&mut fold.model, &event);
            fold.cursor = value["sequence"].as_i64().unwrap_or(fold.cursor);
            apply_activities(&mut fold, &value);
            apply_turns(&mut fold, &value);
            apply_latest_turn_id(&mut fold, &value);
        }
        Ok(read(&mut fold))
    }

    /// The `projection_turns` rows of a thread (test inspection: `readTurn`).
    pub async fn turns_of(&self, thread_id: &ThreadId) -> ReadResult<Vec<ProjectionTurn>> {
        self.with_fold(|fold| fold.turns.get(thread_id.as_str()).cloned().unwrap_or_default()).await
    }

    /// The folded read model (wire JSON), with each thread's `latestTurn` as the SQL snapshot
    /// reports it.
    pub async fn read_model(&self) -> ReadResult<Value> {
        self.with_fold(|fold| {
            let mut model = to_json(&fold.model);
            if let Some(threads) = model["threads"].as_array_mut() {
                for thread in threads {
                    fold.overlay_latest_turn(thread);
                }
            }
            model
        })
        .await
    }
}

fn to_json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

impl Fold {
    /// A thread of the model as wire JSON (deleted ones included), `latestTurn` as SQL has it.
    fn thread(&self, thread_id: &str) -> Option<Value> {
        let mut thread = self.model.threads.iter().find(|thread| thread.id.as_str() == thread_id).map(to_json)?;
        self.overlay_latest_turn(&mut thread);
        Some(thread)
    }

    /// `mapLatestTurn` of the row `latest_turn_id` points at (none when the join finds none).
    fn latest_turn_json(&self, thread_id: &str) -> Value {
        let Some(Some(latest)) = self.latest_turn_ids.get(thread_id) else {
            return Value::Null;
        };
        let Some(row) = self
            .turns
            .get(thread_id)
            .and_then(|rows| rows.iter().find(|row| row.turn_id.as_deref() == Some(latest.as_str())))
        else {
            return Value::Null;
        };
        let state = match row.state.as_str() {
            "error" => "error",
            "interrupted" => "interrupted",
            "completed" => "completed",
            _ => "running",
        };
        let mut turn = json!({
            "turnId": latest,
            "state": state,
            "requestedAt": row.requested_at,
            "startedAt": row.started_at,
            "completedAt": row.completed_at,
            "assistantMessageId": row.assistant_message_id,
        });
        if let (Some(thread_id), Some(plan_id)) = (&row.source_proposed_plan_thread_id, &row.source_proposed_plan_id) {
            turn["sourceProposedPlan"] = json!({"threadId": thread_id, "planId": plan_id});
        }
        turn
    }

    fn overlay_latest_turn(&self, thread: &mut Value) {
        let Some(thread_id) = str_of(thread, "id").map(str::to_owned) else { return };
        thread["latestTurn"] = self.latest_turn_json(&thread_id);
    }

    fn active_thread(&self, thread_id: &str) -> Option<Value> {
        self.thread(thread_id)
            .filter(|thread| thread["deletedAt"].is_null() && thread["archivedAt"].is_null())
    }

    fn projects(&self) -> Vec<Value> {
        self.model
            .projects
            .iter()
            .map(to_json)
            .filter(|project| project["deletedAt"].is_null())
            .collect()
    }

    fn project(&self, project_id: &str) -> Option<Value> {
        self.projects().into_iter().find(|project| str_of(project, "id") == Some(project_id))
    }

    fn shell_of(&self, thread: &Value) -> Value {
        let mut shell = thread.clone();
        let object = shell.as_object_mut().expect("thread object");
        let messages = object.remove("messages").unwrap_or(json!([]));
        let plans = object.remove("proposedPlans").unwrap_or(json!([]));
        object.remove("activities");
        object.remove("checkpoints");
        object.remove("deletedAt");
        let thread_id = str_of(thread, "id").unwrap_or("");
        let latest_user_message_at = messages
            .as_array()
            .into_iter()
            .flatten()
            .filter(|message| str_of(message, "role") == Some("user"))
            .filter_map(|message| str_of(message, "createdAt"))
            .max()
            .map(|at| Value::String(at.to_owned()))
            .unwrap_or(Value::Null);
        object.insert("latestUserMessageAt".into(), latest_user_message_at);
        let (pending_approvals, pending_user_input) = pending_requests(self.activities.get(thread_id).map(Vec::as_slice).unwrap_or(&[]));
        object.insert("hasPendingApprovals".into(), Value::Bool(pending_approvals));
        object.insert("hasPendingUserInput".into(), Value::Bool(pending_user_input));
        let actionable = plans.as_array().into_iter().flatten().any(|plan| plan["implementedAt"].is_null());
        object.insert("hasActionableProposedPlan".into(), Value::Bool(actionable));
        shell
    }

    fn project_shell(project: Value) -> Value {
        let mut shell = project;
        if let Some(object) = shell.as_object_mut() {
            object.remove("deletedAt");
        }
        shell
    }
}

/// Pending approvals and questions from a thread's activities (`hasPendingApprovals`,
/// `hasPendingUserInput`): `derivePendingUserInputCountFromActivities` and the pending
/// approvals projector, in activity time order. A failed answer only closes its request when the
/// provider no longer knew it.
fn pending_requests(activities: &[ProjectionThreadActivity]) -> (bool, bool) {
    let mut ordered: Vec<&ProjectionThreadActivity> = activities.iter().collect();
    ordered.sort_by(|left, right| (&left.created_at, &left.activity_id).cmp(&(&right.created_at, &right.activity_id)));
    let mut approvals: Vec<String> = Vec::new();
    let mut questions: Vec<String> = Vec::new();
    for activity in ordered {
        let Some(request_id) = activity.payload.get("requestId").and_then(Value::as_str) else {
            continue;
        };
        let detail = activity.payload.get("detail").and_then(Value::as_str).map(str::to_lowercase);
        let mentions = |needles: &[&str]| detail.as_ref().is_some_and(|detail| needles.iter().any(|needle| detail.contains(needle)));
        match activity.kind.as_str() {
            "approval.requested" => {
                if !approvals.iter().any(|id| id == request_id) {
                    approvals.push(request_id.to_owned());
                }
            }
            "approval.resolved" => approvals.retain(|id| id != request_id),
            "provider.approval.respond.failed"
                if mentions(&[
                    "stale pending approval request",
                    "unknown pending approval request",
                    "unknown pending permission request",
                ]) =>
            {
                approvals.retain(|id| id != request_id)
            }
            "user-input.requested" => {
                if !questions.iter().any(|id| id == request_id) {
                    questions.push(request_id.to_owned());
                }
            }
            "user-input.resolved" => questions.retain(|id| id != request_id),
            "provider.user-input.respond.failed"
                if mentions(&[
                    "stale pending user-input request",
                    "unknown pending user-input request",
                    "unknown pending user input request",
                    "unknown pending codex user input request",
                ]) =>
            {
                questions.retain(|id| id != request_id)
            }
            _ => {}
        }
    }
    (!approvals.is_empty(), !questions.is_empty())
}

/// SQL replay order: rows without a sequence first, then sequence, time, id.
fn sorted(activities: &[ProjectionThreadActivity]) -> Vec<&ProjectionThreadActivity> {
    let mut rows: Vec<&ProjectionThreadActivity> = activities.iter().collect();
    rows.sort_by(|left, right| {
        (left.sequence.is_some(), left.sequence, &left.created_at, &left.activity_id).cmp(&(
            right.sequence.is_some(),
            right.sequence,
            &right.created_at,
            &right.activity_id,
        ))
    });
    rows
}

/// `ORDER BY sequence DESC, created_at DESC, activity_id DESC` (SQLite sorts NULL lowest).
fn newest_first(activities: &[ProjectionThreadActivity]) -> Vec<&ProjectionThreadActivity> {
    let mut rows: Vec<&ProjectionThreadActivity> = activities.iter().collect();
    rows.sort_by(|left, right| (right.sequence, &right.created_at, &right.activity_id).cmp(&(left.sequence, &left.created_at, &left.activity_id)));
    rows
}

fn apply_activities(fold: &mut Fold, event: &Value) {
    let payload = &event["payload"];
    let Some(thread_id) = str_of(payload, "threadId") else { return };
    match str_of(event, "type").unwrap_or("") {
        "thread.created" => {
            fold.activities.remove(thread_id);
        }
        "thread.activity-appended" => {
            let activity = &payload["activity"];
            let row = ProjectionThreadActivity {
                activity_id: str_of(activity, "id").unwrap_or("").to_owned(),
                thread_id: thread_id.to_owned(),
                turn_id: s(&activity["turnId"]),
                tone: str_of(activity, "tone").unwrap_or("info").to_owned(),
                kind: str_of(activity, "kind").unwrap_or("").to_owned(),
                summary: str_of(activity, "summary").unwrap_or("").to_owned(),
                payload: activity.get("payload").cloned().unwrap_or(Value::Null),
                sequence: activity.get("sequence").and_then(Value::as_i64),
                created_at: str_of(activity, "createdAt").unwrap_or("").to_owned(),
            };
            let rows = fold.activities.entry(thread_id.to_owned()).or_default();
            match rows.iter_mut().find(|existing| existing.activity_id == row.activity_id) {
                Some(existing) => *existing = row,
                None => rows.push(row),
            }
        }
        "thread.reverted" => {
            // Keep what the reverted turns did not produce (an approximation of
            // `retainProjectionActivitiesAfterRevert`: activities of turns past the kept
            // checkpoint count go).
            let turn_count = payload["turnCount"].as_i64().unwrap_or(0);
            let removed: Vec<String> = fold
                .turns
                .get(thread_id)
                .into_iter()
                .flatten()
                .filter(|turn| turn.checkpoint_turn_count.is_none_or(|count| count > turn_count))
                .filter_map(|turn| turn.turn_id.clone())
                .collect();
            if let Some(rows) = fold.activities.get_mut(thread_id) {
                rows.retain(|row| row.turn_id.as_ref().is_none_or(|turn_id| !removed.contains(turn_id)));
            }
        }
        _ => {}
    }
}

/// The `latestTurnId` writes of the threads projector (`Layers/ProjectionPipeline.ts`): reset on
/// create, the active turn of a session, the turn of a diff, the last kept turn of a revert.
fn apply_latest_turn_id(fold: &mut Fold, event: &Value) {
    let payload = &event["payload"];
    let Some(thread_id) = str_of(payload, "threadId").map(str::to_owned) else {
        return;
    };
    match str_of(event, "type").unwrap_or("") {
        "thread.created" => {
            fold.latest_turn_ids.insert(thread_id, None);
        }
        "thread.session-set" => {
            if let Some(turn_id) = s(&payload["session"]["activeTurnId"]) {
                fold.latest_turn_ids.insert(thread_id, Some(turn_id));
            }
        }
        "thread.turn-diff-completed" => {
            fold.latest_turn_ids.insert(thread_id, s(&payload["turnId"]));
        }
        "thread.reverted" => {
            let turn_count = payload["turnCount"].as_i64().unwrap_or(0);
            let latest = fold
                .turns
                .get(&thread_id)
                .into_iter()
                .flatten()
                .filter(|turn| turn.turn_id.is_some() && turn.checkpoint_turn_count.is_some_and(|count| count <= turn_count))
                .max_by_key(|turn| turn.checkpoint_turn_count)
                .and_then(|turn| turn.turn_id.clone());
            fold.latest_turn_ids.insert(thread_id, latest);
        }
        _ => {}
    }
}

fn settled_turn_state_for_session_status(status: &str) -> Option<&'static str> {
    match status {
        "idle" | "ready" => Some("completed"),
        "error" => Some("error"),
        "interrupted" | "stopped" => Some("interrupted"),
        _ => None,
    }
}

fn pending_of(rows: &[ProjectionTurn]) -> Option<PendingTurnStart> {
    rows.iter()
        .filter(|row| row.turn_id.is_none() && row.state == "pending" && row.pending_message_id.is_some() && row.checkpoint_turn_count.is_none())
        .max_by(|left, right| left.requested_at.cmp(&right.requested_at))
        .map(|row| PendingTurnStart {
            thread_id: row.thread_id.clone(),
            message_id: row.pending_message_id.clone().unwrap_or_default(),
            source_proposed_plan_thread_id: row.source_proposed_plan_thread_id.clone(),
            source_proposed_plan_id: row.source_proposed_plan_id.clone(),
            requested_at: row.requested_at.clone(),
        })
}

fn clear_pending(rows: &mut Vec<ProjectionTurn>) {
    rows.retain(|row| !(row.turn_id.is_none() && row.state == "pending" && row.checkpoint_turn_count.is_none()));
}

fn upsert_turn(rows: &mut Vec<ProjectionTurn>, row: ProjectionTurn) {
    match rows.iter_mut().find(|existing| existing.turn_id.is_some() && existing.turn_id == row.turn_id) {
        Some(existing) => *existing = row,
        None => rows.push(row),
    }
}

fn new_turn(thread_id: &str, turn_id: &str) -> ProjectionTurn {
    ProjectionTurn {
        thread_id: thread_id.to_owned(),
        turn_id: Some(turn_id.to_owned()),
        pending_message_id: None,
        source_proposed_plan_thread_id: None,
        source_proposed_plan_id: None,
        assistant_message_id: None,
        state: "running".into(),
        requested_at: String::new(),
        started_at: None,
        completed_at: None,
        checkpoint_turn_count: None,
        checkpoint_ref: None,
        checkpoint_status: None,
        checkpoint_files: json!([]),
    }
}

/// `extractActivityRequestId`.
fn activity_request_id(payload: &Value) -> Option<&str> {
    payload.get("requestId").and_then(Value::as_str)
}

/// The session of a thread in the folded model: `(status, activeTurnId)`.
fn session_of(fold: &Fold, thread_id: &str) -> Option<(String, Option<String>)> {
    let thread = fold.model.threads.iter().find(|thread| thread.id.as_str() == thread_id)?;
    let session = thread.session.as_ref()?;
    Some((session.status.as_str().to_owned(), session.active_turn_id.as_ref().map(|id| id.0.clone())))
}

/// Port of `applyThreadTurnsProjection`.
fn apply_turns(fold: &mut Fold, event: &Value) {
    let payload = &event["payload"];
    let Some(thread_id) = str_of(payload, "threadId").map(str::to_owned) else {
        return;
    };
    let event_type = str_of(event, "type").unwrap_or("");
    match event_type {
        "thread.created" => {
            fold.turns.remove(&thread_id);
        }
        "thread.turn-start-requested" => {
            let rows = fold.turns.get(&thread_id).cloned().unwrap_or_default();
            if let Some(pending) = pending_of(&rows) {
                let is_compact = fold
                    .model
                    .threads
                    .iter()
                    .flat_map(|thread| thread.messages.iter())
                    .find(|message| message.id.as_str() == pending.message_id)
                    .map(|message| {
                        let message = to_json(message);
                        str_of(&message, "role") == Some("user")
                            && message["attachments"].as_array().is_none_or(Vec::is_empty)
                            && trim(str_of(&message, "text").unwrap_or("")).to_lowercase() == "/compact"
                    });
                if is_compact == Some(true) {
                    return;
                }
            }
            let rows = fold.turns.entry(thread_id.clone()).or_default();
            clear_pending(rows);
            rows.push(ProjectionTurn {
                thread_id: thread_id.clone(),
                turn_id: None,
                pending_message_id: s(&payload["messageId"]),
                source_proposed_plan_thread_id: s(&payload["sourceProposedPlan"]["threadId"]),
                source_proposed_plan_id: s(&payload["sourceProposedPlan"]["planId"]),
                assistant_message_id: None,
                state: "pending".into(),
                requested_at: str_of(payload, "createdAt").unwrap_or("").to_owned(),
                started_at: None,
                completed_at: None,
                checkpoint_turn_count: None,
                checkpoint_ref: None,
                checkpoint_status: None,
                checkpoint_files: json!([]),
            });
        }
        "thread.activity-appended" => {
            let activity = &payload["activity"];
            let kind = str_of(activity, "kind").unwrap_or("");
            if kind != "context-compaction" && kind != "provider.turn.start.failed" {
                return;
            }
            let rows = fold.turns.entry(thread_id).or_default();
            let Some(pending) = pending_of(rows) else { return };
            if Some(pending.message_id.as_str()) != activity_request_id(&activity["payload"]) {
                return;
            }
            clear_pending(rows);
        }
        "thread.session-set" => {
            let session = &payload["session"];
            let status = str_of(session, "status").unwrap_or("");
            let updated_at = str_of(session, "updatedAt").unwrap_or("").to_owned();
            let occurred_at = str_of(event, "occurredAt").unwrap_or("").to_owned();
            let rows = fold.turns.entry(thread_id.clone()).or_default();
            let active_turn_id = s(&session["activeTurnId"]);
            match active_turn_id.filter(|_| status == "running") {
                None => {
                    let from_provider_session_set = str_of(event, "commandId").is_some_and(|id| id.starts_with("server:provider-session-set:"));
                    if (status == "ready" && from_provider_session_set) || matches!(status, "error" | "stopped" | "interrupted") {
                        clear_pending(rows);
                    }
                    let Some(settled) = settled_turn_state_for_session_status(status) else {
                        return;
                    };
                    for turn in rows.iter_mut().filter(|turn| turn.turn_id.is_some() && turn.state == "running") {
                        turn.state = settled.into();
                        turn.completed_at = Some(updated_at.clone());
                    }
                }
                Some(turn_id) => {
                    for turn in rows
                        .iter_mut()
                        .filter(|turn| turn.turn_id.is_some() && turn.turn_id.as_deref() != Some(turn_id.as_str()) && turn.state == "running")
                    {
                        turn.state = "completed".into();
                        turn.completed_at = Some(updated_at.clone());
                    }
                    let pending = pending_of(rows);
                    let existing = rows.iter().find(|turn| turn.turn_id.as_deref() == Some(turn_id.as_str())).cloned();
                    let row = match existing {
                        Some(mut turn) => {
                            if turn.state != "completed" && turn.state != "error" {
                                turn.state = "running".into();
                            }
                            if turn.pending_message_id.is_none() {
                                turn.pending_message_id = pending.as_ref().map(|pending| pending.message_id.clone());
                            }
                            if turn.source_proposed_plan_thread_id.is_none() {
                                turn.source_proposed_plan_thread_id = pending.as_ref().and_then(|pending| pending.source_proposed_plan_thread_id.clone());
                            }
                            if turn.source_proposed_plan_id.is_none() {
                                turn.source_proposed_plan_id = pending.as_ref().and_then(|pending| pending.source_proposed_plan_id.clone());
                            }
                            let fallback = pending
                                .as_ref()
                                .map(|pending| pending.requested_at.clone())
                                .unwrap_or_else(|| occurred_at.clone());
                            if turn.started_at.is_none() {
                                turn.started_at = Some(fallback.clone());
                            }
                            if turn.requested_at.is_empty() {
                                turn.requested_at = fallback;
                            }
                            turn
                        }
                        None => {
                            let mut turn = new_turn(&thread_id, &turn_id);
                            turn.pending_message_id = pending.as_ref().map(|pending| pending.message_id.clone());
                            turn.source_proposed_plan_thread_id = pending.as_ref().and_then(|pending| pending.source_proposed_plan_thread_id.clone());
                            turn.source_proposed_plan_id = pending.as_ref().and_then(|pending| pending.source_proposed_plan_id.clone());
                            let at = pending.as_ref().map(|pending| pending.requested_at.clone()).unwrap_or(occurred_at);
                            turn.requested_at = at.clone();
                            turn.started_at = Some(at);
                            turn
                        }
                    };
                    upsert_turn(rows, row);
                    clear_pending(rows);
                }
            }
        }
        "thread.message-sent" => {
            let Some(turn_id) = s(&payload["turnId"]) else { return };
            if str_of(payload, "role") != Some("assistant") {
                return;
            }
            let session = session_of(fold, &thread_id);
            let turn_still_running = session.is_some_and(|(status, active)| status == "running" && active.as_deref() == Some(turn_id.as_str()));
            let streaming = payload["streaming"].as_bool().unwrap_or(false);
            let settles_turn = !streaming && !turn_still_running;
            let created_at = str_of(payload, "createdAt").unwrap_or("").to_owned();
            let updated_at = str_of(payload, "updatedAt").unwrap_or("").to_owned();
            let rows = fold.turns.entry(thread_id.clone()).or_default();
            let existing = rows.iter().find(|turn| turn.turn_id.as_deref() == Some(turn_id.as_str())).cloned();
            let row = match existing {
                Some(mut turn) => {
                    turn.assistant_message_id = s(&payload["messageId"]);
                    if settles_turn {
                        turn.state = match turn.state.as_str() {
                            "interrupted" => "interrupted",
                            "error" => "error",
                            _ => "completed",
                        }
                        .into();
                        if turn.completed_at.is_none() {
                            turn.completed_at = Some(updated_at);
                        }
                    }
                    if turn.started_at.is_none() {
                        turn.started_at = Some(created_at.clone());
                    }
                    if turn.requested_at.is_empty() {
                        turn.requested_at = created_at;
                    }
                    turn
                }
                None => {
                    let mut turn = new_turn(&thread_id, &turn_id);
                    turn.assistant_message_id = s(&payload["messageId"]);
                    turn.state = if settles_turn { "completed" } else { "running" }.into();
                    turn.requested_at = created_at.clone();
                    turn.started_at = Some(created_at);
                    turn.completed_at = settles_turn.then_some(updated_at);
                    turn
                }
            };
            upsert_turn(rows, row);
        }
        "thread.turn-interrupt-requested" => {
            let Some(turn_id) = s(&payload["turnId"]) else { return };
            let created_at = str_of(payload, "createdAt").unwrap_or("").to_owned();
            let rows = fold.turns.entry(thread_id.clone()).or_default();
            let existing = rows.iter().find(|turn| turn.turn_id.as_deref() == Some(turn_id.as_str())).cloned();
            let row = match existing {
                Some(mut turn) => {
                    turn.state = "interrupted".into();
                    turn.completed_at.get_or_insert_with(|| created_at.clone());
                    turn.started_at.get_or_insert_with(|| created_at.clone());
                    if turn.requested_at.is_empty() {
                        turn.requested_at = created_at;
                    }
                    turn
                }
                None => {
                    let mut turn = new_turn(&thread_id, &turn_id);
                    turn.state = "interrupted".into();
                    turn.requested_at = created_at.clone();
                    turn.started_at = Some(created_at.clone());
                    turn.completed_at = Some(created_at);
                    turn
                }
            };
            upsert_turn(rows, row);
        }
        "thread.turn-diff-completed" => {
            let Some(turn_id) = s(&payload["turnId"]) else { return };
            let session = session_of(fold, &thread_id);
            let turn_still_running = session.is_some_and(|(status, active)| status == "running" && active.as_deref() == Some(turn_id.as_str()));
            let status = str_of(payload, "status").unwrap_or("").to_owned();
            let completed_at = str_of(payload, "completedAt").unwrap_or("").to_owned();
            let count = payload["checkpointTurnCount"].as_i64();
            let rows = fold.turns.entry(thread_id.clone()).or_default();
            let existing = rows.iter().find(|turn| turn.turn_id.as_deref() == Some(turn_id.as_str())).cloned();
            if let Some(existing) = &existing {
                if existing.checkpoint_status.as_deref().is_some_and(|current| current != "missing") && status == "missing" {
                    return;
                }
            }
            let next_state = if status == "error" { "error" } else { "completed" };
            for turn in rows.iter_mut() {
                if count.is_some() && turn.checkpoint_turn_count == count && turn.turn_id.as_deref() != Some(turn_id.as_str()) {
                    turn.checkpoint_turn_count = None;
                    turn.checkpoint_ref = None;
                    turn.checkpoint_status = None;
                    turn.checkpoint_files = json!([]);
                }
            }
            let row = match existing {
                Some(mut turn) => {
                    turn.assistant_message_id = s(&payload["assistantMessageId"]);
                    if !(turn_still_running || turn.state == "interrupted") {
                        turn.state = next_state.into();
                    }
                    turn.checkpoint_turn_count = count;
                    turn.checkpoint_ref = s(&payload["checkpointRef"]);
                    turn.checkpoint_status = Some(status);
                    turn.checkpoint_files = payload.get("files").cloned().unwrap_or(json!([]));
                    turn.started_at.get_or_insert_with(|| completed_at.clone());
                    if turn.requested_at.is_empty() {
                        turn.requested_at = completed_at.clone();
                    }
                    turn.completed_at = Some(completed_at);
                    turn
                }
                None => {
                    let mut turn = new_turn(&thread_id, &turn_id);
                    turn.assistant_message_id = s(&payload["assistantMessageId"]);
                    turn.state = if turn_still_running { "running" } else { next_state }.into();
                    turn.requested_at = completed_at.clone();
                    turn.started_at = Some(completed_at.clone());
                    turn.completed_at = Some(completed_at);
                    turn.checkpoint_turn_count = count;
                    turn.checkpoint_ref = s(&payload["checkpointRef"]);
                    turn.checkpoint_status = Some(status);
                    turn.checkpoint_files = payload.get("files").cloned().unwrap_or(json!([]));
                    turn
                }
            };
            upsert_turn(rows, row);
        }
        "thread.reverted" => {
            let turn_count = payload["turnCount"].as_i64().unwrap_or(0);
            let rows = fold.turns.entry(thread_id).or_default();
            rows.retain(|turn| turn.turn_id.is_some() && turn.checkpoint_turn_count.is_some_and(|count| count <= turn_count));
        }
        _ => {}
    }
}

fn message_row(thread_id: &ThreadId, message: &OrchestrationMessage) -> ProjectionThreadMessage {
    let value = to_json(message);
    ProjectionThreadMessage {
        message_id: message.id.0.clone(),
        thread_id: thread_id.0.clone(),
        turn_id: message.turn_id.as_ref().map(|id| id.0.clone()),
        role: str_of(&value, "role").unwrap_or("").to_owned(),
        text: message.text.clone(),
        attachments: value.get("attachments").cloned(),
        context: value.get("context").cloned(),
        is_streaming: message.streaming,
        created_at: message.created_at.clone(),
        updated_at: message.updated_at.clone(),
    }
}

impl Fold {
    fn typed_thread(&self, thread_id: &str) -> Option<&OrchestrationThread> {
        self.model.threads.iter().find(|thread| thread.id.as_str() == thread_id)
    }

    fn is_active(thread: &OrchestrationThread) -> bool {
        thread.deleted_at.is_none() && thread.archived_at.is_none()
    }
}

fn role_of(message: &OrchestrationMessage) -> String {
    to_json(&message.role).as_str().unwrap_or("").to_owned()
}

#[async_trait]
impl ReactorReads for EventLogReactorReads {
    async fn thread_runtime_context(&self, thread_id: &ThreadId) -> ReadResult<Option<Value>> {
        self.with_fold(|fold| {
            let thread = fold.typed_thread(thread_id.as_str()).filter(|thread| Fold::is_active(thread))?;
            Some(json!({
                "id": thread.id,
                "projectId": thread.project_id,
                "title": to_json(&thread.title),
                "titleState": to_json(&thread.title_state),
                "session": to_json(&thread.session),
            }))
        })
        .await
    }

    async fn thread_shell(&self, thread_id: &ThreadId) -> ReadResult<Option<Value>> {
        self.with_fold(|fold| fold.active_thread(thread_id.as_str()).map(|thread| fold.shell_of(&thread)))
            .await
    }

    async fn project_shell(&self, project_id: &ProjectId) -> ReadResult<Option<Value>> {
        self.with_fold(|fold| fold.project(project_id.as_str()).map(Fold::project_shell)).await
    }

    async fn project_shells(&self, project_ids: Option<Vec<ProjectId>>) -> ReadResult<Vec<Value>> {
        self.with_fold(|fold| {
            fold.projects()
                .into_iter()
                .filter(|project| {
                    project_ids
                        .as_ref()
                        .is_none_or(|ids| ids.iter().any(|id| Some(id.as_str()) == str_of(project, "id")))
                })
                .map(Fold::project_shell)
                .collect()
        })
        .await
    }

    async fn thread_detail(&self, thread_id: &ThreadId) -> ReadResult<Option<Value>> {
        self.with_fold(|fold| {
            fold.thread(thread_id.as_str())
                .filter(|thread| thread["deletedAt"].is_null())
                .map(|mut detail| {
                    detail["activities"] = json!([]);
                    detail
                })
        })
        .await
    }

    async fn turn_start_message(&self, thread_id: &ThreadId, message_id: &MessageId) -> ReadResult<Option<TurnStartMessage>> {
        self.with_fold(|fold| {
            let thread = fold.typed_thread(thread_id.as_str())?;
            let message = thread.messages.iter().find(|message| &message.id == message_id)?;
            let has_other_user_messages = thread.messages.iter().any(|other| {
                &other.id != message_id
                    && role_of(other) == "user"
                    && (trim(&other.text).to_lowercase() != "/compact" || other.attachments.as_ref().is_some_and(|items| !items.is_empty()))
            });
            Some(TurnStartMessage {
                message: to_json(message),
                has_other_user_messages,
            })
        })
        .await
    }

    async fn command_read_model_threads(&self) -> ReadResult<Vec<Value>> {
        self.with_fold(|fold| fold.model.threads.iter().map(to_json).collect()).await
    }

    async fn thread_checkpoint_context(&self, thread_id: &ThreadId) -> ReadResult<Option<Value>> {
        self.with_fold(|fold| {
            let thread = fold.typed_thread(thread_id.as_str()).filter(|thread| thread.deleted_at.is_none())?;
            let project = fold.model.projects.iter().find(|project| project.id == thread.project_id)?;
            Some(json!({
                "threadId": thread.id,
                "projectId": thread.project_id,
                "workspaceRoot": to_json(&project.workspace_root),
                "worktreePath": to_json(&thread.worktree_path),
                "checkpoints": to_json(&thread.checkpoints),
            }))
        })
        .await
    }

    async fn shell_snapshot(&self, unsettled_only: bool) -> ReadResult<Value> {
        self.with_fold(|fold| {
            let projects: Vec<Value> = fold.projects().into_iter().map(Fold::project_shell).collect();
            let threads: Vec<Value> = fold
                .model
                .threads
                .iter()
                .filter(|thread| Fold::is_active(thread))
                .map(to_json)
                .filter(|thread| !unsettled_only || (str_of(thread, "settledOverride") != Some("settled") && thread["settledAt"].is_null()))
                .map(|thread| fold.shell_of(&thread))
                .collect();
            json!({
                "snapshotSequence": fold.model.snapshot_sequence,
                "projects": projects,
                "threads": threads,
                "updatedAt": fold.model.updated_at,
            })
        })
        .await
    }

    async fn snapshot_sequence(&self) -> ReadResult<i64> {
        self.with_fold(|fold| fold.cursor).await
    }

    async fn pending_turn_start(&self, thread_id: &ThreadId) -> ReadResult<Option<PendingTurnStart>> {
        self.with_fold(|fold| fold.turns.get(thread_id.as_str()).and_then(|rows| pending_of(rows)))
            .await
    }

    async fn turn(&self, thread_id: &ThreadId, turn_id: &TurnId) -> ReadResult<Option<ProjectionTurn>> {
        self.with_fold(|fold| {
            fold.turns
                .get(thread_id.as_str())?
                .iter()
                .find(|turn| turn.turn_id.as_deref() == Some(turn_id.as_str()))
                .cloned()
        })
        .await
    }

    async fn message(&self, message_id: &MessageId) -> ReadResult<Option<ProjectionThreadMessage>> {
        self.with_fold(|fold| {
            fold.model.threads.iter().find_map(|thread| {
                thread
                    .messages
                    .iter()
                    .find(|message| &message.id == message_id)
                    .map(|message| message_row(&thread.id, message))
            })
        })
        .await
    }

    async fn has_assistant_message_for_turn(&self, thread_id: &ThreadId, turn_id: &TurnId, streaming_only: bool) -> ReadResult<bool> {
        self.with_fold(|fold| {
            fold.typed_thread(thread_id.as_str()).is_some_and(|thread| {
                thread
                    .messages
                    .iter()
                    .any(|message| role_of(message) == "assistant" && message.turn_id.as_ref() == Some(turn_id) && (!streaming_only || message.streaming))
            })
        })
        .await
    }

    async fn proposed_plan(&self, thread_id: &ThreadId, plan_id: &str) -> ReadResult<Option<ProjectionThreadProposedPlan>> {
        self.with_fold(|fold| {
            let thread = fold.typed_thread(thread_id.as_str())?;
            let plan = thread.proposed_plans.iter().map(to_json).find(|plan| str_of(plan, "id") == Some(plan_id))?;
            Some(ProjectionThreadProposedPlan {
                plan_id: plan_id.to_owned(),
                thread_id: thread_id.0.clone(),
                turn_id: s(&plan["turnId"]),
                plan_markdown: str_of(&plan, "planMarkdown").unwrap_or("").to_owned(),
                implemented_at: s(&plan["implementedAt"]),
                implementation_thread_id: s(&plan["implementationThreadId"]),
                created_at: str_of(&plan, "createdAt").unwrap_or("").to_owned(),
                updated_at: str_of(&plan, "updatedAt").unwrap_or("").to_owned(),
            })
        })
        .await
    }

    async fn user_input_lifecycle(&self, thread_id: &ThreadId) -> ReadResult<Vec<ProjectionThreadActivity>> {
        self.with_fold(|fold| {
            sorted(fold.activities.get(thread_id.as_str()).map(Vec::as_slice).unwrap_or(&[]))
                .into_iter()
                .filter(|row| {
                    matches!(
                        row.kind.as_str(),
                        "user-input.requested" | "user-input.resolved" | "provider.user-input.respond.failed"
                    )
                })
                .cloned()
                .collect()
        })
        .await
    }

    async fn latest_task_activity(&self, thread_id: &ThreadId, task_id: &str) -> ReadResult<Option<ProjectionThreadActivity>> {
        self.with_fold(|fold| {
            newest_first(fold.activities.get(thread_id.as_str()).map(Vec::as_slice).unwrap_or(&[]))
                .into_iter()
                .find(|row| {
                    if !matches!(row.kind.as_str(), "task.started" | "task.progress") || row.payload.get("taskId").and_then(Value::as_str) != Some(task_id) {
                        return false;
                    }
                    let title = match row.payload.get("title") {
                        Some(Value::String(title)) => title.as_str(),
                        _ if row.kind == "task.started" => row.payload.get("detail").and_then(Value::as_str).unwrap_or(""),
                        _ => "",
                    };
                    !trim(title).is_empty()
                })
                .cloned()
        })
        .await
    }

    async fn activities(&self, thread_id: &ThreadId, kinds: &[&str], limit: Option<i64>) -> ReadResult<Vec<ProjectionThreadActivity>> {
        self.with_fold(|fold| {
            let rows: Vec<&ProjectionThreadActivity> = newest_first(fold.activities.get(thread_id.as_str()).map(Vec::as_slice).unwrap_or(&[]))
                .into_iter()
                .filter(|row| kinds.contains(&row.kind.as_str()))
                .collect();
            let limited: Vec<ProjectionThreadActivity> = match limit {
                Some(limit) => rows.into_iter().take(limit.max(0) as usize).cloned().collect(),
                None => rows.into_iter().cloned().collect(),
            };
            sorted(&limited).into_iter().cloned().collect()
        })
        .await
    }
}
