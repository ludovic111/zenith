//! `orchestration/Layers/ProviderRuntimeIngestion.ts`: canonical provider runtime events →
//! orchestration commands.
//!
//! - Session lifecycle (`session.*`, `thread.started`, `turn.started|completed|aborted`) becomes
//!   `thread.session.set`, guarded so a stale or foreign turn cannot close the active one.
//! - Assistant and reasoning text deltas are buffered per message and delivered as
//!   `thread.message.{assistant,reasoning}.delta` / `.complete`: in `paragraph` mode finished
//!   markdown blocks go out early (at most every 400 ms), in `turn` mode at the end, past
//!   24,000 UTF-16 units the whole buffer spills; `token` mode streams assistant deltas as they
//!   come (reasoning never streams token by token).
//! - Proposed plans are buffered and upserted on completion; approvals, user input, tasks,
//!   tools, warnings, errors, compaction and token usage become `thread.activity.append`.
//! - `turn.diff.updated` records a placeholder checkpoint for a running turn
//!   (`thread.turn.diff.complete`), after a repository check on its own worker.
//! - Provider thread names become titles while the thread still has a default title.
//! - Task lifecycle and plan updates feed the background-liveness and plan-progress
//!   registries.
//!
//! One worker processes events in order; `drain` waits for both workers.

pub mod activities;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use zc_contracts::{MessageId, ThreadId, TurnId};
use zc_ports::{OrchestrationDispatch, ProviderService, SettingsService, TaggedError};

use crate::common::{dispatch_json, pretty, RepositoryProbe, UuidSource};
use crate::js::{len16, str_of, trim, Obj};
use crate::reads::ReactorReads;
use crate::registries::{TaskLivenessInput, TaskTransition, ThreadBackgroundLivenessRegistry, ThreadPlanProgressRegistry};
use crate::runtime::{DrainableWorker, ReactorClock, TtlMap};
use crate::settings::{resolve_project_settings, response_streaming_mode, settings_json};
use crate::titles::can_replace_thread_title;

use activities::{
    compacted_token_counts, event_turn_id, find_task_title_in_activities, has_renderable_text, is_tool_lifecycle_item_type, normalize_proposed_plan_markdown,
    runtime_event_to_activities, split_buffered_assistant_text,
};

const TURN_MESSAGE_IDS_BY_TURN_CACHE_CAPACITY: usize = 10_000;
const BUFFERED_MESSAGE_TEXT_BY_MESSAGE_ID_CACHE_CAPACITY: usize = 20_000;
const BUFFERED_PROPOSED_PLAN_BY_ID_CACHE_CAPACITY: usize = 10_000;
const TASK_DESCRIPTION_BY_TASK_CACHE_CAPACITY: usize = 10_000;
const CACHE_TTL: Duration = Duration::from_secs(120 * 60);
/// Past this many UTF-16 units a message's buffer is delivered whole.
pub const MAX_BUFFERED_ASSISTANT_CHARS: usize = 24_000;
/// Paragraphs that finish within this window after a delivery wait for the next one.
pub const MIN_ASSISTANT_DELIVERY_INTERVAL_MS: i64 = 400;
const REASONING_MESSAGE_ID_PREFIX: &str = "reasoning:";

/// `T3CODE_STRICT_PROVIDER_LIFECYCLE_GUARD !== "0"`.
fn strict_provider_lifecycle_guard() -> bool {
    std::env::var("T3CODE_STRICT_PROVIDER_LIFECYCLE_GUARD")
        .map(|value| value != "0")
        .unwrap_or(true)
}

/// What the ingestion is built from.
#[derive(Clone)]
pub struct IngestionDeps {
    pub engine: Arc<dyn OrchestrationDispatch>,
    pub reads: Arc<dyn ReactorReads>,
    pub providers: Arc<dyn ProviderService>,
    pub settings: Arc<dyn SettingsService>,
    pub repositories: Arc<dyn RepositoryProbe>,
    pub liveness: Arc<ThreadBackgroundLivenessRegistry>,
    pub plan_progress: Arc<ThreadPlanProgressRegistry>,
    pub clock: Arc<dyn ReactorClock>,
    pub uuids: UuidSource,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    Assistant,
    Reasoning,
}

fn role_of(message_id: &str) -> Role {
    if message_id.starts_with(REASONING_MESSAGE_ID_PREFIX) {
        Role::Reasoning
    } else {
        Role::Assistant
    }
}

/// `assistantSegmentMessageId(baseKey, segmentIndex, role)`.
fn segment_message_id(base_key: &str, segment_index: u64, role: Role) -> String {
    let prefix = match role {
        Role::Reasoning => REASONING_MESSAGE_ID_PREFIX,
        Role::Assistant => "assistant:",
    };
    if segment_index == 0 {
        format!("{prefix}{base_key}")
    } else {
        format!("{prefix}{base_key}:segment:{segment_index}")
    }
}

fn provider_turn_key(thread_id: &str, turn_id: &str) -> String {
    format!("{thread_id}:{turn_id}")
}

fn segment_state_key(thread_id: &str, turn_id: &str, role: Role) -> String {
    match role {
        Role::Reasoning => format!("{}:reasoning", provider_turn_key(thread_id, turn_id)),
        Role::Assistant => provider_turn_key(thread_id, turn_id),
    }
}

/// `String(event.itemId ?? event.turnId ?? event.eventId)`.
fn base_key_of(event: &Value) -> String {
    ["itemId", "turnId", "eventId"]
        .iter()
        .find_map(|key| event.get(*key).filter(|value| !value.is_null()))
        .map(|value| value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string()))
        .unwrap_or_default()
}

fn proposed_plan_id_for_turn(thread_id: &str, turn_id: &str) -> String {
    format!("plan:{thread_id}:turn:{turn_id}")
}

/// `proposedPlanIdFromEvent(event, threadId)`.
fn proposed_plan_id_from_event(event: &Value, thread_id: &str) -> String {
    if let Some(turn_id) = event_turn_id(event) {
        return proposed_plan_id_for_turn(thread_id, &turn_id);
    }
    if let Some(item_id) = event.get("itemId").and_then(Value::as_str) {
        return format!("plan:{thread_id}:item:{item_id}");
    }
    format!("plan:{thread_id}:event:{}", str_of(event, "eventId").unwrap_or(""))
}

fn same_id(left: Option<&str>, right: Option<&str>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left == right)
}

/// `orchestrationSessionStatusFromRuntimeState`.
fn session_status_from_runtime_state(state: &str) -> &'static str {
    match state {
        "starting" => "starting",
        "running" | "waiting" => "running",
        "ready" => "ready",
        "interrupted" => "interrupted",
        "stopped" => "stopped",
        _ => "error",
    }
}

/// `normalizeRuntimeTurnState(value)`.
fn normalize_runtime_turn_state(value: Option<&str>) -> &'static str {
    match value {
        Some("failed") => "failed",
        Some("interrupted") => "interrupted",
        Some("cancelled") => "cancelled",
        _ => "completed",
    }
}

