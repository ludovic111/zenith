//! `orchestration/decider.ts`: `(command, command read model) → events`, or a rejection.
//!
//! Pure except for the clock and the event ids, which come from a [`DeciderEnv`] so tests can
//! pin them. Every event gets `commandId` and `correlationId` = the command id and empty
//! metadata unless stated otherwise.

use serde_json::{Map, Value};
use unicode_segmentation::UnicodeSegmentation;
use zc_contracts::*;

use crate::command::command_type;
use crate::errors::CommandRejection;
use crate::event::{empty_metadata, EventBase, EventPayload, PlannedEvent};
use crate::invariants::*;
use crate::projector::project_event;
use crate::support::{
    canonical_project_icon, compare_date_time_strings, date_parse, is_imported_agent_session_message_id, is_valid_script_id, keys_equal,
    legacy_linked_pull_request_of, legacy_thread_pull_request_key, normalize_key, project_icon_monogram, KeySource, MAX_SCRIPT_ID_LENGTH,
};

/// The clock and id source of the decider (`DateTime.now`, `Crypto.randomUUIDv4`).
pub trait DeciderEnv: Send + Sync {
    /// `DateTime.formatIso(DateTime.now)`.
    fn now_iso(&self) -> String;
    /// A fresh event id (`crypto.randomUUIDv4`).
    fn new_event_id(&self) -> String;
}

/// The real clock and random v4 UUIDs.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemEnv;

impl DeciderEnv for SystemEnv {
    fn now_iso(&self) -> String {
        zc_core::time::now_iso()
    }
    fn new_event_id(&self) -> String {
        zc_core::ids::uuid_v4()
    }
}

/// `QUEUED_TURN_START_GRACE_MS` (`ThreadSettlementPolicy.ts`).
pub const QUEUED_TURN_START_GRACE_MS: f64 = 2.0 * 60.0 * 1_000.0;

fn js_date(value: &str) -> f64 {
    date_parse(value).map_or(f64::NAN, |millis| millis as f64)
}

/// `Math.max`: NaN wins.
fn js_max(left: f64, right: f64) -> f64 {
    if left.is_nan() || right.is_nan() {
        f64::NAN
    } else {
        left.max(right)
    }
}

/// `threadHasQueuedTurnStart` (`ThreadSettlementPolicy.ts`): a recent user message stays
/// queued until a turn adopts its timestamp. The absolute age bounds clock skew both ways.
pub fn thread_has_queued_turn_start(
    latest_user_message_at: Option<&str>,
    latest_turn: Option<&OrchestrationLatestTurn>,
    session_status: Option<OrchestrationSessionStatus>,
    now: &str,
) -> bool {
    let Some(latest_user_message_at) = latest_user_message_at else { return false };
    if session_status == Some(OrchestrationSessionStatus::Error) {
        return false;
    }
    let message_at = js_date(latest_user_message_at);
    let age = js_date(now) - message_at;
    if age.is_nan() || age.abs() > QUEUED_TURN_START_GRACE_MS {
        return false;
    }
    let Some(latest_turn) = latest_turn else { return true };
    [
        Some(latest_turn.requested_at.as_str()),
        latest_turn.started_at.as_deref(),
        latest_turn.completed_at.as_deref(),
    ]
    .into_iter()
    .all(|value| value.is_none_or(|value| js_date(value) < message_at))
}

/// `hasQueuedTurnStartForThread`: the shell-level rule applied to the detailed model.
fn has_queued_turn_start_for_thread(thread: &OrchestrationThread, now: &str) -> bool {
    let mut latest_user_message_at: Option<&str> = None;
    let mut latest_ms = f64::NEG_INFINITY;
    for message in &thread.messages {
        if message.role != OrchestrationMessageRole::User || is_imported_agent_session_message_id(message.id.as_str()) {
            continue;
        }
        let message_ms = js_date(&message.created_at);
        latest_ms = js_max(latest_ms, message_ms);
        if message_ms == latest_ms {
            latest_user_message_at = Some(&message.created_at);
        }
    }
    thread_has_queued_turn_start(
        if latest_ms.is_finite() { latest_user_message_at } else { None },
        thread.latest_turn.as_ref(),
        thread.session.as_ref().map(|session| session.status),
        now,
    )
}

/// `isStaleRequestFailureDetail`: a respond failure that marks the request stale/unknown
/// clears it, exactly as the SQL pending accounting does.
fn is_stale_request_failure_detail(payload: &Map<String, Value>) -> bool {
    let Some(detail) = payload.get("detail").and_then(Value::as_str) else {
        return false;
    };
    let detail = detail.to_lowercase();
    [
        "stale pending approval request",
        "unknown pending approval request",
        "unknown pending permission request",
        "stale pending user-input request",
        "unknown pending user-input request",
        "unknown pending user input request",
        "unknown pending codex user input request",
    ]
    .iter()
    .any(|needle| detail.contains(needle))
}

/// `openRequests`: approval and user-input requests with no later resolution, in the
/// insertion order of a JS `Map` (re-setting a key keeps its slot).
fn open_requests(thread: &OrchestrationThread) -> Vec<(String, &OrchestrationThreadActivity)> {
    let mut requests: Vec<(String, &OrchestrationThreadActivity)> = Vec::new();
    for activity in &thread.activities {
        let Some(payload) = activity.payload.as_object() else { continue };
        let Some(request_id) = payload.get("requestId").and_then(Value::as_str) else {
            continue;
        };
        match activity.kind.as_str() {
            "approval.requested" | "user-input.requested" => match requests.iter_mut().find(|(id, _)| id == request_id) {
                Some(entry) => entry.1 = activity,
                None => requests.push((request_id.to_owned(), activity)),
            },
            "approval.resolved" | "user-input.resolved" => requests.retain(|(id, _)| id != request_id),
            "provider.approval.respond.failed" | "provider.user-input.respond.failed" if is_stale_request_failure_detail(payload) => {
                requests.retain(|(id, _)| id != request_id)
            }
            _ => {}
        }
    }
    requests
}

fn response_mode_is_message(payload: &Value) -> bool {
    payload.get("responseMode").and_then(Value::as_str) == Some("message")
}

fn thread_aggregate(thread_id: &ThreadId) -> ProjectIdOrThreadId {
    crate::event::aggregate_id(thread_id.as_str())
}

fn project_aggregate(project_id: &ProjectId) -> ProjectIdOrThreadId {
    crate::event::aggregate_id(project_id.as_str())
}

/// The output of one decision: one or more events, in order.
pub type Decision = Result<Vec<PlannedEvent>, CommandRejection>;

struct Decider<'a> {
    env: &'a dyn DeciderEnv,
}