#[derive(Clone, Debug)]
struct SegmentState {
    base_key: String,
    next_segment_index: u64,
    active_message_id: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct BufferedPlan {
    text: String,
    created_at: String,
}

/// The ingestion's caches (the Effect `Cache`s of the TS layer).
struct State {
    turn_message_ids: TtlMap<String, Vec<String>>,
    buffered_text: TtlMap<String, String>,
    last_delivery_at: TtlMap<String, i64>,
    reasoning_started_at: TtlMap<String, String>,
    reasoning_part_index: TtlMap<String, i64>,
    segments: TtlMap<String, SegmentState>,
    buffered_plans: TtlMap<String, BufferedPlan>,
    task_descriptions: TtlMap<String, String>,
}

impl State {
    fn new(clock: Arc<dyn ReactorClock>) -> Self {
        Self {
            turn_message_ids: TtlMap::with_clock(TURN_MESSAGE_IDS_BY_TURN_CACHE_CAPACITY, CACHE_TTL, clock.clone()),
            buffered_text: TtlMap::with_clock(BUFFERED_MESSAGE_TEXT_BY_MESSAGE_ID_CACHE_CAPACITY, CACHE_TTL, clock.clone()),
            last_delivery_at: TtlMap::with_clock(BUFFERED_MESSAGE_TEXT_BY_MESSAGE_ID_CACHE_CAPACITY, CACHE_TTL, clock.clone()),
            reasoning_started_at: TtlMap::with_clock(BUFFERED_MESSAGE_TEXT_BY_MESSAGE_ID_CACHE_CAPACITY, CACHE_TTL, clock.clone()),
            reasoning_part_index: TtlMap::with_clock(BUFFERED_MESSAGE_TEXT_BY_MESSAGE_ID_CACHE_CAPACITY, CACHE_TTL, clock.clone()),
            segments: TtlMap::with_clock(TURN_MESSAGE_IDS_BY_TURN_CACHE_CAPACITY, CACHE_TTL, clock.clone()),
            buffered_plans: TtlMap::with_clock(BUFFERED_PROPOSED_PLAN_BY_ID_CACHE_CAPACITY, CACHE_TTL, clock.clone()),
            task_descriptions: TtlMap::with_clock(TASK_DESCRIPTION_BY_TASK_CACHE_CAPACITY, CACHE_TTL, clock.clone()),
        }
    }

    fn remember_message_id(&mut self, thread_id: &str, turn_id: &str, message_id: &str) {
        let key = provider_turn_key(thread_id, turn_id);
        let mut ids = self.turn_message_ids.get(&key).unwrap_or_default();
        if !ids.iter().any(|id| id == message_id) {
            ids.push(message_id.to_owned());
        }
        self.turn_message_ids.set(key, ids);
    }

    fn forget_message_id(&mut self, thread_id: &str, turn_id: &str, message_id: &str) {
        let key = provider_turn_key(thread_id, turn_id);
        let Some(mut ids) = self.turn_message_ids.get(&key) else { return };
        ids.retain(|id| id != message_id);
        if ids.is_empty() {
            self.turn_message_ids.invalidate(&key);
        } else {
            self.turn_message_ids.set(key, ids);
        }
    }

    fn message_ids_for_turn(&mut self, thread_id: &str, turn_id: &str) -> Vec<String> {
        self.turn_message_ids.get(&provider_turn_key(thread_id, turn_id)).unwrap_or_default()
    }

    fn segment(&mut self, thread_id: &str, turn_id: &str, role: Role) -> Option<SegmentState> {
        self.segments.get(&segment_state_key(thread_id, turn_id, role))
    }

    fn active_message_id(&mut self, thread_id: &str, turn_id: &str, role: Role) -> Option<String> {
        self.segment(thread_id, turn_id, role).and_then(|state| state.active_message_id)
    }

    /// `startAssistantSegmentForTurn`.
    fn start_segment(&mut self, thread_id: &str, turn_id: &str, base_key: &str, role: Role) -> String {
        let next = match self.segment(thread_id, turn_id, role) {
            None => SegmentState {
                base_key: base_key.to_owned(),
                next_segment_index: 1,
                active_message_id: Some(segment_message_id(base_key, 0, role)),
            },
            Some(state) => {
                // Reasoning never resets the index: summary → raw → summary would reuse an id.
                let reuse_index = state.base_key == base_key || role == Role::Reasoning;
                let segment_index = if reuse_index { state.next_segment_index } else { 0 };
                SegmentState {
                    base_key: base_key.to_owned(),
                    next_segment_index: if reuse_index { state.next_segment_index + 1 } else { 1 },
                    active_message_id: Some(segment_message_id(base_key, segment_index, role)),
                }
            }
        };
        let id = next.active_message_id.clone().unwrap_or_default();
        self.segments.set(segment_state_key(thread_id, turn_id, role), next);
        id
    }

    fn clear_segment(&mut self, thread_id: &str, turn_id: &str, role: Role) {
        self.segments.invalidate(&segment_state_key(thread_id, turn_id, role));
    }

    fn take_buffered(&mut self, message_id: &str) -> String {
        let key = message_id.to_owned();
        let text = self.buffered_text.get(&key).unwrap_or_default();
        self.buffered_text.invalidate(&key);
        text
    }

    /// `clearAssistantMessageState`.
    fn clear_message_state(&mut self, message_id: &str) {
        let key = message_id.to_owned();
        self.buffered_text.invalidate(&key);
        self.last_delivery_at.invalidate(&key);
        self.reasoning_part_index.invalidate(&key);
        self.reasoning_started_at.invalidate(&key);
    }

    /// `reasoningStartedAt(messageId, fallback)`.
    fn reasoning_started_at(&mut self, message_id: &str, fallback: &str) -> String {
        match self.reasoning_started_at.get(&message_id.to_owned()) {
            Some(started) if !started.is_empty() => started,
            _ => fallback.to_owned(),
        }
    }

    /// `appendBufferedAssistantText(messageId, delta, mode, atMillis)`: what to deliver now.
    fn append_buffered(&mut self, message_id: &str, delta: &str, paragraph_mode: bool, at_millis: i64) -> String {
        let key = message_id.to_owned();
        let next_text = match self.buffered_text.get(&key) {
            Some(text) => format!("{text}{delta}"),
            None => delta.to_owned(),
        };
        let (ready, rest) = if paragraph_mode {
            split_buffered_assistant_text(&next_text)
        } else {
            (String::new(), next_text.clone())
        };
        let last_delivered_at = self.last_delivery_at.get(&key);
        let paced = last_delivered_at.is_none_or(|last| at_millis - last >= MIN_ASSISTANT_DELIVERY_INTERVAL_MS);
        if paced && has_renderable_text(&ready) && len16(&rest) <= MAX_BUFFERED_ASSISTANT_CHARS {
            if rest.is_empty() {
                self.buffered_text.invalidate(&key);
            } else {
                self.buffered_text.set(key.clone(), rest);
            }
            self.last_delivery_at.set(key, at_millis);
            return ready;
        }
        if len16(&next_text) <= MAX_BUFFERED_ASSISTANT_CHARS {
            self.buffered_text.set(key, next_text);
            return String::new();
        }
        // Safety valve: deliver the whole buffer.
        self.buffered_text.invalidate(&key);
        next_text
    }

    /// `clearTurnStateForSession(threadId)`.
    fn clear_turn_state_for_session(&mut self, thread_id: &str) {
        let prefix = format!("{thread_id}:");
        let plan_prefix = format!("plan:{thread_id}:");
        for key in self.turn_message_ids.keys() {
            if !key.starts_with(&prefix) {
                continue;
            }
            if let Some(ids) = self.turn_message_ids.get(&key) {
                for id in ids {
                    self.clear_message_state(&id);
                }
            }
            self.turn_message_ids.invalidate(&key);
        }
        for key in self.segments.keys() {
            if key.starts_with(&prefix) {
                self.segments.invalidate(&key);
            }
        }
        for key in self.buffered_plans.keys() {
            if key.starts_with(&plan_prefix) {
                self.buffered_plans.invalidate(&key);
            }
        }
        for key in self.task_descriptions.keys() {
            if key.starts_with(&prefix) {
                self.task_descriptions.invalidate(&key);
            }
        }
    }
}

/// What reaches the lifecycle worker (`RuntimeIngestionInput`).
enum Input {
    Runtime(Value),
    /// `thread.turn-start-requested` (processed as a no-op, kept for ordering parity).
    Domain,
    /// A diff whose workspace the diff worker confirmed is a git repository.
    Diff(Value),
}

/// Inputs of one finalize (`finalizeAssistantMessage`).
struct Finalize<'a> {
    event: &'a Value,
    thread_id: &'a str,
    message_id: &'a str,
    turn_id: Option<&'a str>,
    created_at: &'a str,
    command_tag: &'a str,
    final_delta_command_tag: &'a str,
    fallback_text: Option<&'a str>,
    has_projected_message: bool,
}

/// Inputs of `finalizeActiveSegmentForTurn`.
struct FinalizeSegment<'a> {
    event: &'a Value,
    thread_id: &'a str,
    turn_id: &'a str,
    created_at: &'a str,
    command_tag: &'a str,
    final_delta_command_tag: &'a str,
    has_projected_message: bool,
    flushed: Option<&'a HashSet<String>>,
    role: Role,
    fallback_text: Option<&'a str>,
}

/// The processing core, shared by the workers.
struct Core {
    deps: IngestionDeps,
    state: Mutex<State>,
    strict_guard: bool,
}

impl Core {
    fn command_id(&self, event: &Value, tag: &str) -> String {
        format!("provider:{}:{tag}:{}", str_of(event, "eventId").unwrap_or(""), (self.deps.uuids)())
    }

    async fn dispatch(&self, command: Value) -> Result<(), TaggedError> {
        dispatch_json(&*self.deps.engine, command).await.map(|_| ())
    }

    async fn streaming_mode(&self, project_id: Option<&str>) -> Result<String, TaggedError> {
        let settings = self
            .deps
            .settings
            .get_settings()
            .await
            .map_err(|error| TaggedError::new("ServerSettingsError", format!("{error:?}")))?;
        Ok(response_streaming_mode(&resolve_project_settings(&settings_json(&settings), project_id)))
    }

    /// `getThreadMessageById(threadId, messageId)`.
    async fn thread_message(&self, thread_id: &str, message_id: &str) -> Result<Option<zc_db::repos::thread_messages::ProjectionThreadMessage>, TaggedError> {
        Ok(self
            .deps
            .reads
            .message(&MessageId::new(message_id))
            .await?
            .filter(|message| message.thread_id == thread_id))
    }

    /// `getExpectedProviderTurnIdForThread`: the provider session's active turn.
    async fn expected_provider_turn_id(&self, thread_id: &str) -> Option<String> {
        self.deps
            .providers
            .list_sessions()
            .await
            .into_iter()
            .find(|session| str_of(&session.0, "threadId") == Some(thread_id))
            .and_then(|session| str_of(&session.0, "activeTurnId").map(str::to_owned))
    }

    /// `flushBufferedAssistantMessage`: true when something was delivered.
    #[allow(clippy::too_many_arguments)]
    async fn flush_buffered(
        &self,
        state: &mut State,
        event: &Value,
        thread_id: &str,
        message_id: &str,
        turn_id: Option<&str>,
        created_at: &str,
        tag: &str,
    ) -> Result<bool, TaggedError> {
        let text = state.take_buffered(message_id);
        if !has_renderable_text(&text) {
            return Ok(false);
        }
        let reasoning = role_of(message_id) == Role::Reasoning;
        let created_at = if reasoning {
            state.reasoning_started_at(message_id, created_at)
        } else {
            created_at.to_owned()
        };
        let command = Obj::new()
            .set(
                "type",
                if reasoning {
                    "thread.message.reasoning.delta"
                } else {
                    "thread.message.assistant.delta"
                },
            )
            .set("commandId", self.command_id(event, tag))
            .set("threadId", thread_id)
            .set("messageId", message_id)
            .set("delta", text)
            .set_if(turn_id.is_some(), "turnId", || json!(turn_id))
            .set("createdAt", created_at)
            .build();
        self.dispatch(command).await?;
        Ok(true)
    }

    /// `finalizeAssistantMessage`.
    async fn finalize_message(&self, state: &mut State, input: Finalize<'_>) -> Result<(), TaggedError> {
        let buffered = state.take_buffered(input.message_id);
        let text = if !buffered.is_empty() {
            buffered
        } else {
            match input.fallback_text {
                Some(fallback) if !trim(fallback).is_empty() => fallback.to_owned(),
                _ => String::new(),
            }
        };
        let renderable = has_renderable_text(&text);
        let reasoning = role_of(input.message_id) == Role::Reasoning;
        if renderable {
            let created_at = if reasoning {
                state.reasoning_started_at(input.message_id, input.created_at)
            } else {
                input.created_at.to_owned()
            };
            let command = Obj::new()
                .set(
                    "type",
                    if reasoning {
                        "thread.message.reasoning.delta"
                    } else {
                        "thread.message.assistant.delta"
                    },
                )
                .set("commandId", self.command_id(input.event, input.final_delta_command_tag))
                .set("threadId", input.thread_id)
                .set("messageId", input.message_id)
                .set("delta", text)
                .set_if(input.turn_id.is_some(), "turnId", || json!(input.turn_id))
                .set("createdAt", created_at)
                .build();
            self.dispatch(command).await?;
        }
        if input.has_projected_message || renderable {
            let command = Obj::new()
                .set(
                    "type",
                    if reasoning {
                        "thread.message.reasoning.complete"
                    } else {
                        "thread.message.assistant.complete"
                    },
                )
                .set("commandId", self.command_id(input.event, input.command_tag))
                .set("threadId", input.thread_id)
                .set("messageId", input.message_id)
                .set_if(input.turn_id.is_some(), "turnId", || json!(input.turn_id))
                .set("createdAt", input.created_at)
                .build();
            self.dispatch(command).await?;
        }
        state.clear_message_state(input.message_id);
        Ok(())
    }

    /// `finalizeActiveSegmentForTurn`.
    async fn finalize_active_segment(&self, state: &mut State, input: FinalizeSegment<'_>) -> Result<(), TaggedError> {
        let Some(active) = state.active_message_id(input.thread_id, input.turn_id, input.role) else {
            return Ok(());
        };
        // A block whose deltas already reached the projection must still be completed.
        let already_projected = input.has_projected_message
            || input.flushed.is_some_and(|flushed| flushed.contains(&active))
            || (input.role == Role::Reasoning && self.thread_message(input.thread_id, &active).await?.is_some());
        self.finalize_message(
            state,
            Finalize {
                event: input.event,
                thread_id: input.thread_id,
                message_id: &active,
                turn_id: Some(input.turn_id),
                created_at: input.created_at,
                command_tag: input.command_tag,
                final_delta_command_tag: input.final_delta_command_tag,
                fallback_text: input.fallback_text,
                has_projected_message: already_projected,
            },
        )
        .await?;
        state.forget_message_id(input.thread_id, input.turn_id, &active);
        // The segment index is kept: reasoning blocks of one turn can share a base key.
        if let Some(mut segment) = state.segment(input.thread_id, input.turn_id, input.role) {
            segment.active_message_id = None;
            state.segments.set(segment_state_key(input.thread_id, input.turn_id, input.role), segment);
        }
        Ok(())
    }

    /// `getOrCreateReasoningMessageId`: switching base key closes the open block.
    async fn reasoning_message_id(
        &self,
        state: &mut State,
        event: &Value,
        thread_id: &str,
        turn_id: &str,
        base_key: &str,
        created_at: &str,
    ) -> Result<String, TaggedError> {
        let segment = state.segment(thread_id, turn_id, Role::Reasoning);
        if let Some(active) = segment.as_ref().and_then(|segment| segment.active_message_id.clone()) {
            if segment.as_ref().map(|segment| segment.base_key.as_str()) == Some(base_key) {
                return Ok(active);
            }
            self.finalize_active_segment(
                state,
                FinalizeSegment {
                    event,
                    thread_id,
                    turn_id,
                    created_at,
                    command_tag: "reasoning-complete-on-new-block",
                    final_delta_command_tag: "reasoning-delta-finalize-on-new-block",
                    has_projected_message: false,
                    flushed: None,
                    role: Role::Reasoning,
                    fallback_text: None,
                },
            )
            .await?;
        }
        Ok(state.start_segment(thread_id, turn_id, base_key, Role::Reasoning))
    }