impl Decider<'_> {
    /// `withEventBase`.
    fn base(
        &self,
        aggregate_kind: OrchestrationAggregateKind,
        aggregate_id: ProjectIdOrThreadId,
        occurred_at: &str,
        command_id: &CommandId,
        metadata: Option<OrchestrationEventMetadata>,
    ) -> EventBase {
        EventBase {
            event_id: EventId::new(self.env.new_event_id()),
            aggregate_kind,
            aggregate_id,
            occurred_at: occurred_at.to_owned(),
            command_id: Some(command_id.clone()),
            causation_event_id: None,
            correlation_id: Some(command_id.clone()),
            metadata: metadata.unwrap_or_else(empty_metadata),
        }
    }

    fn thread_event(&self, thread_id: &ThreadId, occurred_at: &str, command_id: &CommandId, payload: impl Into<EventPayload>) -> PlannedEvent {
        self.thread_event_with(thread_id, occurred_at, command_id, None, payload)
    }

    fn thread_event_with(
        &self,
        thread_id: &ThreadId,
        occurred_at: &str,
        command_id: &CommandId,
        metadata: Option<OrchestrationEventMetadata>,
        payload: impl Into<EventPayload>,
    ) -> PlannedEvent {
        PlannedEvent {
            base: self.base(
                OrchestrationAggregateKind::Thread,
                thread_aggregate(thread_id),
                occurred_at,
                command_id,
                metadata,
            ),
            payload: payload.into(),
        }
    }

    fn project_event(&self, project_id: &ProjectId, occurred_at: &str, command_id: &CommandId, payload: impl Into<EventPayload>) -> PlannedEvent {
        PlannedEvent {
            base: self.base(
                OrchestrationAggregateKind::Project,
                project_aggregate(project_id),
                occurred_at,
                command_id,
                None,
            ),
            payload: payload.into(),
        }
    }

    fn now(&self) -> String {
        self.env.now_iso()
    }

    /// `decideCommandSequence`: decides each command against the model the previous ones
    /// produced.
    fn decide_sequence(&self, commands: Vec<OrchestrationCommand>, model: &OrchestrationReadModel) -> Decision {
        let mut next_model = model.clone();
        let mut next_sequence = model.snapshot_sequence;
        let mut planned = Vec::new();
        for command in commands {
            for event in self.decide(&command, &next_model, None)? {
                next_sequence += 1;
                project_event(&mut next_model, &event.clone().into_event(next_sequence));
                planned.push(event);
            }
        }
        Ok(planned)
    }

    fn decide(&self, command: &OrchestrationCommand, model: &OrchestrationReadModel, user_input_activity: Option<&OrchestrationThreadActivity>) -> Decision {
        use OrchestrationCommand as C;
        let kind = command_type(command);
        match command {
            C::ProjectCreateCommand(command) => {
                require_project_absent(model, kind, &command.project_id)?;
                require_active_project_workspace_root_absent(model, kind, &command.workspace_root, Some(&command.project_id))?;
                Ok(vec![self.project_event(
                    &command.project_id,
                    &command.created_at,
                    &command.command_id,
                    ProjectCreatedPayload {
                        project_id: command.project_id.clone(),
                        title: command.title.clone(),
                        workspace_root: command.workspace_root.clone(),
                        repository_identity: None,
                        // Project creation has no user model choice: only a metadata update
                        // records an explicit project default.
                        default_model_selection: None,
                        favicon_path: Some(None),
                        project_icon: Some(None),
                        scripts: Vec::new(),
                        created_at: command.created_at.clone(),
                        updated_at: command.created_at.clone(),
                    },
                )])
            }

            C::ClientOrchestrationCommandProjectMetaUpdate(command) => {
                let project = require_project(model, kind, &command.project_id)?;
                let monogram = command.project_icon.as_ref().and_then(Option::as_ref).and_then(project_icon_monogram);
                if let Some(text) = monogram {
                    if text.graphemes(true).count() > 2 {
                        return Err(CommandRejection::invariant(kind, "Project monograms must contain at most two characters."));
                    }
                }
                if let Some(scripts) = &command.scripts {
                    // Persisted ids predate shortcut validation: they may be edited or removed,
                    // but no new invalid id may enter.
                    for script in scripts {
                        let existing = project.scripts.iter().any(|entry| entry.id == script.id);
                        if !existing && !is_valid_script_id(&script.id) {
                            return Err(CommandRejection::invariant(
                                kind,
                                format!(
                                    "Script ID '{}' must be 1-{MAX_SCRIPT_ID_LENGTH} lowercase letters, digits or hyphens, starting with a letter or digit.",
                                    script.id
                                ),
                            ));
                        }
                    }
                }
                if let Some(workspace_root) = &command.workspace_root {
                    require_active_project_workspace_root_absent(model, kind, workspace_root, Some(&command.project_id))?;
                }
                let occurred_at = self.now();
                Ok(vec![self.project_event(
                    &command.project_id,
                    &occurred_at,
                    &command.command_id,
                    ProjectMetaUpdatedPayload {
                        project_id: command.project_id.clone(),
                        title: command.title.clone(),
                        workspace_root: command.workspace_root.clone(),
                        repository_identity: None,
                        default_model_selection: command.default_model_selection.clone(),
                        default_thread_env_mode: command.default_thread_env_mode,
                        auto_pull: command.auto_pull,
                        favicon_path: command.favicon_path.clone(),
                        // The event store writes the encoded icon (`encodeProjectIcon`).
                        project_icon: command.project_icon.as_ref().map(|icon| icon.as_ref().map(canonical_project_icon)),
                        scripts: command.scripts.clone(),
                        updated_at: occurred_at.clone(),
                    },
                )])
            }

            C::ClientOrchestrationCommandProjectDelete(command) => {
                require_project(model, kind, &command.project_id)?;
                let active_threads: Vec<&OrchestrationThread> = list_threads_by_project_id(model, &command.project_id)
                    .filter(|thread| thread.deleted_at.is_none())
                    .collect();
                if !active_threads.is_empty() && command.force != Some(true) {
                    return Err(CommandRejection::invariant(
                        kind,
                        format!("Project '{}' is not empty and cannot be deleted without force=true.", command.project_id),
                    ));
                }
                if !active_threads.is_empty() {
                    let mut commands: Vec<OrchestrationCommand> = active_threads
                        .iter()
                        .map(|thread| {
                            C::ClientOrchestrationCommandThreadDelete(ClientOrchestrationCommandThreadDelete {
                                r#type: LitThreadDelete,
                                command_id: command.command_id.clone(),
                                thread_id: thread.id.clone(),
                            })
                        })
                        .collect();
                    commands.push(C::ClientOrchestrationCommandProjectDelete(ClientOrchestrationCommandProjectDelete {
                        r#type: LitProjectDelete,
                        command_id: command.command_id.clone(),
                        project_id: command.project_id.clone(),
                        force: None,
                    }));
                    return self.decide_sequence(commands, model);
                }
                let occurred_at = self.now();
                Ok(vec![self.project_event(
                    &command.project_id,
                    &occurred_at,
                    &command.command_id,
                    ProjectDeletedPayload {
                        project_id: command.project_id.clone(),
                        deleted_at: occurred_at.clone(),
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadCreate(command) => {
                require_project(model, kind, &command.project_id)?;
                require_thread_absent(model, kind, &command.thread_id)?;
                let metadata = command.history_import.map(|_| OrchestrationEventMetadata {
                    history_import: Some(true),
                    ..empty_metadata()
                });
                Ok(vec![self.thread_event_with(
                    &command.thread_id,
                    &command.created_at,
                    &command.command_id,
                    metadata,
                    ThreadCreatedPayload {
                        thread_id: command.thread_id.clone(),
                        project_id: command.project_id.clone(),
                        title: command.title.clone(),
                        model_selection: command.model_selection.clone(),
                        runtime_mode: command.runtime_mode,
                        interaction_mode: command.interaction_mode,
                        branch: command.branch.clone(),
                        worktree_path: command.worktree_path.clone(),
                        created_at: command.created_at.clone(),
                        updated_at: command.created_at.clone(),
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadDelete(command) => {
                require_thread(model, kind, &command.thread_id)?;
                let occurred_at = self.now();
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadDeletedPayload {
                        thread_id: command.thread_id.clone(),
                        deleted_at: occurred_at.clone(),
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadArchive(command) => {
                require_thread_not_archived(model, kind, &command.thread_id)?;
                let occurred_at = self.now();
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadArchivedPayload {
                        thread_id: command.thread_id.clone(),
                        archived_at: occurred_at.clone(),
                        updated_at: occurred_at.clone(),
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadUnarchive(command) => {
                require_thread_archived(model, kind, &command.thread_id)?;
                let occurred_at = self.now();
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadUnarchivedPayload {
                        thread_id: command.thread_id.clone(),
                        updated_at: occurred_at.clone(),
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadSettle(command) => self.decide_settle(model, kind, &command.command_id, &command.thread_id, None),
            C::ThreadAutoSettle(command) => self.decide_settle(model, kind, &command.command_id, &command.thread_id, Some(&command.settled_at)),

            C::ClientOrchestrationCommandThreadUnsettle(command) => {
                let thread = require_thread_not_archived(model, kind, &command.thread_id)?;
                // Idempotent by re-emission: a duplicate keeps the existing updatedAt.
                let already_pinned_active = thread.settled_override == Some(OrchestrationThreadSettledOverride::Active);
                let occurred_at = self.now();
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadUnsettledPayload {
                        thread_id: command.thread_id.clone(),
                        reason: ThreadUnsettledPayloadReason::User,
                        updated_at: if already_pinned_active {
                            thread.updated_at.clone()
                        } else {
                            occurred_at.clone()
                        },
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadSnooze(command) => {
                let thread = require_thread_not_archived(model, kind, &command.thread_id)?;
                let occurred_at = self.now();
                // A wake time in the past (or unparseable) would snooze and wake at once.
                if js_date(&command.snoozed_until).partial_cmp(&js_date(&occurred_at)) != Some(std::cmp::Ordering::Greater) {
                    return Err(CommandRejection::invariant(
                        kind,
                        format!("thread {} snooze wake time {} is not in the future", command.thread_id, command.snoozed_until),
                    ));
                }
                // Blocked-on-you work must not be snoozed away.
                if !open_requests(thread).is_empty() {
                    return Err(CommandRejection::invariant(
                        kind,
                        format!(
                            "thread {} has a pending approval or user-input request and cannot be snoozed",
                            command.thread_id
                        ),
                    ));
                }
                if has_queued_turn_start_for_thread(thread, &occurred_at) {
                    return Err(CommandRejection::invariant(
                        kind,
                        format!("thread {} has a queued turn start and cannot be snoozed", command.thread_id),
                    ));
                }
                // Re-snoozing to the same wake time re-emits the original timestamps.
                let existing_snoozed_at = match (&thread.snoozed_until, &thread.snoozed_at) {
                    (Some(Some(until)), Some(Some(at))) if *until == command.snoozed_until => Some(at.clone()),
                    _ => None,
                };
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadSnoozedPayload {
                        thread_id: command.thread_id.clone(),
                        snoozed_until: command.snoozed_until.clone(),
                        snoozed_at: existing_snoozed_at.clone().unwrap_or_else(|| occurred_at.clone()),
                        updated_at: if existing_snoozed_at.is_some() {
                            thread.updated_at.clone()
                        } else {
                            occurred_at.clone()
                        },
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadUnsnooze(command) => {
                let thread = require_thread_not_archived(model, kind, &command.thread_id)?;
                let already_awake = thread.snoozed_until.as_ref().and_then(Option::as_ref).is_none();
                let occurred_at = self.now();
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadUnsnoozedPayload {
                        thread_id: command.thread_id.clone(),
                        reason: ThreadUnsnoozedPayloadReason::User,
                        updated_at: if already_awake { thread.updated_at.clone() } else { occurred_at.clone() },
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadPin(command) => {
                let thread = require_thread_not_archived(model, kind, &command.thread_id)?;
                let occurred_at = self.now();
                // Re-pinning re-emits the original timestamps; a fresh pin takes the client's
                // slot, a re-pin keeps the existing one.
                let existing_pinned_at = thread.pinned_at.clone().flatten();
                let pinned = self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadPinnedPayload {
                        thread_id: command.thread_id.clone(),
                        pinned_at: existing_pinned_at.clone().unwrap_or_else(|| occurred_at.clone()),
                        pin_order_key: if existing_pinned_at.is_none() { command.order_key.clone() } else { None },
                        updated_at: if existing_pinned_at.is_some() {
                            thread.updated_at.clone()
                        } else {
                            occurred_at.clone()
                        },
                    },
                );
                // Pinning is a promotion: it clears settled and snoozed states.
                let mut events = vec![pinned];
                if thread.settled_override == Some(OrchestrationThreadSettledOverride::Settled) {
                    events.push(self.thread_event(
                        &command.thread_id,
                        &occurred_at,
                        &command.command_id,
                        ThreadUnsettledPayload {
                            thread_id: command.thread_id.clone(),
                            reason: ThreadUnsettledPayloadReason::User,
                            updated_at: occurred_at.clone(),
                        },
                    ));
                }
                if thread.snoozed_until.as_ref().and_then(Option::as_ref).is_some() {
                    events.push(self.thread_event(
                        &command.thread_id,
                        &occurred_at,
                        &command.command_id,
                        ThreadUnsnoozedPayload {
                            thread_id: command.thread_id.clone(),
                            reason: ThreadUnsnoozedPayloadReason::User,
                            updated_at: occurred_at.clone(),
                        },
                    ));
                }
                Ok(events)
            }

            C::ClientOrchestrationCommandThreadUnpin(command) => {
                let thread = require_thread_not_archived(model, kind, &command.thread_id)?;
                let already_unpinned = thread.pinned_at.as_ref().and_then(Option::as_ref).is_none();
                let occurred_at = self.now();
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadUnpinnedPayload {
                        thread_id: command.thread_id.clone(),
                        updated_at: if already_unpinned { thread.updated_at.clone() } else { occurred_at.clone() },
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadPinReorder(command) => {
                let thread = require_thread_not_archived(model, kind, &command.thread_id)?;
                if thread.pinned_at.as_ref().and_then(Option::as_ref).is_none() {
                    return Err(CommandRejection::invariant(
                        kind,
                        format!("thread {} is not pinned and cannot be reordered", command.thread_id),
                    ));
                }
                let key_unchanged = thread.pin_order_key.as_ref().and_then(Option::as_ref) == Some(&command.order_key);
                let occurred_at = self.now();
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadPinReorderedPayload {
                        thread_id: command.thread_id.clone(),
                        order_key: command.order_key.clone(),
                        updated_at: if key_unchanged { thread.updated_at.clone() } else { occurred_at.clone() },
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadAutoSettleSet(command) => {
                let thread = require_thread_not_archived(model, kind, &command.thread_id)?;
                let currently_disabled_at = thread.auto_settle_disabled_at.clone().flatten();
                let unchanged = if command.enabled {
                    currently_disabled_at.is_none()
                } else {
                    currently_disabled_at.is_some()
                };
                let occurred_at = self.now();
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadAutoSettleSetPayload {
                        thread_id: command.thread_id.clone(),
                        auto_settle_disabled_at: if command.enabled {
                            None
                        } else {
                            Some(currently_disabled_at.unwrap_or_else(|| occurred_at.clone()))
                        },
                        updated_at: if unchanged { thread.updated_at.clone() } else { occurred_at.clone() },
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadActiveReorder(command) => {
                let thread = require_thread_not_archived(model, kind, &command.thread_id)?;
                let occurred_at = self.now();
                if thread.deleted_at.is_some()
                    || thread.pinned_at.as_ref().and_then(Option::as_ref).is_some()
                    || thread.settled_override == Some(OrchestrationThreadSettledOverride::Settled)
                {
                    return Err(CommandRejection::invariant(
                        kind,
                        format!("thread {} is not active and cannot be reordered", command.thread_id),
                    ));
                }
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadMetaUpdatedPayload {
                        active_order_key: Some(Some(command.order_key.clone())),
                        // Arranging the list is not thread activity.
                        ..meta_updated(&command.thread_id, &thread.updated_at)
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadMetaUpdate(command) => self.decide_meta_update(model, kind, command),

            C::ClientOrchestrationCommandThreadPullRequestLink(command) => {
                let thread = require_thread(model, kind, &command.thread_id)?;
                let key = normalize_key(KeySource {
                    host: &command.host,
                    repository: &command.repository,
                    number: command.number,
                    url: Some(&command.url),
                });
                let existing = find_pull_request_link(thread, &key);
                // An explicit link on a dismissed stack member un-dismisses it; any other
                // duplicate is a no-op the engine would reject as zero-event.
                let undismisses = existing.is_some_and(|existing| existing.source == ThreadPullRequestLinkSource::StackDismissed)
                    && matches!(
                        command.source,
                        ThreadPullRequestLinkSource::Manual | ThreadPullRequestLinkSource::Agent | ThreadPullRequestLinkSource::Created
                    );
                if existing.is_some() && !undismisses {
                    return Err(CommandRejection::invariant(
                        kind,
                        format!(
                            "pull request {}/{}#{} is already linked to thread {}",
                            key.host, key.repository, key.number, command.thread_id
                        ),
                    ));
                }
                let occurred_at = self.now();
                let link = match existing {
                    Some(existing) => ThreadPullRequestLink {
                        url: command.url.clone(),
                        source: command.source,
                        ..existing.clone()
                    },
                    None => ThreadPullRequestLink {
                        host: key.host.clone(),
                        repository: key.repository.clone(),
                        number: key.number,
                        url: command.url.clone(),
                        source: command.source,
                        linked_at: occurred_at.clone(),
                        snapshot: None,
                        stack: None,
                    },
                };
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadPullRequestLinkedPayload {
                        thread_id: command.thread_id.clone(),
                        link,
                        updated_at: occurred_at.clone(),
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadPullRequestUnlink(command) => {
                let thread = require_thread(model, kind, &command.thread_id)?;
                let key = normalize_key(KeySource {
                    host: &command.host,
                    repository: &command.repository,
                    number: command.number,
                    url: None,
                });
                let Some(existing) = find_pull_request_link(thread, &key) else {
                    return Err(CommandRejection::invariant(
                        kind,
                        format!(
                            "pull request {}/{}#{} is not linked to thread {}",
                            key.host, key.repository, key.number, command.thread_id
                        ),
                    ));
                };
                let occurred_at = self.now();
                // Any known native-stack member needs a tombstone, whoever linked it.
                let belongs_to_stack = existing.source == ThreadPullRequestLinkSource::Stack
                    || existing.stack.is_some()
                    || thread.pull_requests.iter().any(|link| {
                        link.host.to_lowercase() == key.host
                            && link.repository.to_lowercase() == key.repository
                            && link
                                .stack
                                .as_ref()
                                .is_some_and(|stack| stack.layers.iter().any(|layer| layer.number == key.number))
                    });
                if belongs_to_stack {
                    return Ok(vec![self.thread_event(
                        &command.thread_id,
                        &occurred_at,
                        &command.command_id,
                        ThreadPullRequestLinkedPayload {
                            thread_id: command.thread_id.clone(),
                            link: ThreadPullRequestLink {
                                source: ThreadPullRequestLinkSource::StackDismissed,
                                ..existing.clone()
                            },
                            updated_at: occurred_at.clone(),
                        },
                    )]);
                }
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadPullRequestUnlinkedPayload {
                        thread_id: command.thread_id.clone(),
                        host: key.host,
                        repository: key.repository,
                        number: key.number,
                        updated_at: occurred_at.clone(),
                    },
                )])
            }

            C::ThreadPullRequestLinkSync(command) | C::ThreadPullRequestLinkSync_(command) => {
                let thread = require_thread(model, kind, &command.thread_id)?;
                let key = normalize_key(KeySource {
                    host: &command.host,
                    repository: &command.repository,
                    number: command.number,
                    url: None,
                });
                if find_pull_request_link(thread, &key).is_none() {
                    return Err(CommandRejection::invariant(
                        kind,
                        format!(
                            "pull request {}/{}#{} is not linked to thread {}",
                            key.host, key.repository, key.number, command.thread_id
                        ),
                    ));
                }
                let occurred_at = self.now();
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadPullRequestSyncedPayload {
                        thread_id: command.thread_id.clone(),
                        host: key.host,
                        repository: key.repository,
                        number: key.number,
                        snapshot: command.snapshot.clone(),
                        stack: command.stack.clone(),
                        updated_at: occurred_at.clone(),
                    },
                )])
            }

            C::ThreadPullRequestSync(command) | C::ThreadPullRequestSync_(command) => {
                let thread = require_thread_not_archived(model, kind, &command.thread_id)?;
                if thread.deleted_at.is_some() {
                    return Err(CommandRejection::invariant(
                        kind,
                        format!("thread {} was deleted before pull request discovery", command.thread_id),
                    ));
                }
                let expected = &command.expected;
                if thread.project_id != command.project_id
                    || thread.branch != expected.branch
                    || thread.worktree_path != expected.worktree_path
                    || thread.linked_pull_request.clone().flatten() != expected.linked_pull_request
                    || thread.branch_pull_request.clone().flatten() != expected.branch_pull_request
                {
                    return Err(CommandRejection::invariant(
                        kind,
                        format!("thread {} changed before pull request discovery", command.thread_id),
                    ));
                }
                let project = require_project(model, kind, &command.project_id)?;
                if project.deleted_at.is_some() || project.workspace_root != expected.workspace_root {
                    return Err(CommandRejection::invariant(
                        kind,
                        format!("project {} changed before pull request discovery", command.project_id),
                    ));
                }
                let occurred_at = self.now();
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadMetaUpdatedPayload {
                        branch_pull_request: Some(command.branch_pull_request.clone()),
                        linked_pull_request: command.linked_pull_request.clone().map(Some),
                        ..meta_updated(&command.thread_id, &thread.updated_at)
                    },
                )])
            }

            C::ThreadTitleGenerateComplete(command) => {
                let thread = require_thread(model, kind, &command.thread_id)?;
                let title_state = thread.title_state.as_ref().and_then(Option::as_ref);
                let current = thread.deleted_at.is_none()
                    && title_state.map(|state| state.source) != Some(ThreadTitleStateSource::Manual)
                    && thread.title == command.expected_title
                    && title_state.map(|state| &state.version) == command.expected_version.as_ref()
                    && thread.title_regeneration.as_ref().and_then(Option::as_ref).is_none();
                let occurred_at = self.now();
                let mut payload = meta_updated(&command.thread_id, &thread.updated_at);
                if current {
                    payload.title = Some(command.title.clone());
                    payload.title_state = Some(Some(ThreadTitleState {
                        source: ThreadTitleStateSource::Generated,
                        version: command.command_id.clone(),
                        needs_refinement: command.needs_refinement,
                    }));
                }
                Ok(vec![self.thread_event(&command.thread_id, &occurred_at, &command.command_id, payload)])
            }

            C::ThreadTitleRefine(command) => {
                let thread = require_thread(model, kind, &command.thread_id)?;
                let title_state = thread.title_state.as_ref().and_then(Option::as_ref);
                let current = thread.deleted_at.is_none()
                    && thread.latest_turn.as_ref().map(|turn| turn.state) == Some(OrchestrationLatestTurnState::Completed)
                    && thread.session.as_ref().map(|session| session.status) == Some(OrchestrationSessionStatus::Ready)
                    && title_state.is_some_and(|state| {
                        state.source == ThreadTitleStateSource::Generated && state.version == command.expected_version && state.needs_refinement
                    })
                    && thread.title_regeneration.as_ref().and_then(Option::as_ref).is_none();
                let occurred_at = self.now();
                let mut payload = meta_updated(&command.thread_id, &thread.updated_at);
                if current {
                    payload.title_state = Some(Some(ThreadTitleState {
                        source: ThreadTitleStateSource::Generated,
                        version: command.command_id.clone(),
                        needs_refinement: false,
                    }));
                    payload.regenerate_title = Some(LitTrue);
                    payload.previous_title = Some(thread.title.clone());
                    payload.title_regeneration = Some(Some(ThreadTitleRegeneration {
                        request_id: command.command_id.clone(),
                        started_at: occurred_at.clone(),
                    }));
                }
                Ok(vec![self.thread_event(&command.thread_id, &occurred_at, &command.command_id, payload)])
            }

            C::ThreadTitleRegenerationComplete(command) => {
                let thread = require_thread(model, kind, &command.thread_id)?;
                let request_is_current = thread
                    .title_regeneration
                    .as_ref()
                    .and_then(Option::as_ref)
                    .is_some_and(|regeneration| regeneration.request_id == command.request_id);
                let occurred_at = self.now();
                let mut payload = meta_updated(&command.thread_id, if request_is_current { &occurred_at } else { &thread.updated_at });
                if request_is_current {
                    payload.title = command.title.clone();
                    payload.title_regeneration = Some(None);
                }
                Ok(vec![self.thread_event(&command.thread_id, &occurred_at, &command.command_id, payload)])
            }

            C::ClientOrchestrationCommandThreadRuntimeModeSet(command) => {
                require_thread(model, kind, &command.thread_id)?;
                let occurred_at = self.now();
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadRuntimeModeSetPayload {
                        thread_id: command.thread_id.clone(),
                        runtime_mode: command.runtime_mode,
                        updated_at: occurred_at.clone(),
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadInteractionModeSet(command) => {
                require_thread(model, kind, &command.thread_id)?;
                let occurred_at = self.now();
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &occurred_at,
                    &command.command_id,
                    ThreadInteractionModeSetPayload {
                        thread_id: command.thread_id.clone(),
                        interaction_mode: command.interaction_mode,
                        updated_at: occurred_at.clone(),
                    },
                )])
            }

            C::ThreadTurnStartCommand(command) => self.decide_turn_start(model, kind, command),

            C::ThreadMessageUserAppend(command) => {
                reject_imported_message_id(kind, &command.message.message_id)?;
                let thread = require_thread(model, kind, &command.thread_id)?;
                if thread.messages.iter().any(|message| message.id == command.message.message_id) {
                    return Err(CommandRejection::invariant(
                        kind,
                        format!("Message '{}' already exists on thread '{}'.", command.message.message_id, command.thread_id),
                    ));
                }
                Ok(vec![self.thread_event_with(
                    &command.thread_id,
                    &command.created_at,
                    &command.command_id,
                    Some(OrchestrationEventMetadata {
                        deferred_turn: Some(true),
                        ..empty_metadata()
                    }),
                    ThreadMessageSentPayload {
                        thread_id: command.thread_id.clone(),
                        message_id: command.message.message_id.clone(),
                        role: OrchestrationMessageRole::User,
                        text: command.message.text.clone(),
                        attachments: Some(command.message.attachments.clone()),
                        context: command.message.context.clone(),
                        turn_id: None,
                        streaming: false,
                        created_at: command.created_at.clone(),
                        updated_at: command.created_at.clone(),
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadTurnInterrupt(command) => {
                require_thread(model, kind, &command.thread_id)?;
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &command.created_at,
                    &command.command_id,
                    ThreadTurnInterruptRequestedPayload {
                        thread_id: command.thread_id.clone(),
                        turn_id: command.turn_id.clone(),
                        created_at: command.created_at.clone(),
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadApprovalRespond(command) => {
                require_thread(model, kind, &command.thread_id)?;
                Ok(vec![self.thread_event_with(
                    &command.thread_id,
                    &command.created_at,
                    &command.command_id,
                    Some(OrchestrationEventMetadata {
                        request_id: Some(command.request_id.clone()),
                        ..empty_metadata()
                    }),
                    ThreadApprovalResponseRequestedPayload {
                        thread_id: command.thread_id.clone(),
                        request_id: command.request_id.clone(),
                        decision: command.decision,
                        created_at: command.created_at.clone(),
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadUserInputRespond(command) => self.decide_user_input_respond(model, kind, command, user_input_activity),

            C::ClientOrchestrationCommandThreadUserInputDismiss(command) => {
                require_thread(model, kind, &command.thread_id)?;
                let request = match user_input_activity {
                    Some(request) if request.kind == "user-input.requested" => request,
                    _ => return Err(CommandRejection::invariant(kind, "This question has already been answered.")),
                };
                // Only async questions can be dropped silently: a native callback question
                // keeps the provider blocked until it gets a reply.
                if !response_mode_is_message(&request.payload) {
                    return Err(CommandRejection::invariant(kind, "This question needs an answer. Answer it or stop the turn."));
                }
                let mut payload = Map::new();
                payload.insert("requestId".into(), Value::String(command.request_id.0.clone()));
                payload.insert("responseMode".into(), Value::String("message".into()));
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &command.created_at,
                    &command.command_id,
                    ThreadActivityAppendedPayload {
                        thread_id: command.thread_id.clone(),
                        activity: OrchestrationThreadActivity {
                            id: EventId::new(format!("async-dismiss:{}", command.request_id)),
                            tone: OrchestrationThreadActivityTone::Info,
                            kind: "user-input.resolved".into(),
                            summary: "User input dismissed".into(),
                            payload: Value::Object(payload),
                            turn_id: request.turn_id.clone(),
                            sequence: None,
                            created_at: command.created_at.clone(),
                        },
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadCheckpointRevert(command) => {
                require_thread(model, kind, &command.thread_id)?;
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &command.created_at,
                    &command.command_id,
                    ThreadCheckpointRevertRequestedPayload {
                        thread_id: command.thread_id.clone(),
                        turn_count: command.turn_count,
                        restore_files: None,
                        created_at: command.created_at.clone(),
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadConversationRevert(command) => {
                require_thread(model, kind, &command.thread_id)?;
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &command.created_at,
                    &command.command_id,
                    ThreadCheckpointRevertRequestedPayload {
                        thread_id: command.thread_id.clone(),
                        turn_count: command.turn_count,
                        restore_files: Some(false),
                        created_at: command.created_at.clone(),
                    },
                )])
            }

            C::ClientOrchestrationCommandThreadSessionStop(command) => {
                let thread = require_thread(model, kind, &command.thread_id)?;
                // Settle-cleanup stops are conditional: another client may have re-engaged the
                // thread between the settle and this command.
                if command.only_if_settled == Some(true) {
                    let session_coming_alive = thread
                        .session
                        .as_ref()
                        .is_some_and(|session| matches!(session.status, OrchestrationSessionStatus::Starting | OrchestrationSessionStatus::Running));
                    if thread.settled_override != Some(OrchestrationThreadSettledOverride::Settled)
                        || session_coming_alive
                        || has_queued_turn_start_for_thread(thread, &command.created_at)
                    {
                        return Err(CommandRejection::invariant(
                            kind,
                            format!("thread {} was re-engaged after settle; skipping session stop", command.thread_id),
                        ));
                    }
                }
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &command.created_at,
                    &command.command_id,
                    ThreadSessionStopRequestedPayload {
                        thread_id: command.thread_id.clone(),
                        created_at: command.created_at.clone(),
                    },
                )])
            }

            C::ThreadSessionSet(command) => {
                let thread = require_thread(model, kind, &command.thread_id)?;
                let session_set = self.thread_event_with(
                    &command.thread_id,
                    &command.created_at,
                    &command.command_id,
                    Some(empty_metadata()),
                    ThreadSessionSetPayload {
                        thread_id: command.thread_id.clone(),
                        session: command.session.clone(),
                    },
                );
                // Only a session coming alive wakes a settled thread; snooze is never cleared
                // here (a snoozed thread's agent keeps working).
                let is_session_activity = matches!(
                    command.session.status,
                    OrchestrationSessionStatus::Starting | OrchestrationSessionStatus::Running
                );
                if thread.settled_override.is_none() || !is_session_activity {
                    return Ok(vec![session_set]);
                }
                let unsettled = self.thread_event(
                    &command.thread_id,
                    &command.created_at,
                    &command.command_id,
                    ThreadUnsettledPayload {
                        thread_id: command.thread_id.clone(),
                        reason: ThreadUnsettledPayloadReason::Activity,
                        updated_at: command.created_at.clone(),
                    },
                );
                Ok(vec![unsettled, session_set])
            }

            C::ThreadMessageAssistantDelta(command) => self.decide_message(
                model,
                kind,
                &command.command_id,
                &command.thread_id,
                &command.message_id,
                OrchestrationMessageRole::Assistant,
                &command.delta,
                command.turn_id.as_ref(),
                true,
                &command.created_at,
            ),
            C::ThreadMessageReasoningDelta(command) => self.decide_message(
                model,
                kind,
                &command.command_id,
                &command.thread_id,
                &command.message_id,
                OrchestrationMessageRole::Reasoning,
                &command.delta,
                command.turn_id.as_ref(),
                true,
                &command.created_at,
            ),
            C::ThreadMessageAssistantComplete(command) => self.decide_message(
                model,
                kind,
                &command.command_id,
                &command.thread_id,
                &command.message_id,
                OrchestrationMessageRole::Assistant,
                "",
                command.turn_id.as_ref(),
                false,
                &command.created_at,
            ),
            C::ThreadMessageReasoningComplete(command) => self.decide_message(
                model,
                kind,
                &command.command_id,
                &command.thread_id,
                &command.message_id,
                OrchestrationMessageRole::Reasoning,
                "",
                command.turn_id.as_ref(),
                false,
                &command.created_at,
            ),

            C::ThreadHistoryImport(command) => {
                let thread = require_thread(model, kind, &command.thread_id)?;
                if thread.deleted_at.is_some()
                    || thread.archived_at.is_some()
                    || !thread.messages.is_empty()
                    || thread.latest_turn.is_some()
                    || thread.session.is_some()
                    || !open_requests(thread).is_empty()
                {
                    return Err(CommandRejection::invariant(
                        kind,
                        format!("Thread '{}' must be active and empty before history can be imported.", command.thread_id),
                    ));
                }
                let Some(first) = command.messages.first() else {
                    return Err(CommandRejection::invariant(kind, "Thread history imports require at least one message."));
                };
                let history_metadata = || OrchestrationEventMetadata {
                    history_import: Some(true),
                    ..empty_metadata()
                };
                let mut events = Vec::new();
                for message in &command.messages {
                    events.push(self.thread_event_with(
                        &command.thread_id,
                        &message.created_at,
                        &command.command_id,
                        Some(history_metadata()),
                        ThreadMessageSentPayload {
                            thread_id: command.thread_id.clone(),
                            message_id: message.message_id.clone(),
                            role: match message.role {
                                OrchestrationCommandThreadHistoryImportMessagesItemRole::User => OrchestrationMessageRole::User,
                                OrchestrationCommandThreadHistoryImportMessagesItemRole::Assistant => OrchestrationMessageRole::Assistant,
                            },
                            text: message.text.clone(),
                            attachments: None,
                            context: None,
                            turn_id: None,
                            streaming: false,
                            created_at: message.created_at.clone(),
                            updated_at: message.created_at.clone(),
                        },
                    ));
                }
                let mut settled_at = first.created_at.clone();
                for message in &command.messages {
                    if compare_date_time_strings(&message.created_at, &settled_at).is_gt() {
                        settled_at = message.created_at.clone();
                    }
                }
                events.push(self.thread_event_with(
                    &command.thread_id,
                    &settled_at,
                    &command.command_id,
                    Some(history_metadata()),
                    ThreadSettledPayload {
                        thread_id: command.thread_id.clone(),
                        settled_at: settled_at.clone(),
                        updated_at: settled_at.clone(),
                    },
                ));
                Ok(events)
            }

            C::ThreadProposedPlanUpsert(command) => {
                require_thread(model, kind, &command.thread_id)?;
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &command.created_at,
                    &command.command_id,
                    ThreadProposedPlanUpsertedPayload {
                        thread_id: command.thread_id.clone(),
                        proposed_plan: command.proposed_plan.clone(),
                    },
                )])
            }

            C::ThreadTurnDiffComplete(command) => {
                let thread = require_thread(model, kind, &command.thread_id)?;
                // A placeholder must never replace a checkpoint captured with a real git ref;
                // deciding under the engine's lock closes the race with CheckpointReactor.
                let existing = thread.checkpoints.iter().find(|checkpoint| checkpoint.turn_id == command.turn_id);
                if command.status == OrchestrationCheckpointStatus::Missing
                    && existing.is_some_and(|existing| existing.status != OrchestrationCheckpointStatus::Missing)
                {
                    return Err(CommandRejection::invariant(
                        kind,
                        format!("turn {} already has a captured checkpoint", command.turn_id),
                    ));
                }
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &command.created_at,
                    &command.command_id,
                    ThreadTurnDiffCompletedPayload {
                        thread_id: command.thread_id.clone(),
                        turn_id: command.turn_id.clone(),
                        checkpoint_turn_count: command.checkpoint_turn_count,
                        checkpoint_ref: command.checkpoint_ref.clone(),
                        status: command.status,
                        files: command.files.clone(),
                        assistant_message_id: command.assistant_message_id.clone(),
                        completed_at: command.completed_at.clone(),
                    },
                )])
            }

            C::ThreadRevertComplete(command) => {
                require_thread(model, kind, &command.thread_id)?;
                Ok(vec![self.thread_event(
                    &command.thread_id,
                    &command.created_at,
                    &command.command_id,
                    ThreadRevertedPayload {
                        thread_id: command.thread_id.clone(),
                        turn_count: command.turn_count,
                    },
                )])
            }

            C::ThreadActivityAppend(command) => {
                let thread = require_thread(model, kind, &command.thread_id)?;
                let request_id = command
                    .activity
                    .payload
                    .as_object()
                    .and_then(|payload| payload.get("requestId"))
                    .and_then(Value::as_str)
                    .map(ApprovalRequestId::new);
                let metadata = request_id.map(|request_id| OrchestrationEventMetadata {
                    request_id: Some(request_id),
                    ..empty_metadata()
                });
                let appended = self.thread_event_with(
                    &command.thread_id,
                    &command.created_at,
                    &command.command_id,
                    metadata,
                    ThreadActivityAppendedPayload {
                        thread_id: command.thread_id.clone(),
                        activity: command.activity.clone(),
                    },
                );
                // An approval or user-input request must never stay hidden inside a settled row.
                let wakes_settled_thread = matches!(command.activity.kind.as_str(), "approval.requested" | "user-input.requested");
                if thread.settled_override.is_none() || !wakes_settled_thread {
                    return Ok(vec![appended]);
                }
                let unsettled = self.thread_event(
                    &command.thread_id,
                    &command.created_at,
                    &command.command_id,
                    ThreadUnsettledPayload {
                        thread_id: command.thread_id.clone(),
                        reason: ThreadUnsettledPayloadReason::Activity,
                        updated_at: command.created_at.clone(),
                    },
                );
                Ok(vec![unsettled, appended])
            }
        }
    }

    /// `thread.settle` / `thread.auto-settle`. `auto_settled_at` is set for the automatic one.
    fn decide_settle(
        &self,
        model: &OrchestrationReadModel,
        kind: &str,
        command_id: &CommandId,
        thread_id: &ThreadId,
        auto_settled_at: Option<&String>,
    ) -> Decision {
        let thread = require_thread_not_archived(model, kind, thread_id)?;
        let is_auto = auto_settled_at.is_some();
        if is_auto && (thread.settled_override.is_some() || thread.auto_settle_disabled_at.as_ref().and_then(Option::as_ref).is_some()) {
            return Err(CommandRejection::invariant(
                kind,
                format!("thread {thread_id} changed before automatic settlement"),
            ));
        }
        // The server owns settle eligibility: a session coming alive or working blocks it.
        if thread
            .session
            .as_ref()
            .is_some_and(|session| matches!(session.status, OrchestrationSessionStatus::Starting | OrchestrationSessionStatus::Running))
        {
            return Err(CommandRejection::SettleBlocked { thread_id: thread_id.clone() });
        }
        let pending = open_requests(thread);
        // Manual settlement dismisses async questions; native callbacks and approvals still
        // need a response or an interruption.
        if pending
            .iter()
            .any(|(_, activity)| is_auto || activity.kind != "user-input.requested" || !response_mode_is_message(&activity.payload))
        {
            return Err(CommandRejection::SettleBlocked { thread_id: thread_id.clone() });
        }
        let occurred_at = self.now();
        // Settling inside the adoption window would hide just-requested work.
        if has_queued_turn_start_for_thread(thread, &occurred_at) {
            return Err(CommandRejection::SettleBlocked { thread_id: thread_id.clone() });
        }
        // Settling an already-settled thread re-emits the original settledAt and updatedAt,
        // so bulk-settle and double clicks stay silent no-ops.
        let already_settled = thread.settled_override == Some(OrchestrationThreadSettledOverride::Settled) && thread.settled_at.is_some();
        let settled = self.thread_event(
            thread_id,
            &occurred_at,
            command_id,
            ThreadSettledPayload {
                thread_id: thread_id.clone(),
                settled_at: if already_settled {
                    thread.settled_at.clone().unwrap_or_default()
                } else {
                    auto_settled_at.cloned().unwrap_or_else(|| occurred_at.clone())
                },
                updated_at: if already_settled { thread.updated_at.clone() } else { occurred_at.clone() },
            },
        );
        let mut events = vec![settled];
        for (request_id, request) in &pending {
            let mut payload = Map::new();
            payload.insert("requestId".into(), Value::String(request_id.clone()));
            payload.insert("responseMode".into(), Value::String("message".into()));
            events.push(self.thread_event(
                thread_id,
                &occurred_at,
                command_id,
                ThreadActivityAppendedPayload {
                    thread_id: thread_id.clone(),
                    activity: OrchestrationThreadActivity {
                        id: EventId::new(format!("settle:{command_id}:{request_id}")),
                        tone: OrchestrationThreadActivityTone::Info,
                        kind: "user-input.resolved".into(),
                        summary: "User input dismissed".into(),
                        payload: Value::Object(payload),
                        turn_id: request.turn_id.clone(),
                        sequence: None,
                        created_at: occurred_at.clone(),
                    },
                },
            ));
        }
        if thread.pinned_at.as_ref().and_then(Option::as_ref).is_some() {
            events.push(self.thread_event(
                thread_id,
                &occurred_at,
                command_id,
                ThreadUnpinnedPayload {
                    thread_id: thread_id.clone(),
                    updated_at: occurred_at.clone(),
                },
            ));
        }
        if thread.snoozed_until.as_ref().and_then(Option::as_ref).is_some() {
            events.push(self.thread_event(
                thread_id,
                &occurred_at,
                command_id,
                ThreadUnsnoozedPayload {
                    thread_id: thread_id.clone(),
                    reason: ThreadUnsnoozedPayloadReason::User,
                    updated_at: occurred_at.clone(),
                },
            ));
        }
        Ok(events)
    }

    fn decide_meta_update(&self, model: &OrchestrationReadModel, kind: &str, command: &ClientOrchestrationCommandThreadMetaUpdate) -> Decision {
        use OrchestrationCommand as C;
        let thread = require_thread(model, kind, &command.thread_id)?;
        let project = find_project(model, &thread.project_id);
        let identity = project.and_then(|project| project.repository_identity.as_ref()).and_then(Option::as_ref);
        // Old clients only see the derived single link: unlink it through the same command path
        // as modern clients, keeping the links they cannot see.
        let legacy = legacy_linked_pull_request_of(&thread.pull_requests, &thread.project_id, identity);
        let current_pull_request = legacy
            .as_ref()
            .and_then(|legacy| thread.pull_requests.iter().find(|link| link.url == legacy.url && link.number == legacy.number));
        let metadata_only = ClientOrchestrationCommandThreadMetaUpdate {
            linked_pull_request: None,
            ..command.clone()
        };
        let has_metadata = command.title.is_some()
            || command.regenerate_title.is_some()
            || command.model_selection.is_some()
            || command.branch.is_some()
            || command.expected_branch.is_some()
            || command.worktree_path.is_some();
        let unlink_current = |link: &ThreadPullRequestLink| {
            C::ClientOrchestrationCommandThreadPullRequestUnlink(ClientOrchestrationCommandThreadPullRequestUnlink {
                r#type: LitThreadPullRequestUnlink,
                command_id: command.command_id.clone(),
                thread_id: command.thread_id.clone(),
                host: link.host.clone(),
                repository: link.repository.clone(),
                number: link.number,
            })
        };

        if let Some(Some(linked)) = &command.linked_pull_request {
            // Historical clients can send links without a parseable URL.
            let host = match url::Url::parse(&linked.url) {
                Ok(url) => url.host_str().unwrap_or("").to_owned(),
                Err(_) => identity
                    .map(|identity| identity.canonical_key.split('/').next().unwrap_or("").to_owned())
                    .unwrap_or_else(|| "unknown".to_owned()),
            };
            let key = legacy_thread_pull_request_key(linked, Some(&host));
            let mut commands = Vec::new();
            if has_metadata {
                commands.push(C::ClientOrchestrationCommandThreadMetaUpdate(metadata_only));
            }
            if let Some(current) = current_pull_request.filter(|link| link.source == ThreadPullRequestLinkSource::Manual) {
                commands.push(unlink_current(current));
            }
            commands.push(C::ClientOrchestrationCommandThreadPullRequestLink(
                ClientOrchestrationCommandThreadPullRequestLink {
                    r#type: LitThreadPullRequestLink,
                    command_id: command.command_id.clone(),
                    thread_id: command.thread_id.clone(),
                    host: key.host,
                    repository: key.repository,
                    number: key.number,
                    url: linked.url.clone(),
                    source: ThreadPullRequestLinkSource::Manual,
                },
            ));
            return self.decide_sequence(commands, model);
        }

        if let (Some(None), Some(current)) = (&command.linked_pull_request, current_pull_request) {
            let mut commands = Vec::new();
            if has_metadata {
                commands.push(C::ClientOrchestrationCommandThreadMetaUpdate(metadata_only));
            }
            commands.push(unlink_current(current));
            return self.decide_sequence(commands, model);
        }

        let branch = match (&command.branch, &command.expected_branch) {
            (Some(_), Some(expected)) if thread.branch != *expected => Some(thread.branch.clone()),
            _ => command.branch.clone(),
        };
        let occurred_at = self.now();
        let mut payload = meta_updated(&command.thread_id, &occurred_at);
        if let Some(title) = &command.title {
            payload.title = Some(title.clone());
            payload.title_state = Some(Some(ThreadTitleState {
                source: ThreadTitleStateSource::Manual,
                version: command.command_id.clone(),
                needs_refinement: false,
            }));
        }
        if command.regenerate_title.is_some() {
            payload.title_state = Some(Some(ThreadTitleState {
                source: ThreadTitleStateSource::Generated,
                version: command.command_id.clone(),
                needs_refinement: false,
            }));
            payload.regenerate_title = Some(LitTrue);
            payload.previous_title = Some(thread.title.clone());
            payload.title_regeneration = Some(Some(ThreadTitleRegeneration {
                request_id: command.command_id.clone(),
                started_at: occurred_at.clone(),
            }));
        }
        if command.title.is_some() && thread.title_regeneration.as_ref().and_then(Option::as_ref).is_some() {
            payload.title_regeneration = Some(None);
        }
        payload.model_selection = command.model_selection.clone();
        payload.branch = branch;
        payload.worktree_path = command.worktree_path.clone();
        payload.linked_pull_request = command.linked_pull_request.clone();
        Ok(vec![self.thread_event(&command.thread_id, &occurred_at, &command.command_id, payload)])
    }

    fn decide_turn_start(&self, model: &OrchestrationReadModel, kind: &str, command: &ThreadTurnStartCommand) -> Decision {
        reject_imported_message_id(kind, &command.message.message_id)?;
        let target = require_thread(model, kind, &command.thread_id)?;
        let source_proposed_plan = command.source_proposed_plan.as_ref();
        let source_thread = match source_proposed_plan {
            Some(source) => Some(require_thread(model, kind, &source.thread_id)?),
            None => None,
        };
        if let (Some(source), Some(source_thread)) = (source_proposed_plan, source_thread) {
            if !source_thread.proposed_plans.iter().any(|plan| plan.id == source.plan_id) {
                return Err(CommandRejection::invariant(
                    kind,
                    format!("Proposed plan '{}' does not exist on thread '{}'.", source.plan_id, source.thread_id),
                ));
            }
            if source_thread.project_id != target.project_id {
                return Err(CommandRejection::invariant(
                    kind,
                    format!(
                        "Proposed plan '{}' belongs to thread '{}' in a different project.",
                        source.plan_id, source_thread.id
                    ),
                ));
            }
        }
        // A worktree bootstrap persists the message ahead of the turn with
        // `thread.message.user.append`; the turn then only references it.
        let persisted_user_message = target
            .messages
            .iter()
            .any(|message| message.id == command.message.message_id && message.role == OrchestrationMessageRole::User && message.turn_id.is_none());
        let user_message = (!persisted_user_message).then(|| {
            self.thread_event(
                &command.thread_id,
                &command.created_at,
                &command.command_id,
                ThreadMessageSentPayload {
                    thread_id: command.thread_id.clone(),
                    message_id: command.message.message_id.clone(),
                    role: OrchestrationMessageRole::User,
                    text: command.message.text.clone(),
                    attachments: Some(command.message.attachments.clone()),
                    context: command.message.context.clone(),
                    turn_id: None,
                    streaming: false,
                    created_at: command.created_at.clone(),
                    updated_at: command.created_at.clone(),
                },
            )
        });
        let mut turn_start = self.thread_event(
            &command.thread_id,
            &command.created_at,
            &command.command_id,
            ThreadTurnStartRequestedPayload {
                thread_id: command.thread_id.clone(),
                message_id: command.message.message_id.clone(),
                model_selection: command.model_selection.clone(),
                title_seed: command.title_seed.clone(),
                runtime_mode: target.runtime_mode,
                interaction_mode: target.interaction_mode,
                source_proposed_plan: command.source_proposed_plan.clone(),
                created_at: command.created_at.clone(),
            },
        );
        if let Some(user_message) = &user_message {
            turn_start.base.causation_event_id = Some(user_message.base.event_id.clone());
        }
        // Real activity resets any override and spends a snooze's return ticket.
        let mut events = Vec::new();
        if target.settled_override.is_some() {
            events.push(self.thread_event(
                &command.thread_id,
                &command.created_at,
                &command.command_id,
                ThreadUnsettledPayload {
                    thread_id: command.thread_id.clone(),
                    reason: ThreadUnsettledPayloadReason::Activity,
                    updated_at: command.created_at.clone(),
                },
            ));
        }
        if target.snoozed_until.as_ref().and_then(Option::as_ref).is_some() {
            events.push(self.thread_event(
                &command.thread_id,
                &command.created_at,
                &command.command_id,
                ThreadUnsnoozedPayload {
                    thread_id: command.thread_id.clone(),
                    reason: ThreadUnsnoozedPayloadReason::Activity,
                    updated_at: command.created_at.clone(),
                },
            ));
        }
        events.extend(user_message);
        events.push(turn_start);
        Ok(events)
    }

    fn decide_user_input_respond(
        &self,
        model: &OrchestrationReadModel,
        kind: &str,
        command: &ClientOrchestrationCommandThreadUserInputRespond,
        request: Option<&OrchestrationThreadActivity>,
    ) -> Decision {
        use OrchestrationCommand as C;
        let thread = require_thread(model, kind, &command.thread_id)?;
        let attachments: Vec<&ChatImageAttachmentOrChatFileAttachment> = command
            .attachments_by_question_id
            .iter()
            .flat_map(|by_question| by_question.values().flatten())
            .collect();
        let decode_requested =
            |request: &OrchestrationThreadActivity| -> Option<UserInputRequestedPayload> { serde_json::from_value(request.payload.clone()).ok() };
        let mut question_text_by_id = Map::new();
        if !attachments.is_empty() {
            let payload = request.filter(|request| request.kind == "user-input.requested").and_then(decode_requested);
            let Some(payload) = payload else {
                return Err(CommandRejection::invariant(
                    kind,
                    if request.is_some_and(|request| request.kind == "user-input.resolved") {
                        "This question has already been answered."
                    } else {
                        "This question is no longer pending."
                    },
                ));
            };
            for question in &payload.questions {
                question_text_by_id.insert(question.id.clone(), Value::String(question.question.clone()));
            }
            for question_id in command.attachments_by_question_id.iter().flat_map(|by_question| by_question.keys()) {
                let question = payload.questions.iter().find(|question| &question.id == question_id);
                if question.is_none_or(|question| question.allow_custom_answer == Some(false)) {
                    return Err(CommandRejection::invariant(kind, "This question does not accept file references."));
                }
            }
        }

        if let Some(request) = request.filter(|request| response_mode_is_message(&request.payload)) {
            let payload = if request.kind == "user-input.requested" {
                decode_requested(request)
            } else {
                None
            };
            let Some(payload) = payload else {
                return Err(CommandRejection::invariant(kind, "This question has already been answered."));
            };
            let mut replies = Vec::new();
            for question in &payload.questions {
                let question_attachments = command
                    .attachments_by_question_id
                    .as_ref()
                    .and_then(|by_question| by_question.get(&question.id));
                let answer = command.answers.get(&question.id).and_then(Value::as_str);
                let Some(answer) = answer
                    .filter(|answer| !crate::support::js_trim(answer).is_empty() || question_attachments.is_some_and(|attachments| !attachments.is_empty()))
                else {
                    return Err(CommandRejection::invariant(kind, "Answer each question before sending."));
                };
                let labels = question_attachments
                    .map(|attachments| {
                        attachments
                            .iter()
                            .map(|attachment| {
                                let (name, id) = attachment_name_and_id(attachment);
                                format!("Attached file: {name} ({id})")
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                let reply = format!("{}\n{}", question.question, crate::support::js_trim(answer));
                replies.push(if labels.is_empty() { reply } else { format!("{reply}\n{labels}") });
            }
            let mut activity_payload = Map::new();
            activity_payload.insert("requestId".into(), Value::String(command.request_id.0.clone()));
            activity_payload.insert("responseMode".into(), Value::String("message".into()));
            activity_payload.insert("answers".into(), serde_json::to_value(&command.answers).unwrap_or(Value::Null));
            if let Some(by_question) = &command.attachments_by_question_id {
                activity_payload.insert("attachmentsByQuestionId".into(), serde_json::to_value(by_question).unwrap_or(Value::Null));
            }
            // The answer and its message commit together; the normal turn path steers a
            // running agent or resumes an idle session.
            return self.decide_sequence(
                vec![
                    C::ThreadActivityAppend(OrchestrationCommandThreadActivityAppend {
                        r#type: LitThreadActivityAppend,
                        command_id: command.command_id.clone(),
                        thread_id: command.thread_id.clone(),
                        activity: OrchestrationThreadActivity {
                            id: EventId::new(format!("async-answer:{}", command.request_id)),
                            tone: OrchestrationThreadActivityTone::Info,
                            kind: "user-input.resolved".into(),
                            summary: "User input submitted".into(),
                            payload: Value::Object(activity_payload),
                            turn_id: request.turn_id.clone(),
                            sequence: None,
                            created_at: command.created_at.clone(),
                        },
                        created_at: command.created_at.clone(),
                    }),
                    C::ThreadTurnStartCommand(ThreadTurnStartCommand {
                        r#type: LitThreadTurnStart,
                        command_id: command.command_id.clone(),
                        thread_id: command.thread_id.clone(),
                        message: ThreadTurnStartCommandMessage {
                            message_id: MessageId::new(format!("async-answer:{}", command.request_id)),
                            role: LitUser,
                            text: replies.join("\n\n"),
                            attachments: attachments.iter().map(|attachment| to_chat_attachment(attachment)).collect(),
                            context: None,
                        },
                        model_selection: None,
                        title_seed: None,
                        runtime_mode: thread.runtime_mode,
                        interaction_mode: thread.interaction_mode,
                        bootstrap: None,
                        source_proposed_plan: None,
                        created_at: command.created_at.clone(),
                    }),
                ],
                model,
            );
        }

        let response = self.thread_event_with(
            &command.thread_id,
            &command.created_at,
            &command.command_id,
            Some(OrchestrationEventMetadata {
                request_id: Some(command.request_id.clone()),
                ..empty_metadata()
            }),
            OrchestrationEventThreadUserInputResponseRequestedPayload {
                thread_id: command.thread_id.clone(),
                request_id: command.request_id.clone(),
                answers: command.answers.clone(),
                attachments_by_question_id: command.attachments_by_question_id.clone(),
                created_at: command.created_at.clone(),
            },
        );
        if attachments.is_empty() {
            return Ok(vec![response]);
        }
        let mut history_payload = Map::new();
        history_payload.insert("requestId".into(), Value::String(command.request_id.0.clone()));
        history_payload.insert("answers".into(), serde_json::to_value(&command.answers).unwrap_or(Value::Null));
        history_payload.insert("questionTextById".into(), Value::Object(question_text_by_id));
        history_payload.insert(
            "attachmentsByQuestionId".into(),
            serde_json::to_value(&command.attachments_by_question_id).unwrap_or(Value::Null),
        );
        history_payload.insert(
            "detail".into(),
            Value::String(
                attachments
                    .iter()
                    .map(|attachment| attachment_name_and_id(attachment).0.to_owned())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        );
        let mut events = self.decide(
            &C::ThreadActivityAppend(OrchestrationCommandThreadActivityAppend {
                r#type: LitThreadActivityAppend,
                command_id: command.command_id.clone(),
                thread_id: command.thread_id.clone(),
                activity: OrchestrationThreadActivity {
                    id: EventId::new(format!("question-answer:{}", command.command_id)),
                    tone: OrchestrationThreadActivityTone::Info,
                    kind: "user-input.answer-submitted".into(),
                    summary: "Question answer submitted".into(),
                    payload: Value::Object(history_payload),
                    turn_id: request.and_then(|request| request.turn_id.clone()),
                    sequence: None,
                    created_at: command.created_at.clone(),
                },
                created_at: command.created_at.clone(),
            }),
            model,
            None,
        )?;
        events.push(response);
        Ok(events)
    }

    /// `thread.message.{assistant,reasoning}.{delta,complete}`.
    #[allow(clippy::too_many_arguments)]
    fn decide_message(
        &self,
        model: &OrchestrationReadModel,
        kind: &str,
        command_id: &CommandId,
        thread_id: &ThreadId,
        message_id: &MessageId,
        role: OrchestrationMessageRole,
        text: &str,
        turn_id: Option<&TurnId>,
        streaming: bool,
        created_at: &str,
    ) -> Decision {
        reject_imported_message_id(kind, message_id)?;
        require_thread(model, kind, thread_id)?;
        Ok(vec![self.thread_event(
            thread_id,
            created_at,
            command_id,
            ThreadMessageSentPayload {
                thread_id: thread_id.clone(),
                message_id: message_id.clone(),
                role,
                text: text.to_owned(),
                attachments: None,
                context: None,
                turn_id: turn_id.map(|turn_id| turn_id.0.clone()),
                streaming,
                created_at: created_at.to_owned(),
                updated_at: created_at.to_owned(),
            },
        )])
    }
}

fn reject_imported_message_id(kind: &str, message_id: &MessageId) -> Result<(), CommandRejection> {
    if is_imported_agent_session_message_id(message_id.as_str()) {
        return Err(CommandRejection::invariant(
            kind,
            format!("Message id '{message_id}' uses the reserved imported-session namespace."),
        ));
    }
    Ok(())
}

/// A `thread.meta-updated` payload with only the thread and `updatedAt`.
fn meta_updated(thread_id: &ThreadId, updated_at: &str) -> ThreadMetaUpdatedPayload {
    ThreadMetaUpdatedPayload {
        thread_id: thread_id.clone(),
        active_order_key: None,
        title: None,
        regenerate_title: None,
        previous_title: None,
        title_regeneration: None,
        title_state: None,
        model_selection: None,
        branch: None,
        worktree_path: None,
        linked_pull_request: None,
        branch_pull_request: None,
        updated_at: updated_at.to_owned(),
    }
}

/// `findPullRequestLink`.
fn find_pull_request_link<'a>(thread: &'a OrchestrationThread, key: &zc_db::pr_keys::ThreadPullRequestKey) -> Option<&'a ThreadPullRequestLink> {
    thread
        .pull_requests
        .iter()
        .find(|link| keys_equal(KeySource::of_link(link), KeySource::of_key(key)))
}

fn attachment_name_and_id(attachment: &ChatImageAttachmentOrChatFileAttachment) -> (&str, &str) {
    match attachment {
        ChatImageAttachmentOrChatFileAttachment::ChatImageAttachment(image) => (&image.name, &image.id),
        ChatImageAttachmentOrChatFileAttachment::ChatFileAttachment(file) => (&file.name, &file.id),
    }
}

fn to_chat_attachment(attachment: &ChatImageAttachmentOrChatFileAttachment) -> ChatAttachment {
    match attachment {
        ChatImageAttachmentOrChatFileAttachment::ChatImageAttachment(image) => ChatAttachment::ChatImageAttachment(image.clone()),
        ChatImageAttachmentOrChatFileAttachment::ChatFileAttachment(file) => ChatAttachment::ChatFileAttachment(file.clone()),
    }
}

/// `decideOrchestrationCommand`: the events a command produces against `model`, or why it
/// is rejected. `user_input_activity` is the request's durable activity, which the engine
/// reads from SQL for `thread.user-input.respond` / `dismiss`.
pub fn decide_orchestration_command(
    command: &OrchestrationCommand,
    model: &OrchestrationReadModel,
    user_input_activity: Option<&OrchestrationThreadActivity>,
    env: &dyn DeciderEnv,
) -> Decision {
    Decider { env }.decide(command, model, user_input_activity)
}