    /// `finalizeBufferedProposedPlan`.
    #[allow(clippy::too_many_arguments)]
    async fn finalize_buffered_plan(
        &self,
        state: &mut State,
        event: &Value,
        thread_id: &str,
        plan_id: &str,
        turn_id: Option<&str>,
        fallback_markdown: Option<&str>,
        updated_at: &str,
    ) -> Result<(), TaggedError> {
        let buffered = state.buffered_plans.get(&plan_id.to_owned());
        let markdown =
            normalize_proposed_plan_markdown(buffered.as_ref().map(|plan| plan.text.as_str())).or_else(|| normalize_proposed_plan_markdown(fallback_markdown));
        let Some(markdown) = markdown else {
            state.buffered_plans.invalidate(&plan_id.to_owned());
            return Ok(());
        };
        let existing = self.deps.reads.proposed_plan(&ThreadId::new(thread_id), plan_id).await?;
        let created_at = match &existing {
            Some(existing) => existing.created_at.clone(),
            None => buffered
                .as_ref()
                .map(|plan| plan.created_at.clone())
                .filter(|at| !at.is_empty())
                .unwrap_or_else(|| updated_at.to_owned()),
        };
        self.dispatch(json!({
            "type": "thread.proposed-plan.upsert",
            "commandId": self.command_id(event, "proposed-plan-upsert"),
            "threadId": thread_id,
            "proposedPlan": {
                "id": plan_id,
                "turnId": turn_id,
                "planMarkdown": markdown,
                "implementedAt": existing.as_ref().and_then(|plan| plan.implemented_at.clone()),
                "implementationThreadId": existing.as_ref().and_then(|plan| plan.implementation_thread_id.clone()),
                "createdAt": created_at,
                "updatedAt": updated_at,
            },
            "createdAt": updated_at,
        }))
        .await?;
        state.buffered_plans.invalidate(&plan_id.to_owned());
        Ok(())
    }

    /// `markSourceProposedPlanImplemented`.
    async fn mark_source_plan_implemented(
        &self,
        source_thread_id: &str,
        source_plan_id: &str,
        implementation_thread_id: &str,
        implemented_at: &str,
    ) -> Result<(), TaggedError> {
        let source_thread = self.deps.reads.thread_runtime_context(&ThreadId::new(source_thread_id)).await?;
        let source_plan = self.deps.reads.proposed_plan(&ThreadId::new(source_thread_id), source_plan_id).await?;
        let (Some(_), Some(plan)) = (source_thread, source_plan) else {
            return Ok(());
        };
        if plan.implemented_at.is_some() {
            return Ok(());
        }
        self.dispatch(json!({
            "type": "thread.proposed-plan.upsert",
            "commandId": format!("provider:source-proposed-plan-implemented:{implementation_thread_id}:{}", (self.deps.uuids)()),
            "threadId": source_thread_id,
            "proposedPlan": {
                "id": plan.plan_id,
                "turnId": plan.turn_id,
                "planMarkdown": plan.plan_markdown,
                "createdAt": plan.created_at,
                "implementedAt": implemented_at,
                "implementationThreadId": implementation_thread_id,
                "updatedAt": implemented_at,
            },
            "createdAt": implemented_at,
        }))
        .await
    }

    /// `processRuntimeEvent(event)`.
    async fn process_runtime_event(&self, event: &Value) -> Result<(), TaggedError> {
        let event_type = str_of(event, "type").unwrap_or("");
        let payload = event.get("payload").cloned().unwrap_or(Value::Null);
        let stream_kind = str_of(&payload, "streamKind");
        if event_type == "content.delta" && !matches!(stream_kind, Some("assistant_text" | "reasoning_text" | "reasoning_summary_text")) {
            return Ok(());
        }
        let event_thread_id = ThreadId::new(str_of(event, "threadId").unwrap_or(""));
        let Some(thread) = self.deps.reads.thread_runtime_context(&event_thread_id).await? else {
            return Ok(());
        };
        let mut state = self.state.lock().await;
        let state = &mut *state;
        let thread_id = str_of(&thread, "id").unwrap_or("").to_owned();
        let project_id = str_of(&thread, "projectId").map(str::to_owned);
        let session = thread.get("session").filter(|session| !session.is_null()).cloned();
        let session_status = session.as_ref().and_then(|session| str_of(session, "status")).map(str::to_owned);

        let now = str_of(event, "createdAt").unwrap_or("").to_owned();
        let event_turn = event_turn_id(event);
        let active_turn_id = session.as_ref().and_then(|session| str_of(session, "activeTurnId")).map(str::to_owned);
        let is_terminal_turn = event_type == "turn.completed" || event_type == "turn.aborted";
        let is_compacted_thread_state = event_type == "thread.state.changed" && str_of(&payload, "state") == Some("compacted");
        let reads_pending = matches!(
            event_type,
            "session.started" | "session.state.changed" | "session.exited" | "thread.started" | "turn.started"
        ) || is_terminal_turn
            || is_compacted_thread_state;
        let pending_turn_start = if reads_pending {
            self.deps.reads.pending_turn_start(&ThreadId::new(&thread_id)).await?
        } else {
            None
        };
        let has_pending_turn_start = pending_turn_start.is_some() && session_status.as_deref() == Some("starting");
        let conflicts_with_active_turn = active_turn_id.is_some() && event_turn.is_some() && !same_id(active_turn_id.as_deref(), event_turn.as_deref());
        let missing_turn_for_active_turn = active_turn_id.is_some() && event_turn.is_none();

        // Steering can make a provider open a new turn without completing the one it supersedes.
        let conflicting_turn_start_is_pending_turn_start = if event_type == "turn.started" && conflicts_with_active_turn {
            same_id(self.expected_provider_turn_id(&thread_id).await.as_deref(), event_turn.as_deref()) && pending_turn_start.is_some()
        } else {
            false
        };

        let should_apply_thread_lifecycle = if !self.strict_guard {
            true
        } else {
            match event_type {
                "session.exited" | "session.started" | "thread.started" => true,
                "turn.started" => !conflicts_with_active_turn || conflicting_turn_start_is_pending_turn_start,
                "turn.completed" | "turn.aborted" => {
                    if conflicts_with_active_turn || missing_turn_for_active_turn {
                        false
                    } else if active_turn_id.is_some() && event_turn.is_some() {
                        same_id(active_turn_id.as_deref(), event_turn.as_deref())
                    } else {
                        // A named completion can recover a lost turn.started; an abort cannot.
                        event_type == "turn.completed" && event_turn.is_some()
                    }
                }
                _ => true,
            }
        };

        let accepted_turn_started_source_plan = if event_type == "turn.started" && should_apply_thread_lifecycle {
            match &event_turn {
                None => None,
                Some(turn_id) => {
                    let expected = self.expected_provider_turn_id(&thread_id).await;
                    if !same_id(expected.as_deref(), Some(turn_id)) {
                        None
                    } else {
                        self.deps
                            .reads
                            .pending_turn_start(&ThreadId::new(&thread_id))
                            .await?
                            .and_then(|pending| Some((pending.source_proposed_plan_thread_id?, pending.source_proposed_plan_id?)))
                    }
                }
            }
        } else {
            None
        };

        if matches!(
            event_type,
            "session.started" | "session.state.changed" | "session.exited" | "thread.started" | "turn.started"
        ) || is_terminal_turn
        {
            let runtime_state = str_of(&payload, "state");
            let status: &str = match event_type {
                "session.state.changed" => {
                    let runtime_status = session_status_from_runtime_state(runtime_state.unwrap_or(""));
                    if has_pending_turn_start && runtime_status == "ready" {
                        "starting"
                    } else {
                        runtime_status
                    }
                }
                "turn.started" => "running",
                "session.exited" => "stopped",
                "turn.aborted" => "interrupted",
                "turn.completed" => {
                    if normalize_runtime_turn_state(runtime_state) == "failed" {
                        "error"
                    } else {
                        "ready"
                    }
                }
                _ => {
                    if active_turn_id.is_some() {
                        "running"
                    } else if has_pending_turn_start {
                        "starting"
                    } else {
                        "ready"
                    }
                }
            };
            let next_active_turn_id: Option<String> = if event_type == "turn.started" {
                event_turn.clone()
            } else if is_terminal_turn
                || event_type == "session.exited"
                || (event_type == "session.state.changed" && !matches!(session_status_from_runtime_state(runtime_state.unwrap_or("")), "starting" | "running"))
            {
                None
            } else {
                active_turn_id.clone()
            };
            let previous_error = session.as_ref().and_then(|session| str_of(session, "lastError")).map(str::to_owned);
            let last_error: Option<String> = if event_type == "session.state.changed" && runtime_state == Some("error") {
                Some(
                    str_of(&payload, "reason")
                        .map(str::to_owned)
                        .or(previous_error.clone())
                        .unwrap_or_else(|| "Provider session error".into()),
                )
            } else if event_type == "turn.completed" && normalize_runtime_turn_state(runtime_state) == "failed" {
                Some(
                    str_of(&payload, "errorMessage")
                        .map(str::to_owned)
                        .or(previous_error.clone())
                        .unwrap_or_else(|| "Turn failed".into()),
                )
            } else if status == "ready" || status == "interrupted" {
                None
            } else {
                previous_error
            };

            if should_apply_thread_lifecycle {
                if event_type == "turn.started" {
                    if let Some((source_thread_id, source_plan_id)) = &accepted_turn_started_source_plan {
                        if let Err(error) = self.mark_source_plan_implemented(source_thread_id, source_plan_id, &thread_id, &now).await {
                            tracing::warn!(event_id = str_of(event, "eventId"), event_type, cause = %pretty(&error), "provider runtime ingestion failed to mark source proposed plan");
                        }
                    }
                }
                let session_value = Obj::new()
                    .set("threadId", thread_id.as_str())
                    .set("status", status)
                    .set("providerName", event.get("provider").cloned().unwrap_or(Value::Null))
                    .copy_defined(event, "providerInstanceId")
                    .set(
                        "runtimeMode",
                        session
                            .as_ref()
                            .and_then(|session| session.get("runtimeMode").cloned())
                            .unwrap_or(json!("full-access")),
                    )
                    .set("activeTurnId", json!(next_active_turn_id))
                    .set("lastError", json!(last_error))
                    .set("updatedAt", now.as_str())
                    .build();
                self.dispatch(json!({
                    "type": "thread.session.set",
                    "commandId": self.command_id(event, "thread-session-set"),
                    "threadId": thread_id,
                    "session": session_value,
                    "createdAt": now,
                }))
                .await?;
            }
        }

        let delta = str_of(&payload, "delta").unwrap_or("");
        let assistant_delta = (event_type == "content.delta" && stream_kind == Some("assistant_text")).then_some(delta);
        let reasoning_delta = (event_type == "content.delta" && matches!(stream_kind, Some("reasoning_text" | "reasoning_summary_text"))).then_some(delta);
        let proposed_plan_delta = (event_type == "turn.proposed.delta").then_some(delta);

        // Every close path of a thinking block is keyed by turn, so a block without one is dropped.
        if let (Some(reasoning_delta), Some(turn_id)) = (reasoning_delta.filter(|delta| !delta.is_empty()), event_turn.as_deref()) {
            let stream = if stream_kind == Some("reasoning_summary_text") { "summary" } else { "raw" };
            let base_key = format!("{stream}:{}", base_key_of(event));
            let message_id = self.reasoning_message_id(state, event, &thread_id, turn_id, &base_key, &now).await?;
            state.remember_message_id(&thread_id, turn_id, &message_id);
            if state.reasoning_started_at.get(&message_id).unwrap_or_default().is_empty() {
                state.reasoning_started_at.set(message_id.clone(), now.clone());
            }
            let mut delta = reasoning_delta.to_owned();
            let part_index = payload
                .get("summaryIndex")
                .filter(|value| !value.is_null())
                .or_else(|| payload.get("contentIndex").filter(|value| !value.is_null()))
                .and_then(Value::as_i64);
            if let Some(part_index) = part_index {
                let last_index = state.reasoning_part_index.get(&message_id).unwrap_or(-1);
                if last_index >= 0 && last_index != part_index {
                    delta = format!("\n\n{delta}");
                }
                state.reasoning_part_index.set(message_id.clone(), part_index);
            }
            // Reasoning is never delivered token by token.
            let mode = self.streaming_mode(project_id.as_deref()).await?;
            let paragraph = mode == "token" || mode == "paragraph";
            let spill = state.append_buffered(&message_id, &delta, paragraph, self.deps.clock.now_millis());
            if !spill.is_empty() {
                let created_at = state.reasoning_started_at(&message_id, &now);
                self.dispatch(json!({
                    "type": "thread.message.reasoning.delta",
                    "commandId": self.command_id(event, "reasoning-delta-buffer-spill"),
                    "threadId": thread_id,
                    "messageId": message_id,
                    "delta": spill,
                    "turnId": turn_id,
                    "createdAt": created_at,
                }))
                .await?;
            }
        }

        if let Some(assistant_delta) = assistant_delta.filter(|delta| !delta.is_empty()) {
            let turn_id = event_turn.as_deref();
            // Visible text ends the thinking block before it.
            if let Some(turn_id) = turn_id {
                self.finalize_active_segment(
                    state,
                    FinalizeSegment {
                        event,
                        thread_id: &thread_id,
                        turn_id,
                        created_at: &now,
                        command_tag: "reasoning-complete-on-assistant-text",
                        final_delta_command_tag: "reasoning-delta-finalize-on-assistant-text",
                        has_projected_message: false,
                        flushed: None,
                        role: Role::Reasoning,
                        fallback_text: None,
                    },
                )
                .await?;
            }
            let message_id = match turn_id {
                None => segment_message_id(&base_key_of(event), 0, Role::Assistant),
                Some(turn_id) => match state.active_message_id(&thread_id, turn_id, Role::Assistant) {
                    Some(active) => active,
                    None => state.start_segment(&thread_id, turn_id, &base_key_of(event), Role::Assistant),
                },
            };
            if let Some(turn_id) = turn_id {
                state.remember_message_id(&thread_id, turn_id, &message_id);
            }
            let mode = self.streaming_mode(project_id.as_deref()).await?;
            if mode != "token" {
                // Pace on the server clock: some providers stamp every delta with the part's start.
                let spill = state.append_buffered(&message_id, assistant_delta, mode == "paragraph", self.deps.clock.now_millis());
                if !spill.is_empty() {
                    let command = Obj::new()
                        .set("type", "thread.message.assistant.delta")
                        .set("commandId", self.command_id(event, "assistant-delta-buffer-spill"))
                        .set("threadId", thread_id.as_str())
                        .set("messageId", message_id.as_str())
                        .set("delta", spill)
                        .set_if(turn_id.is_some(), "turnId", || json!(turn_id))
                        .set("createdAt", now.as_str())
                        .build();
                    self.dispatch(command).await?;
                }
            } else {
                let command = Obj::new()
                    .set("type", "thread.message.assistant.delta")
                    .set("commandId", self.command_id(event, "assistant-delta"))
                    .set("threadId", thread_id.as_str())
                    .set("messageId", message_id.as_str())
                    .set("delta", assistant_delta)
                    .set_if(turn_id.is_some(), "turnId", || json!(turn_id))
                    .set("createdAt", now.as_str())
                    .build();
                self.dispatch(command).await?;
            }
        }

        let pause_for_user_turn_id =
            if event_type == "request.opened" || (event_type == "user-input.requested" && str_of(&payload, "responseMode") != Some("message")) {
                event_turn.clone()
            } else {
                None
            };
        if let Some(turn_id) = pause_for_user_turn_id.as_deref() {
            let has_projected_message = self
                .deps
                .reads
                .has_assistant_message_for_turn(&ThreadId::new(&thread_id), &TurnId::new(turn_id), true)
                .await?;
            let mode = self.streaming_mode(project_id.as_deref()).await?;
            let request_opened = event_type == "request.opened";
            let mut flushed = HashSet::new();
            if mode != "token" {
                let tag = if request_opened {
                    "assistant-delta-flush-on-request-opened"
                } else {
                    "assistant-delta-flush-on-user-input-requested"
                };
                for message_id in state.message_ids_for_turn(&thread_id, turn_id) {
                    if self.flush_buffered(state, event, &thread_id, &message_id, Some(turn_id), &now, tag).await? {
                        flushed.insert(message_id);
                    }
                }
            }
            self.finalize_active_segment(
                state,
                FinalizeSegment {
                    event,
                    thread_id: &thread_id,
                    turn_id,
                    created_at: &now,
                    command_tag: "reasoning-complete-on-pause",
                    final_delta_command_tag: "reasoning-delta-finalize-on-pause",
                    has_projected_message: false,
                    flushed: Some(&flushed),
                    role: Role::Reasoning,
                    fallback_text: None,
                },
            )
            .await?;
            self.finalize_active_segment(
                state,
                FinalizeSegment {
                    event,
                    thread_id: &thread_id,
                    turn_id,
                    created_at: &now,
                    command_tag: if request_opened {
                        "assistant-complete-on-request-opened"
                    } else {
                        "assistant-complete-on-user-input-requested"
                    },
                    final_delta_command_tag: if request_opened {
                        "assistant-delta-finalize-on-request-opened"
                    } else {
                        "assistant-delta-finalize-on-user-input-requested"
                    },
                    has_projected_message,
                    flushed: Some(&flushed),
                    role: Role::Assistant,
                    fallback_text: None,
                },
            )
            .await?;
        }

        if let Some(plan_delta) = proposed_plan_delta.filter(|delta| !delta.is_empty()) {
            let plan_id = proposed_plan_id_from_event(event, &thread_id);
            let existing = state.buffered_plans.get(&plan_id);
            let created_at = existing
                .as_ref()
                .map(|plan| plan.created_at.clone())
                .filter(|at| !at.is_empty())
                .unwrap_or_else(|| now.clone());
            state.buffered_plans.set(
                plan_id,
                BufferedPlan {
                    text: format!("{}{plan_delta}", existing.map(|plan| plan.text).unwrap_or_default()),
                    created_at,
                },
            );
        }

        // Tool work ends the thinking block that led to it.
        if event_type == "item.started" && is_tool_lifecycle_item_type(str_of(&payload, "itemType")) {
            if let Some(turn_id) = event_turn.as_deref() {
                self.finalize_active_segment(
                    state,
                    FinalizeSegment {
                        event,
                        thread_id: &thread_id,
                        turn_id,
                        created_at: &now,
                        command_tag: "reasoning-complete-on-tool-start",
                        final_delta_command_tag: "reasoning-delta-finalize-on-tool-start",
                        has_projected_message: false,
                        flushed: None,
                        role: Role::Reasoning,
                        fallback_text: None,
                    },
                )
                .await?;
            }
        }

        if event_type == "item.completed" && str_of(&payload, "itemType") == Some("reasoning") {
            if let Some(turn_id) = event_turn.as_deref() {
                let active = state.active_message_id(&thread_id, turn_id, Role::Reasoning);
                // The item detail is a whole-block snapshot: it may only stand in for deltas
                // that never arrived.
                let existing = match &active {
                    Some(active) => self.thread_message(&thread_id, active).await?,
                    None => None,
                };
                let detail = str_of(&payload, "detail");
                let fallback_text = detail.filter(|detail| !trim(detail).is_empty() && existing.as_ref().is_none_or(|message| message.text.is_empty()));
                match &active {
                    None => {
                        // Segment state outlives a closed block: its presence means the turn
                        // already streamed a trace.
                        let already_streamed = state.segment(&thread_id, turn_id, Role::Reasoning).is_some();
                        if let (Some(fallback_text), false) = (fallback_text, already_streamed) {
                            let item_key = event
                                .get("itemId")
                                .filter(|value| !value.is_null())
                                .or_else(|| event.get("eventId"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let snapshot_id = segment_message_id(&format!("snapshot:{item_key}"), 0, Role::Reasoning);
                            if self.thread_message(&thread_id, &snapshot_id).await?.is_none() {
                                self.dispatch(json!({
                                    "type": "thread.message.reasoning.delta",
                                    "commandId": self.command_id(event, "reasoning-delta-snapshot"),
                                    "threadId": thread_id,
                                    "messageId": snapshot_id,
                                    "delta": fallback_text,
                                    "turnId": turn_id,
                                    "createdAt": now,
                                }))
                                .await?;
                                self.dispatch(json!({
                                    "type": "thread.message.reasoning.complete",
                                    "commandId": self.command_id(event, "reasoning-complete-snapshot"),
                                    "threadId": thread_id,
                                    "messageId": snapshot_id,
                                    "turnId": turn_id,
                                    "createdAt": now,
                                }))
                                .await?;
                            }
                        }
                    }
                    Some(_) => {
                        self.finalize_active_segment(
                            state,
                            FinalizeSegment {
                                event,
                                thread_id: &thread_id,
                                turn_id,
                                created_at: &now,
                                command_tag: "reasoning-complete",
                                final_delta_command_tag: "reasoning-delta-finalize",
                                has_projected_message: existing.is_some(),
                                flushed: None,
                                role: Role::Reasoning,
                                fallback_text,
                            },
                        )
                        .await?;
                    }
                }
            }
        }

        if event_type == "item.completed" && str_of(&payload, "itemType") == Some("assistant_message") {
            let completion_message_id = format!("assistant:{}", base_key_of(event));
            let fallback_text = str_of(&payload, "detail");
            let turn_id = event_turn.as_deref();
            if let Some(turn_id) = turn_id {
                self.finalize_active_segment(
                    state,
                    FinalizeSegment {
                        event,
                        thread_id: &thread_id,
                        turn_id,
                        created_at: &now,
                        command_tag: "reasoning-complete-on-assistant-completion",
                        final_delta_command_tag: "reasoning-delta-finalize-on-assistant-completion",
                        has_projected_message: false,
                        flushed: None,
                        role: Role::Reasoning,
                        fallback_text: None,
                    },
                )
                .await?;
            }
            let active = turn_id.and_then(|turn_id| state.active_message_id(&thread_id, turn_id, Role::Assistant));
            let message_id = active.clone().unwrap_or(completion_message_id);
            let existing = self.thread_message(&thread_id, &message_id).await?;
            let has_assistant_messages_for_turn = match turn_id {
                None => false,
                Some(turn_id) => {
                    self.deps
                        .reads
                        .has_assistant_message_for_turn(&ThreadId::new(&thread_id), &TurnId::new(turn_id), false)
                        .await?
                }
            };
            let apply_fallback = existing.as_ref().is_none_or(|message| message.text.is_empty());
            let skip_redundant =
                active.is_none() && turn_id.is_some() && has_assistant_messages_for_turn && fallback_text.is_none_or(|text| trim(text).is_empty());
            if !skip_redundant {
                if let (Some(turn_id), None) = (turn_id, &active) {
                    state.remember_message_id(&thread_id, turn_id, &message_id);
                }
                self.finalize_message(
                    state,
                    Finalize {
                        event,
                        thread_id: &thread_id,
                        message_id: &message_id,
                        turn_id,
                        created_at: &now,
                        command_tag: "assistant-complete",
                        final_delta_command_tag: "assistant-delta-finalize",
                        fallback_text: fallback_text.filter(|_| apply_fallback),
                        has_projected_message: existing.is_some(),
                    },
                )
                .await?;
                if let Some(turn_id) = turn_id {
                    state.forget_message_id(&thread_id, turn_id, &message_id);
                }
            }
            if let Some(turn_id) = turn_id {
                state.clear_segment(&thread_id, turn_id, Role::Assistant);
            }
        }

        if event_type == "turn.proposed.completed" {
            let plan_id = proposed_plan_id_from_event(event, &thread_id);
            self.finalize_buffered_plan(
                state,
                event,
                &thread_id,
                &plan_id,
                event_turn.as_deref(),
                str_of(&payload, "planMarkdown"),
                &now,
            )
            .await?;
        }

        if is_terminal_turn {
            if let Some(turn_id) = event_turn.as_deref() {
                let lifecycle = self.deps.reads.user_input_lifecycle(&ThreadId::new(&thread_id)).await?;
                let mut pending_request_ids: Vec<String> = Vec::new();
                for activity in &lifecycle {
                    let Some(request_id) = activity.payload.get("requestId").and_then(Value::as_str) else {
                        continue;
                    };
                    if activity.kind == "user-input.requested"
                        && activity.turn_id.as_deref() == Some(turn_id)
                        && activity.payload.get("responseMode").and_then(Value::as_str) != Some("message")
                    {
                        if !pending_request_ids.iter().any(|id| id == request_id) {
                            pending_request_ids.push(request_id.to_owned());
                        }
                    } else if activity.kind == "user-input.resolved" {
                        pending_request_ids.retain(|id| id != request_id);
                    }
                }
                // A terminal turn cannot take native answers; message-mode questions outlive it.
                for request_id in pending_request_ids {
                    self.dispatch(json!({
                        "type": "thread.activity.append",
                        "commandId": self.command_id(event, "terminal-user-input-resolved"),
                        "threadId": thread_id,
                        "activity": {
                            "id": format!("{}:user-input-resolved:{request_id}", str_of(event, "eventId").unwrap_or("")),
                            "createdAt": now,
                            "tone": "info",
                            "kind": "user-input.resolved",
                            "summary": "User input dismissed",
                            "payload": {"requestId": request_id},
                            "turnId": turn_id,
                        },
                        "createdAt": now,
                    }))
                    .await?;
                }
                for message_id in state.message_ids_for_turn(&thread_id, turn_id) {
                    let existing = self.thread_message(&thread_id, &message_id).await?;
                    self.finalize_message(
                        state,
                        Finalize {
                            event,
                            thread_id: &thread_id,
                            message_id: &message_id,
                            turn_id: Some(turn_id),
                            created_at: &now,
                            command_tag: "assistant-complete-finalize",
                            final_delta_command_tag: "assistant-delta-finalize-fallback",
                            fallback_text: None,
                            has_projected_message: existing.is_some(),
                        },
                    )
                    .await?;
                }
                state.turn_message_ids.invalidate(&provider_turn_key(&thread_id, turn_id));
                state.clear_segment(&thread_id, turn_id, Role::Assistant);
                state.clear_segment(&thread_id, turn_id, Role::Reasoning);
                let plan_id = proposed_plan_id_for_turn(&thread_id, turn_id);
                self.finalize_buffered_plan(state, event, &thread_id, &plan_id, Some(turn_id), None, &now)
                    .await?;
            }
        }

        if event_type == "session.exited" {
            state.clear_turn_state_for_session(&thread_id);
        }

        if event_type == "runtime.error" {
            let should_apply =
                !self.strict_guard || active_turn_id.is_none() || event_turn.is_none() || same_id(active_turn_id.as_deref(), event_turn.as_deref());
            if should_apply {
                let session_value = Obj::new()
                    .set("threadId", thread_id.as_str())
                    .set("status", "error")
                    .set("providerName", event.get("provider").cloned().unwrap_or(Value::Null))
                    .copy_defined(event, "providerInstanceId")
                    .set(
                        "runtimeMode",
                        session
                            .as_ref()
                            .and_then(|session| session.get("runtimeMode").cloned())
                            .unwrap_or(json!("full-access")),
                    )
                    .set("activeTurnId", json!(event_turn))
                    .set("lastError", payload.get("message").cloned().unwrap_or(Value::Null))
                    .set("updatedAt", now.as_str())
                    .build();
                self.dispatch(json!({
                    "type": "thread.session.set",
                    "commandId": self.command_id(event, "runtime-error-session-set"),
                    "threadId": thread_id,
                    "session": session_value,
                    "createdAt": now,
                }))
                .await?;
            }
        }

        if event_type == "thread.metadata.updated" {
            if let Some(name) = str_of(&payload, "name").filter(|name| !name.is_empty()) {
                let title_state = thread.get("titleState").filter(|state| !state.is_null());
                let title = str_of(&thread, "title").unwrap_or("");
                if title_state.and_then(|state| str_of(state, "source")) != Some("manual") && can_replace_thread_title(title, None) {
                    self.dispatch(json!({
                        "type": "thread.title.generate.complete",
                        "commandId": self.command_id(event, "thread-meta-update"),
                        "threadId": thread_id,
                        "title": name,
                        "expectedTitle": title,
                        "expectedVersion": title_state.and_then(|state| state.get("version").cloned()).unwrap_or(Value::Null),
                        "needsRefinement": false,
                    }))
                    .await?;
                }
            }
        }

        if event_type == "task.started" || event_type == "task.progress" {
            if let (Some(description), Some(task_id)) = (str_of(&payload, "description"), str_of(&payload, "taskId")) {
                let description = trim(description);
                if !description.is_empty() {
                    state.task_descriptions.set(format!("{thread_id}:{task_id}"), description.to_owned());
                }
            }
        }

        // Working-indicator plan progress; stale (superseded) turns neither set nor clear it.
        if event_type == "session.exited" {
            self.deps.plan_progress.clear_thread_plan_progress(&thread_id);
        } else if !conflicts_with_active_turn {
            if event_type == "turn.plan.updated" {
                let plan = payload.get("plan").and_then(Value::as_array).cloned().unwrap_or_default();
                self.deps.plan_progress.record_plan_progress(&thread_id, &plan);
            } else if is_terminal_turn && should_apply_thread_lifecycle {
                self.deps.plan_progress.clear_thread_plan_progress(&thread_id);
            }
        }

        match event_type {
            "task.started" | "task.progress" | "task.updated" | "task.completed" => {
                self.deps.liveness.record_task_liveness(TaskLivenessInput {
                    thread_id: &thread_id,
                    task_id: str_of(&payload, "taskId").unwrap_or(""),
                    task_type: str_of(&payload, "taskType"),
                    status: str_of(&payload, "status"),
                    kind: match event_type {
                        "task.started" => TaskTransition::Started,
                        "task.progress" => TaskTransition::Progress,
                        "task.updated" => TaskTransition::Updated,
                        _ => TaskTransition::Completed,
                    },
                    agent_id: str_of(&payload, "agentId"),
                });
            }
            "session.exited" => self.deps.liveness.clear_thread_liveness(&thread_id),
            _ => {}
        }

        let mut task_title: Option<String> = None;
        if event_type == "task.completed" {
            let task_id = str_of(&payload, "taskId").unwrap_or("");
            task_title = state.task_descriptions.get(&format!("{thread_id}:{task_id}")).filter(|title| !title.is_empty());
            if task_title.is_none() {
                let latest = self.deps.reads.latest_task_activity(&ThreadId::new(&thread_id), task_id).await?;
                task_title = find_task_title_in_activities(&latest.into_iter().collect::<Vec<_>>(), task_id);
            }
        }

        let mut activity_event = event.clone();
        if is_compacted_thread_state && event.get("requestId").is_none() {
            if let Some(pending) = &pending_turn_start {
                let session_matches = session_status.as_deref() == Some("starting")
                    && active_turn_id.is_none()
                    && same_id(session.as_ref().and_then(|session| str_of(session, "providerName")), str_of(event, "provider"))
                    && same_id(
                        session.as_ref().and_then(|session| str_of(session, "providerInstanceId")),
                        str_of(event, "providerInstanceId"),
                    );
                let after_request = match (crate::js::parse_date_millis(&now), crate::js::parse_date_millis(&pending.requested_at)) {
                    (Some(at), Some(requested)) => at >= requested,
                    _ => false,
                };
                if session_matches && after_request {
                    if let Some(message) = self.thread_message(&thread_id, &pending.message_id).await? {
                        let no_attachments = message.attachments.as_ref().and_then(Value::as_array).is_none_or(Vec::is_empty);
                        if message.role == "user" && no_attachments && trim(&message.text).to_lowercase() == "/compact" {
                            activity_event["requestId"] = json!(pending.message_id);
                        }
                    }
                }
            }
        }
        if str_of(&activity_event, "type") == Some("thread.state.changed")
            && activity_event["payload"]["state"] == json!("compacted")
            && (activity_event["payload"].get("beforeTokens").is_none() || activity_event["payload"].get("afterTokens").is_none())
        {
            let rows = self
                .deps
                .reads
                .activities(&ThreadId::new(&thread_id), &["context-window.updated", "context-compaction"], Some(500))
                .await?;
            if let Some((before, after)) = compacted_token_counts(&rows) {
                let payload = &mut activity_event["payload"];
                if payload.get("beforeTokens").is_none() {
                    payload["beforeTokens"] = before;
                }
                if payload.get("afterTokens").is_none() {
                    payload["afterTokens"] = after;
                }
            }
        }

        for activity in runtime_event_to_activities(&activity_event, task_title.as_deref()) {
            let created_at = activity["createdAt"].clone();
            self.dispatch(json!({
                "type": "thread.activity.append",
                "commandId": self.command_id(event, "thread-activity-append"),
                "threadId": thread_id,
                "activity": activity,
                "createdAt": created_at,
            }))
            .await?;
        }
        Ok(())
    }

    /// `recordProviderDiff`: a placeholder checkpoint for a running turn's provider diff.
    async fn record_provider_diff(&self, event: &Value) -> Result<(), TaggedError> {
        let thread_id = ThreadId::new(str_of(event, "threadId").unwrap_or(""));
        let Some(thread) = self.deps.reads.thread_runtime_context(&thread_id).await? else {
            return Ok(());
        };
        let Some(turn_id) = event_turn_id(event) else { return Ok(()) };
        let thread_id = ThreadId::new(str_of(&thread, "id").unwrap_or(""));
        let turn = self.deps.reads.turn(&thread_id, &TurnId::new(&turn_id)).await?;
        if turn.is_none_or(|turn| turn.state != "running") {
            return Ok(());
        }
        let Some(context) = self.deps.reads.thread_checkpoint_context(&thread_id).await? else {
            return Ok(());
        };
        let checkpoints = context["checkpoints"].as_array().cloned().unwrap_or_default();
        // A real capture must not be clobbered, and a duplicate placeholder would make the
        // checkpoint count unstable.
        if checkpoints.iter().any(|checkpoint| str_of(checkpoint, "turnId") == Some(turn_id.as_str())) {
            return Ok(());
        }
        let max_count = checkpoints
            .iter()
            .filter_map(|checkpoint| checkpoint["checkpointTurnCount"].as_i64())
            .max()
            .unwrap_or(0)
            .max(0);
        let now = str_of(event, "createdAt").unwrap_or("");
        self.dispatch(json!({
            "type": "thread.turn.diff.complete",
            "commandId": self.command_id(event, "thread-turn-diff-complete"),
            "threadId": thread_id,
            "turnId": turn_id,
            "completedAt": now,
            "checkpointRef": format!("provider-diff:{}", str_of(event, "eventId").unwrap_or("")),
            "status": "missing",
            "files": [],
            "assistantMessageId": format!("assistant:{}", base_key_of(event)),
            "checkpointTurnCount": max_count + 1,
            "createdAt": now,
        }))
        .await
    }

    async fn process_input(&self, input: Input) {
        let (source, event, result) = match input {
            Input::Runtime(event) => {
                let result = self.process_runtime_event(&event).await;
                ("runtime", event, result)
            }
            Input::Domain => return,
            Input::Diff(event) => {
                let result = self.record_provider_diff(&event).await;
                ("diff", event, result)
            }
        };
        if let Err(error) = result {
            tracing::warn!(
                source,
                event_id = str_of(&event, "eventId"),
                event_type = str_of(&event, "type"),
                cause = %pretty(&error),
                "provider runtime ingestion failed to process event"
            );
        }
    }
}

/// `ProviderRuntimeIngestionService`.
pub struct ProviderRuntimeIngestion {
    core: Arc<Core>,
    worker: DrainableWorker<Input>,
    diff_worker: DrainableWorker<Value>,
    stop: CancellationToken,
}

impl ProviderRuntimeIngestion {
    /// Builds the ingestion and its two workers (nothing is subscribed until [`Self::start`]).
    pub fn new(deps: IngestionDeps, stop: CancellationToken) -> Self {
        let clock = deps.clock.clone();
        let core = Arc::new(Core {
            deps,
            state: Mutex::new(State::new(clock)),
            strict_guard: strict_provider_lifecycle_guard(),
        });
        let worker_core = core.clone();
        let worker = DrainableWorker::start(stop.clone(), move |input: Input| {
            let core = worker_core.clone();
            async move { core.process_input(input).await }
        });
        // Repository detection goes through VCS subprocesses that can hang; it runs on its own
        // worker so a stuck diff never delays the lifecycle worker.
        let diff_core = core.clone();
        let lifecycle = worker.clone();
        let diff_worker = DrainableWorker::start(stop.clone(), move |event: Value| {
            let core = diff_core.clone();
            let lifecycle = lifecycle.clone();
            async move {
                match detect_provider_diff_repository(&core, &event).await {
                    Ok(true) => lifecycle.enqueue(Input::Diff(event)),
                    Ok(false) => {}
                    Err(error) => tracing::warn!(
                        source = "diff",
                        event_id = str_of(&event, "eventId"),
                        event_type = str_of(&event, "type"),
                        cause = %pretty(&error),
                        "provider runtime ingestion failed to process event"
                    ),
                }
            }
        });
        Self {
            core,
            worker,
            diff_worker,
            stop,
        }
    }

    /// `start()`: subscribes to the provider event stream and the domain event stream (both
    /// subscriptions exist when this returns) and feeds the workers.
    pub fn start(&self) {
        let mut runtime_events = self.core.deps.providers.subscribe_events();
        let worker = self.worker.clone();
        let diff_worker = self.diff_worker.clone();
        let stop = self.stop.clone();
        tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    _ = stop.cancelled() => break,
                    event = runtime_events.next() => event,
                };
                let Some(event) = event else { break };
                let event = event.0;
                if str_of(&event, "type") == Some("turn.diff.updated") {
                    diff_worker.enqueue(event);
                } else {
                    worker.enqueue(Input::Runtime(event));
                }
            }
        });
        let mut domain_events = self.core.deps.engine.subscribe_domain_events();
        let worker = self.worker.clone();
        let stop = self.stop.clone();
        tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    _ = stop.cancelled() => break,
                    event = domain_events.next() => event,
                };
                let Some(event) = event else { break };
                if matches!(event, zc_contracts::OrchestrationEvent::ThreadTurnStartRequested(_)) {
                    worker.enqueue(Input::Domain);
                }
            }
        });
    }

    /// Feeds one runtime event to the lifecycle worker, as the provider stream would (replays).
    pub fn enqueue_runtime_event(&self, event: Value) {
        if str_of(&event, "type") == Some("turn.diff.updated") {
            self.diff_worker.enqueue(event);
        } else {
            self.worker.enqueue(Input::Runtime(event));
        }
    }

    /// `drain`: the diff worker feeds the lifecycle worker, so it drains first.
    pub async fn drain(&self) {
        self.diff_worker.drain().await;
        self.worker.drain().await;
    }

    /// Stops both workers and the subscriptions.
    pub fn stop(&self) {
        self.stop.cancel();
    }
}

/// `detectProviderDiffRepository`: whether the diff's workspace is a git repository.
async fn detect_provider_diff_repository(core: &Core, event: &Value) -> Result<bool, TaggedError> {
    if event_turn_id(event).is_none() {
        return Ok(false);
    }
    let context = core
        .deps
        .reads
        .thread_checkpoint_context(&ThreadId::new(str_of(event, "threadId").unwrap_or("")))
        .await?;
    let cwd = context
        .as_ref()
        .and_then(|context| str_of(context, "worktreePath").or_else(|| str_of(context, "workspaceRoot")))
        .filter(|cwd| !cwd.is_empty());
    let Some(cwd) = cwd else { return Ok(false) };
    core.deps.repositories.is_git_repository(cwd).await
}
