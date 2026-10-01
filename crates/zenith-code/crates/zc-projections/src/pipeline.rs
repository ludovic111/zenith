//! `orchestration/Layers/ProjectionPipeline.ts`: the nine SQL projectors, their cursors in
//! `projection_state`, bootstrap replay and the attachment-cleanup cursor.
//!
//! - [`ProjectionPipeline::project_event_deferred`] runs every projector for one event in one
//!   transaction (a savepoint inside the engine's dispatch transaction), writes the nine cursors
//!   with one multi-row upsert, and returns the [`AttachmentCleanup`] to run after the outer
//!   transaction commits.
//! - [`ProjectionPipeline::bootstrap`] replays the event log per projector from its own cursor,
//!   one transaction per event and projector (the order and granularity of the TS bootstrap),
//!   then runs the pending attachment cleanup behind the `projection.attachment-cleanup` cursor.
//!
//! Everything runs on the writer connection (`&Conn`), which is how the engine's transaction
//! reaches the projectors in Rust.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;
use zc_contracts::OrchestrationEvent;
use zc_db::repos::event_store::{self, EventPager, PersistedEvent};
use zc_db::repos::pending_approvals::{self, ProjectionPendingApproval};
use zc_db::repos::projection_state::{self, ProjectionState};
use zc_db::repos::projects::{self, ProjectionProject};
use zc_db::repos::proposed_plans::{self, ProjectionThreadProposedPlan};
use zc_db::repos::thread_activities::{self, ProjectionThreadActivity};
use zc_db::repos::thread_messages::{self, AppendStreamingMessage, ProjectionThreadMessage};
use zc_db::repos::thread_pull_requests::{self, ProjectionThreadPullRequest};
use zc_db::repos::thread_sessions::{self, ProjectionThreadSession};
use zc_db::repos::threads::{self, ProjectionThread};
use zc_db::repos::turns::{self, PendingTurnStart, ProjectionTurn};
use zc_db::{Conn, Db, DbError};

use crate::attachments::{attachment_relative_path, collect_thread_attachment_relative_paths, run_attachment_side_effects, AttachmentSideEffects};
use crate::event::{has_key, opt_string, opt_value, str_field, ProjectionEvent};
use crate::pull_requests::{legacy_thread_pull_request_key, thread_pull_request_keys_equal};

/// `ORCHESTRATION_PROJECTOR_NAMES`.
pub mod projector_names {
    pub const PROJECTS: &str = "projection.projects";
    pub const THREADS: &str = "projection.threads";
    pub const THREAD_MESSAGES: &str = "projection.thread-messages";
    pub const THREAD_PROPOSED_PLANS: &str = "projection.thread-proposed-plans";
    pub const THREAD_ACTIVITIES: &str = "projection.thread-activities";
    pub const THREAD_SESSIONS: &str = "projection.thread-sessions";
    pub const THREAD_TURNS: &str = "projection.thread-turns";
    pub const CHECKPOINTS: &str = "projection.checkpoints";
    pub const PENDING_APPROVALS: &str = "projection.pending-approvals";
    /// The cursor behind which attachment cleanup is retried at bootstrap.
    pub const ATTACHMENT_CLEANUP: &str = "projection.attachment-cleanup";
}

/// The projectors, in the order they run (threads last: it reads what the others wrote).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Projector {
    Projects,
    ThreadMessages,
    ThreadProposedPlans,
    ThreadActivities,
    ThreadSessions,
    ThreadTurns,
    Checkpoints,
    PendingApprovals,
    Threads,
}

impl Projector {
    pub const ALL: [Projector; 9] = [
        Projector::Projects,
        Projector::ThreadMessages,
        Projector::ThreadProposedPlans,
        Projector::ThreadActivities,
        Projector::ThreadSessions,
        Projector::ThreadTurns,
        Projector::Checkpoints,
        Projector::PendingApprovals,
        Projector::Threads,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Projector::Projects => projector_names::PROJECTS,
            Projector::ThreadMessages => projector_names::THREAD_MESSAGES,
            Projector::ThreadProposedPlans => projector_names::THREAD_PROPOSED_PLANS,
            Projector::ThreadActivities => projector_names::THREAD_ACTIVITIES,
            Projector::ThreadSessions => projector_names::THREAD_SESSIONS,
            Projector::ThreadTurns => projector_names::THREAD_TURNS,
            Projector::Checkpoints => projector_names::CHECKPOINTS,
            Projector::PendingApprovals => projector_names::PENDING_APPROVALS,
            Projector::Threads => projector_names::THREADS,
        }
    }
}

/// `Number.MAX_SAFE_INTEGER`: "read everything" for the event pager.
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// The attachment work one projected event left behind, to run **after** the transaction
/// that projected it commits (`projectEventDeferred`'s returned effect). Running it reads
/// the projections again (later events of the same transaction can add references), so it
/// needs the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentCleanup {
    attachments_dir: PathBuf,
    sequence: i64,
    event_type: String,
    side_effects: AttachmentSideEffects,
}

impl AttachmentCleanup {
    /// Nothing to do (most events): skip the call entirely.
    pub fn is_empty(&self) -> bool {
        self.side_effects.is_empty()
    }

    /// Runs the cleanup on the writer connection, outside any transaction. Failures are
    /// logged and reported as `false`, never raised (`applyAttachmentSideEffects`).
    pub fn run(&self, conn: &Conn) -> bool {
        if self.is_empty() {
            return true;
        }
        apply_attachment_side_effects(conn, &self.attachments_dir, self.sequence, &self.event_type, &self.side_effects)
    }

    /// [`AttachmentCleanup::run`] through the database actor.
    pub async fn run_on(self, db: &Db) -> bool {
        if self.is_empty() {
            return true;
        }
        db.call(move |conn| Ok(self.run(conn))).await.unwrap_or(false)
    }
}

/// `OrchestrationProjectionPipeline`.
#[derive(Debug, Clone)]
pub struct ProjectionPipeline {
    attachments_dir: PathBuf,
}

impl ProjectionPipeline {
    /// `attachments_dir` is `ServerConfig.attachmentsDir` (`<stateDir>/attachments`).
    pub fn new(attachments_dir: impl Into<PathBuf>) -> Self {
        Self {
            attachments_dir: attachments_dir.into(),
        }
    }

    pub fn attachments_dir(&self) -> &Path {
        &self.attachments_dir
    }

    /// `projectEventDeferred`: projects `event` in a transaction (a savepoint when the caller
    /// already has one open, as the engine does), advances the nine cursors together, and
    /// returns the attachment cleanup to run after the caller commits.
    pub fn project_event_deferred(&self, conn: &Conn, event: &OrchestrationEvent) -> Result<AttachmentCleanup, DbError> {
        let event = ProjectionEvent::from_contract(event)?;
        self.project_projection_event_deferred(conn, &event)
    }

    /// [`ProjectionPipeline::project_event_deferred`] for a stored row (decoded with the
    /// contract first).
    pub fn project_persisted_deferred(&self, conn: &Conn, event: &PersistedEvent) -> Result<AttachmentCleanup, DbError> {
        let event = ProjectionEvent::from_persisted(event)?;
        self.project_projection_event_deferred(conn, &event)
    }

    /// The deferred projection of an already decoded event.
    pub fn project_projection_event_deferred(&self, conn: &Conn, event: &ProjectionEvent) -> Result<AttachmentCleanup, DbError> {
        let mut side_effects = AttachmentSideEffects::default();
        conn.transaction(|conn| -> Result<(), DbError> {
            for projector in Projector::ALL {
                apply_projector(conn, projector, event, &mut side_effects)?;
            }
            // Runtime projectors commit together. Bootstrap still advances each cursor separately.
            let cursors: Vec<ProjectionState> = Projector::ALL
                .iter()
                .map(|projector| ProjectionState {
                    projector: projector.name().to_string(),
                    last_applied_sequence: event.sequence,
                    updated_at: event.occurred_at.clone(),
                })
                .collect();
            projection_state::upsert_many(conn, &cursors)
        })
        .map_err(|error| rename_sql(error, "ProjectionPipeline.projectEvent:query"))?;
        Ok(AttachmentCleanup {
            attachments_dir: self.attachments_dir.clone(),
            sequence: event.sequence,
            event_type: event.event_type.clone(),
            side_effects,
        })
    }

    /// `projectEvent`: [`ProjectionPipeline::project_event_deferred`] in its own transaction,
    /// then the cleanup.
    pub fn project_event(&self, conn: &Conn, event: &OrchestrationEvent) -> Result<(), DbError> {
        let cleanup = self.project_event_deferred(conn, event)?;
        cleanup.run(conn);
        Ok(())
    }

    /// `bootstrap`: replays the log into each projector from its own cursor, then retries the
    /// attachment cleanup the cleanup cursor is behind on.
    pub fn bootstrap(&self, conn: &Conn) -> Result<(), DbError> {
        let cleanup_projector = projector_names::ATTACHMENT_CLEANUP;
        let states = projection_state::list_all(conn)?;
        let cursor_of = |name: &str| states.iter().find(|state| state.projector == name).map(|state| state.last_applied_sequence);
        let cleanup_state = states.iter().find(|state| state.projector == cleanup_projector).cloned();
        let mut cleanup_start = cleanup_state.as_ref().map(|state| state.last_applied_sequence).unwrap_or(0);
        for projector in Projector::ALL {
            cleanup_start = cleanup_start.min(cursor_of(projector.name()).unwrap_or(0));
        }
        // Persist this boundary before replay: a reset projector can encounter an old
        // revert, then fail after other projectors have committed past that event.
        projection_state::upsert(
            conn,
            &ProjectionState {
                projector: cleanup_projector.to_string(),
                last_applied_sequence: cleanup_start,
                updated_at: cleanup_state
                    .map(|state| state.updated_at)
                    .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string()),
            },
        )
        .map_err(|error| rename_sql(error, "ProjectionPipeline.bootstrap:query"))?;

        for projector in Projector::ALL {
            self.bootstrap_projector(conn, projector)?;
        }

        // Cleanup has its own cursor so retries never have to replay committed text.
        // All message and activity references are current before any files are removed.
        let mut pending: Vec<(String, ProjectionEvent)> = Vec::new();
        let mut last_event: Option<(i64, String)> = None;
        let mut pager = EventPager::from_sequence(cleanup_start, Some(MAX_SAFE_INTEGER));
        while let Some(page) = pager.next_page(conn)? {
            for row in page {
                last_event = Some((row.sequence, row.occurred_at.clone()));
                if row.event_type == "thread.reverted" || row.event_type == "thread.deleted" {
                    let event = ProjectionEvent::from_persisted(&row)?;
                    let key = format!("{}:{}", event.event_type, event.thread_id().unwrap_or(""));
                    // `Map.set` keeps the first insertion position and takes the latest value.
                    match pending.iter_mut().find(|(existing, _)| *existing == key) {
                        Some(entry) => entry.1 = event,
                        None => pending.push((key, event)),
                    }
                }
            }
        }
        for (_, event) in &pending {
            let thread_id = event.thread_id().unwrap_or("").to_string();
            let mut side_effects = AttachmentSideEffects::default();
            if event.event_type == "thread.deleted" {
                side_effects.delete_thread(&thread_id);
            } else {
                side_effects.prune_thread(&thread_id, HashSet::new());
            }
            let cleaned = apply_attachment_side_effects(conn, &self.attachments_dir, event.sequence, &event.event_type, &side_effects);
            // Leave the cleanup cursor behind this event so the next bootstrap retries it.
            if !cleaned {
                return Ok(());
            }
        }
        if let Some((sequence, occurred_at)) = last_event {
            projection_state::upsert(
                conn,
                &ProjectionState {
                    projector: cleanup_projector.to_string(),
                    last_applied_sequence: sequence,
                    updated_at: occurred_at,
                },
            )
            .map_err(|error| rename_sql(error, "ProjectionPipeline.bootstrap:query"))?;
        }
        tracing::debug!(projectors = Projector::ALL.len(), "orchestration projection pipeline bootstrapped");
        Ok(())
    }

    /// [`ProjectionPipeline::bootstrap`] through the database actor.
    pub async fn bootstrap_on(&self, db: &Db) -> Result<(), DbError> {
        let pipeline = self.clone();
        db.call(move |conn| pipeline.bootstrap(conn)).await
    }

    fn bootstrap_projector(&self, conn: &Conn, projector: Projector) -> Result<(), DbError> {
        let start = projection_state::get_by_projector(conn, projector.name())?
            .map(|state| state.last_applied_sequence)
            .unwrap_or(0);
        let mut pager = EventPager::from_sequence(start, Some(MAX_SAFE_INTEGER));
        while let Some(page) = pager.next_page(conn)? {
            for row in page {
                let event = ProjectionEvent::from_persisted(&row)?;
                // Bootstrap side effects are discarded: the cleanup cursor covers them.
                let mut side_effects = AttachmentSideEffects::default();
                conn.transaction(|conn| -> Result<(), DbError> {
                    apply_projector(conn, projector, &event, &mut side_effects)?;
                    projection_state::upsert(
                        conn,
                        &ProjectionState {
                            projector: projector.name().to_string(),
                            last_applied_sequence: event.sequence,
                            updated_at: event.occurred_at.clone(),
                        },
                    )
                })
                .map_err(|error| rename_sql(error, "ProjectionPipeline.bootstrap:query"))?;
            }
        }
        Ok(())
    }
}

/// Keeps decode errors; renames SQL errors to the pipeline operation, as
/// `catchTag("SqlError", toPersistenceSqlError(...))` does for raw SQL failures. Repository
/// errors already carry their own operation and pass through.
fn rename_sql(error: DbError, _operation: &str) -> DbError {
    error
}

fn apply_projector(conn: &Conn, projector: Projector, event: &ProjectionEvent, side_effects: &mut AttachmentSideEffects) -> Result<(), DbError> {
    match projector {
        Projector::Projects => apply_projects(conn, event),
        Projector::ThreadMessages => apply_thread_messages(conn, event, side_effects),
        Projector::ThreadProposedPlans => apply_thread_proposed_plans(conn, event),
        Projector::ThreadActivities => apply_thread_activities(conn, event, side_effects),
        Projector::ThreadSessions => apply_thread_sessions(conn, event),
        Projector::ThreadTurns => apply_thread_turns(conn, event),
        Projector::Checkpoints => Ok(()),
        Projector::PendingApprovals => apply_pending_approvals(conn, event),
        Projector::Threads => apply_threads(conn, event, side_effects),
    }
}

fn payload_str<'a>(event: &'a ProjectionEvent, key: &str) -> &'a str {
    str_field(&event.payload, key).unwrap_or("")
}

fn payload_thread_id(event: &ProjectionEvent) -> &str {
    payload_str(event, "threadId")
}

// ---------------------------------------------------------------------------------------------
// projects
// ---------------------------------------------------------------------------------------------

fn apply_projects(conn: &Conn, event: &ProjectionEvent) -> Result<(), DbError> {
    let payload = &event.payload;
    match event.event_type.as_str() {
        "project.created" => projects::upsert(
            conn,
            &ProjectionProject {
                project_id: payload_str(event, "projectId").to_string(),
                title: payload_str(event, "title").to_string(),
                workspace_root: payload_str(event, "workspaceRoot").to_string(),
                default_model_selection: opt_value(payload, "defaultModelSelection"),
                default_thread_env_mode: None,
                auto_pull: false,
                favicon_path: opt_string(payload, "faviconPath"),
                project_icon: opt_value(payload, "projectIcon"),
                scripts: payload.get("scripts").cloned().unwrap_or(Value::Array(vec![])),
                created_at: payload_str(event, "createdAt").to_string(),
                updated_at: payload_str(event, "updatedAt").to_string(),
                deleted_at: None,
            },
        ),
        "project.meta-updated" => {
            let Some(mut row) = projects::get_by_id(conn, payload_str(event, "projectId"))? else {
                return Ok(());
            };
            if has_key(payload, "title") {
                row.title = payload_str(event, "title").to_string();
            }
            if has_key(payload, "workspaceRoot") {
                row.workspace_root = payload_str(event, "workspaceRoot").to_string();
            }
            if has_key(payload, "defaultModelSelection") {
                row.default_model_selection = opt_value(payload, "defaultModelSelection");
            }
            if has_key(payload, "defaultThreadEnvMode") {
                row.default_thread_env_mode = opt_string(payload, "defaultThreadEnvMode");
            }
            if has_key(payload, "autoPull") {
                row.auto_pull = payload.get("autoPull").and_then(Value::as_bool).unwrap_or(false);
            }
            if has_key(payload, "faviconPath") {
                row.favicon_path = opt_string(payload, "faviconPath");
            }
            if has_key(payload, "projectIcon") {
                row.project_icon = opt_value(payload, "projectIcon");
            }
            if has_key(payload, "scripts") {
                row.scripts = payload["scripts"].clone();
            }
            row.updated_at = payload_str(event, "updatedAt").to_string();
            projects::upsert(conn, &row)
        }
        "project.deleted" => {
            let Some(mut row) = projects::get_by_id(conn, payload_str(event, "projectId"))? else {
                return Ok(());
            };
            let deleted_at = payload_str(event, "deletedAt").to_string();
            row.deleted_at = Some(deleted_at.clone());
            row.updated_at = deleted_at;
            projects::upsert(conn, &row)
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// threads (runs last)
// ---------------------------------------------------------------------------------------------

fn extract_activity_request_id(payload: &Value) -> Option<String> {
    payload.as_object()?.get("requestId")?.as_str().map(str::to_owned)
}

/// `payload.detail` lower-cased when it is a string.
fn activity_detail_lower(payload: &Value) -> Option<String> {
    payload.as_object()?.get("detail")?.as_str().map(str::to_lowercase)
}

fn is_stale_pending_approval_failure_detail(detail: Option<&str>) -> bool {
    detail.is_some_and(|detail| {
        detail.contains("stale pending approval request")
            || detail.contains("unknown pending approval request")
            || detail.contains("unknown pending permission request")
    })
}

/// A refresh reads each persisted summary source, so skip activities that cannot change it.
fn should_refresh_thread_shell_summary(event: &ProjectionEvent) -> bool {
    if event.event_type != "thread.activity-appended" {
        return true;
    }
    matches!(
        activity_kind(event),
        "approval.requested"
            | "approval.resolved"
            | "provider.approval.respond.failed"
            | "user-input.requested"
            | "user-input.resolved"
            | "provider.user-input.respond.failed"
    )
}

fn activity(event: &ProjectionEvent) -> &Value {
    event.payload.get("activity").unwrap_or(&Value::Null)
}

fn activity_kind(event: &ProjectionEvent) -> &str {
    str_field(activity(event), "kind").unwrap_or("")
}

/// `derivePendingUserInputCountFromActivities`.
pub fn derive_pending_user_input_count(activities: &[ProjectionThreadActivity]) -> i64 {
    let mut ordered: Vec<&ProjectionThreadActivity> = activities.iter().collect();
    ordered.sort_by(|left, right| {
        crate::js::locale_compare(&left.created_at, &right.created_at).then_with(|| crate::js::locale_compare(&left.activity_id, &right.activity_id))
    });
    let mut open: Vec<String> = Vec::new();
    for activity in ordered {
        let Some(request_id) = extract_activity_request_id(&activity.payload) else {
            continue;
        };
        let detail = activity_detail_lower(&activity.payload);
        match activity.kind.as_str() {
            "user-input.requested" => {
                if !open.contains(&request_id) {
                    open.push(request_id);
                }
            }
            "user-input.resolved" => open.retain(|id| *id != request_id),
            "provider.user-input.respond.failed"
                if detail.is_some_and(|detail| {
                    detail.contains("stale pending user-input request")
                        || detail.contains("unknown pending user-input request")
                        || detail.contains("unknown pending user input request")
                        || detail.contains("unknown pending codex user input request")
                }) =>
            {
                open.retain(|id| *id != request_id);
            }
            _ => {}
        }
    }
    open.len() as i64
}

fn refresh_thread_shell_summary(conn: &Conn, thread_id: &str) -> Result<(), DbError> {
    let Some(mut row) = threads::get_by_id(conn, thread_id)? else {
        return Ok(());
    };
    let latest_user_message_at = thread_messages::get_latest_user_message_at(conn, thread_id)?;
    let has_actionable = proposed_plans::has_actionable_by_thread_id(conn, thread_id, row.latest_turn_id.as_deref())?;
    let activities = thread_activities::list_user_input_lifecycle_by_thread_id(conn, thread_id)?;
    let pending_approval_count = pending_approvals::count_pending_by_thread_id(conn, thread_id)?;
    row.latest_user_message_at = latest_user_message_at;
    row.pending_approval_count = pending_approval_count;
    row.pending_user_input_count = derive_pending_user_input_count(&activities);
    row.has_actionable_proposed_plan = i64::from(has_actionable);
    threads::upsert(conn, &row)
}

fn with_thread(conn: &Conn, thread_id: &str, update: impl FnOnce(&mut ProjectionThread)) -> Result<bool, DbError> {
    let Some(mut row) = threads::get_by_id(conn, thread_id)? else {
        return Ok(false);
    };
    update(&mut row);
    threads::upsert(conn, &row)?;
    Ok(true)
}

fn apply_threads(conn: &Conn, event: &ProjectionEvent, side_effects: &mut AttachmentSideEffects) -> Result<(), DbError> {
    let payload = &event.payload;
    let thread_id = payload_thread_id(event);
    let updated_at = payload_str(event, "updatedAt").to_string();
    match event.event_type.as_str() {
        "thread.created" => {
            // A draft retry can re-create this id; links belong to the old incarnation.
            thread_pull_requests::delete_by_thread_id(conn, thread_id)?;
            threads::upsert(
                conn,
                &ProjectionThread {
                    thread_id: thread_id.to_string(),
                    project_id: payload_str(event, "projectId").to_string(),
                    title: payload_str(event, "title").to_string(),
                    title_state: None,
                    model_selection: payload.get("modelSelection").cloned().unwrap_or(Value::Null),
                    runtime_mode: payload_str(event, "runtimeMode").to_string(),
                    interaction_mode: payload_str(event, "interactionMode").to_string(),
                    branch: opt_string(payload, "branch"),
                    worktree_path: opt_string(payload, "worktreePath"),
                    linked_pull_request: None,
                    branch_pull_request: None,
                    latest_turn_id: None,
                    created_at: payload_str(event, "createdAt").to_string(),
                    updated_at,
                    archived_at: None,
                    settled_override: None,
                    settled_at: None,
                    unsettled_at: None,
                    snoozed_until: None,
                    snoozed_at: None,
                    pinned_at: None,
                    pin_order_key: None,
                    active_order_key: None,
                    auto_settle_disabled_at: None,
                    title_regeneration_request_id: None,
                    title_regeneration_started_at: None,
                    latest_user_message_at: None,
                    pending_approval_count: 0,
                    pending_user_input_count: 0,
                    has_actionable_proposed_plan: 0,
                    deleted_at: None,
                },
            )
        }
        "thread.archived" => with_thread(conn, thread_id, |row| {
            row.archived_at = opt_string(payload, "archivedAt");
            row.title_regeneration_request_id = None;
            row.title_regeneration_started_at = None;
            row.updated_at = updated_at;
        })
        .map(drop),
        "thread.unarchived" => with_thread(conn, thread_id, |row| {
            row.archived_at = None;
            row.updated_at = updated_at;
        })
        .map(drop),
        "thread.settled" => with_thread(conn, thread_id, |row| {
            row.settled_override = Some("settled".to_string());
            row.settled_at = opt_string(payload, "settledAt");
            row.unsettled_at = None;
            row.active_order_key = None;
            row.updated_at = updated_at;
        })
        .map(drop),
        "thread.unsettled" => with_thread(conn, thread_id, |row| {
            let reason_user = str_field(payload, "reason") == Some("user");
            // Re-entry stamp for active-list ordering. A thread already pinned active keeps
            // its stamp: the activity reset that clears the pin is not a re-entry.
            let unsettled_at = if row.settled_override.as_deref() == Some("active") {
                row.unsettled_at.clone()
            } else {
                Some(updated_at.clone())
            };
            row.settled_override = reason_user.then(|| "active".to_string());
            row.settled_at = None;
            row.unsettled_at = unsettled_at;
            row.updated_at = updated_at;
        })
        .map(drop),
        "thread.snoozed" => with_thread(conn, thread_id, |row| {
            row.snoozed_until = opt_string(payload, "snoozedUntil");
            row.snoozed_at = opt_string(payload, "snoozedAt");
            row.updated_at = updated_at;
        })
        .map(drop),
        "thread.unsnoozed" => with_thread(conn, thread_id, |row| {
            row.snoozed_until = None;
            row.snoozed_at = None;
            row.updated_at = updated_at;
        })
        .map(drop),
        "thread.pinned" => with_thread(conn, thread_id, |row| {
            row.pinned_at = opt_string(payload, "pinnedAt");
            if has_key(payload, "pinOrderKey") {
                row.pin_order_key = opt_string(payload, "pinOrderKey");
            }
            row.updated_at = updated_at;
        })
        .map(drop),
        "thread.unpinned" => with_thread(conn, thread_id, |row| {
            row.pinned_at = None;
            row.pin_order_key = None;
            row.updated_at = updated_at;
        })
        .map(drop),
        "thread.auto-settle-set" => with_thread(conn, thread_id, |row| {
            row.auto_settle_disabled_at = opt_string(payload, "autoSettleDisabledAt");
            row.updated_at = updated_at;
        })
        .map(drop),
        "thread.pin-reordered" => with_thread(conn, thread_id, |row| {
            row.pin_order_key = opt_string(payload, "orderKey");
            row.updated_at = updated_at.clone();
        })
        .map(drop),
        "thread.meta-updated" => {
            let found = with_thread(conn, thread_id, |row| {
                if has_key(payload, "title") {
                    row.title = payload_str(event, "title").to_string();
                }
                if has_key(payload, "activeOrderKey") {
                    row.active_order_key = opt_string(payload, "activeOrderKey");
                }
                if has_key(payload, "titleState") {
                    row.title_state = opt_value(payload, "titleState");
                }
                if has_key(payload, "titleRegeneration") {
                    let regeneration = payload.get("titleRegeneration").unwrap_or(&Value::Null);
                    row.title_regeneration_request_id = opt_string(regeneration, "requestId");
                    row.title_regeneration_started_at = opt_string(regeneration, "startedAt");
                }
                if has_key(payload, "modelSelection") {
                    row.model_selection = payload["modelSelection"].clone();
                }
                if has_key(payload, "branch") {
                    row.branch = opt_string(payload, "branch");
                }
                if has_key(payload, "worktreePath") {
                    row.worktree_path = opt_string(payload, "worktreePath");
                }
                if has_key(payload, "linkedPullRequest") {
                    row.linked_pull_request = opt_value(payload, "linkedPullRequest");
                }
                if has_key(payload, "branchPullRequest") {
                    row.branch_pull_request = opt_value(payload, "branchPullRequest");
                }
                row.updated_at = updated_at.clone();
            })?;
            if !found {
                return Ok(());
            }
            // Legacy single-link events replay into the link table. The old field held one
            // user-chosen link, so it only ever owns the manual rows.
            if has_key(payload, "linkedPullRequest") {
                thread_pull_requests::delete_by_thread_id_and_source(conn, thread_id, "manual")?;
                if let Some(linked) = opt_value(payload, "linkedPullRequest") {
                    let url = str_field(&linked, "url").unwrap_or("").to_string();
                    let key = legacy_thread_pull_request_key(
                        str_field(&linked, "repository").unwrap_or(""),
                        linked.get("number").and_then(Value::as_i64).unwrap_or(0),
                        &url,
                        None,
                    );
                    thread_pull_requests::upsert(
                        conn,
                        &ProjectionThreadPullRequest {
                            thread_id: thread_id.to_string(),
                            host: key.host,
                            repository: key.repository,
                            number: key.number,
                            url,
                            source: "manual".to_string(),
                            linked_at: updated_at,
                            snapshot: None,
                            stack: None,
                        },
                    )?;
                }
            }
            Ok(())
        }
        "thread.pull-request-linked" => {
            let Some(mut row) = threads::get_by_id(conn, thread_id)? else {
                return Ok(());
            };
            let link = payload.get("link").unwrap_or(&Value::Null);
            thread_pull_requests::upsert(
                conn,
                &ProjectionThreadPullRequest {
                    thread_id: thread_id.to_string(),
                    host: str_field(link, "host").unwrap_or("").to_string(),
                    repository: str_field(link, "repository").unwrap_or("").to_string(),
                    number: link.get("number").and_then(Value::as_i64).unwrap_or(0),
                    url: str_field(link, "url").unwrap_or("").to_string(),
                    source: str_field(link, "source").unwrap_or("").to_string(),
                    linked_at: str_field(link, "linkedAt").unwrap_or("").to_string(),
                    snapshot: opt_value(link, "snapshot"),
                    stack: opt_value(link, "stack"),
                },
            )?;
            row.updated_at = updated_at;
            threads::upsert(conn, &row)
        }
        "thread.pull-request-unlinked" => {
            let Some(mut row) = threads::get_by_id(conn, thread_id)? else {
                return Ok(());
            };
            let links = thread_pull_requests::list_by_thread_id(conn, thread_id)?;
            if let Some(link) = find_link(&links, payload) {
                thread_pull_requests::delete(conn, thread_id, &link.host, &link.repository, link.number)?;
            }
            row.updated_at = updated_at;
            threads::upsert(conn, &row)
        }
        "thread.pull-request-synced" => {
            let Some(mut row) = threads::get_by_id(conn, thread_id)? else {
                return Ok(());
            };
            // A sync for a link the user removed in the meantime is stale; drop it.
            let links = thread_pull_requests::list_by_thread_id(conn, thread_id)?;
            let Some(link) = find_link(&links, payload) else {
                return Ok(());
            };
            let mut link = link.clone();
            link.snapshot = opt_value(payload, "snapshot");
            link.stack = opt_value(payload, "stack");
            thread_pull_requests::upsert(conn, &link)?;
            row.updated_at = updated_at;
            threads::upsert(conn, &row)
        }
        "thread.runtime-mode-set" => with_thread(conn, thread_id, |row| {
            row.runtime_mode = payload_str(event, "runtimeMode").to_string();
            row.updated_at = updated_at;
        })
        .map(drop),
        "thread.interaction-mode-set" => with_thread(conn, thread_id, |row| {
            row.interaction_mode = payload_str(event, "interactionMode").to_string();
            row.updated_at = updated_at;
        })
        .map(drop),
        "thread.deleted" => {
            // A draft retry can re-create this id later in the log. During replay the
            // attachment files on disk already belong to that later incarnation, so only an
            // unsuperseded deletion removes them.
            let recreated_later = event_store::has_event_after(conn, "thread", thread_id, Some("thread.created"), event.sequence)?;
            if !recreated_later {
                side_effects.delete_thread(thread_id);
            }
            // A tombstoned thread must not show up as linked to a pull request.
            thread_pull_requests::delete_by_thread_id(conn, thread_id)?;
            let deleted_at = payload_str(event, "deletedAt").to_string();
            with_thread(conn, thread_id, |row| {
                row.deleted_at = Some(deleted_at.clone());
                row.updated_at = deleted_at;
            })
            .map(drop)
        }
        // A message cannot change any summary field except latestUserMessageAt, a monotonic
        // maximum that folds in directly.
        "thread.message-sent" => with_thread(conn, thread_id, |row| {
            let previous = row.latest_user_message_at.clone();
            let created_at = payload_str(event, "createdAt");
            let is_user = str_field(payload, "role") == Some("user") && !payload_str(event, "messageId").starts_with("import:");
            row.updated_at = event.occurred_at.clone();
            row.latest_user_message_at = if is_user && previous.as_deref().is_none_or(|previous| js_string_gt(created_at, previous)) {
                Some(created_at.to_string())
            } else {
                previous
            };
        })
        .map(drop),
        "thread.proposed-plan-upserted" | "thread.activity-appended" | "thread.approval-response-requested" | "thread.user-input-response-requested" => {
            let found = with_thread(conn, thread_id, |row| {
                row.updated_at = event.occurred_at.clone();
            })?;
            if found && should_refresh_thread_shell_summary(event) {
                refresh_thread_shell_summary(conn, thread_id)?;
            }
            Ok(())
        }
        "thread.session-set" => {
            let active_turn_id = payload.get("session").and_then(|session| str_field(session, "activeTurnId")).map(str::to_owned);
            let found = with_thread(conn, thread_id, |row| {
                // activeTurnId describes current work; a terminal session must not erase history.
                if active_turn_id.is_some() {
                    row.latest_turn_id = active_turn_id;
                }
                row.updated_at = event.occurred_at.clone();
            })?;
            if found {
                refresh_thread_shell_summary(conn, thread_id)?;
            }
            Ok(())
        }
        "thread.turn-diff-completed" => {
            let found = with_thread(conn, thread_id, |row| {
                row.latest_turn_id = opt_string(payload, "turnId");
                row.updated_at = event.occurred_at.clone();
            })?;
            if found {
                refresh_thread_shell_summary(conn, thread_id)?;
            }
            Ok(())
        }
        "thread.reverted" => {
            let Some(mut row) = threads::get_by_id(conn, thread_id)? else {
                return Ok(());
            };
            let turn_count = payload.get("turnCount").and_then(Value::as_i64).unwrap_or(0);
            let retained = turns::list_by_thread_id(conn, thread_id)?;
            let mut latest_turn_id: Option<String> = None;
            let mut latest_count = -1;
            for turn in &retained {
                let (Some(turn_id), Some(count)) = (&turn.turn_id, turn.checkpoint_turn_count) else {
                    continue;
                };
                if count > turn_count {
                    continue;
                }
                if count > latest_count {
                    latest_count = count;
                    latest_turn_id = Some(turn_id.clone());
                }
            }
            row.latest_turn_id = latest_turn_id;
            row.updated_at = event.occurred_at.clone();
            threads::upsert(conn, &row)?;
            refresh_thread_shell_summary(conn, thread_id)
        }
        _ => Ok(()),
    }
}

/// `left > right` on JS strings (UTF-16 code unit order).
fn js_string_gt(left: &str, right: &str) -> bool {
    left.encode_utf16().cmp(right.encode_utf16()) == std::cmp::Ordering::Greater
}

fn find_link<'a>(links: &'a [ProjectionThreadPullRequest], payload: &Value) -> Option<&'a ProjectionThreadPullRequest> {
    let host = str_field(payload, "host").unwrap_or("");
    let repository = str_field(payload, "repository").unwrap_or("");
    let number = payload.get("number").and_then(Value::as_i64).unwrap_or(0);
    links.iter().find(|candidate| {
        thread_pull_request_keys_equal(
            (&candidate.host, &candidate.repository, candidate.number, Some(&candidate.url)),
            (host, repository, number, None),
        )
    })
}

// ---------------------------------------------------------------------------------------------
// messages
// ---------------------------------------------------------------------------------------------

/// `compareDateTimeStrings`: by absolute time, valid timestamps after invalid ones, invalid
/// ones by code units.
pub fn compare_date_time_strings(left: &str, right: &str) -> std::cmp::Ordering {
    static ZONED: OnceLock<Regex> = OnceLock::new();
    let zoned = ZONED.get_or_init(|| {
        Regex::new(r"^(?:\d{4}|[+-]\d{6})-(?:0[1-9]|1[0-2])-(?:0[1-9]|[12]\d|3[01])T(?:(?:[01]\d|2[0-3]):[0-5]\d(?::[0-5]\d(?:\.\d+)?)?|24:00(?::00(?:\.0+)?)?)(?:Z|[+-](?:[01]\d|2[0-3]):[0-5]\d)$").unwrap()
    });
    let parse = |value: &str| -> Option<i64> {
        if !zoned.is_match(value) || crate::js::trim(value) != value {
            return None;
        }
        zc_core::time::parse_iso_millis(value)
    };
    match (parse(left), parse(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (None, None) => left.encode_utf16().cmp(right.encode_utf16()),
    }
}

fn retained_turn_ids(turns: &[ProjectionTurn], turn_count: i64) -> HashSet<String> {
    turns
        .iter()
        .filter(|turn| turn.checkpoint_turn_count.is_some_and(|count| count <= turn_count))
        .filter_map(|turn| turn.turn_id.clone())
        .collect()
}

/// `retainProjectionMessagesAfterRevert`.
pub fn retain_messages_after_revert(messages: &[ProjectionThreadMessage], turns: &[ProjectionTurn], turn_count: i64) -> Vec<ProjectionThreadMessage> {
    let mut retained_message_ids: HashSet<String> = HashSet::new();
    let mut retained_turn_ids: HashSet<String> = HashSet::new();
    for turn in turns
        .iter()
        .filter(|turn| turn.turn_id.is_some() && turn.checkpoint_turn_count.is_some_and(|count| count <= turn_count))
    {
        if let Some(turn_id) = &turn.turn_id {
            retained_turn_ids.insert(turn_id.clone());
        }
        if let Some(id) = &turn.pending_message_id {
            retained_message_ids.insert(id.clone());
        }
        if let Some(id) = &turn.assistant_message_id {
            retained_message_ids.insert(id.clone());
        }
    }
    let imported = |message: &ProjectionThreadMessage| message.message_id.starts_with("import:");
    for message in messages {
        if message.role == "system" || imported(message) {
            retained_message_ids.insert(message.message_id.clone());
            continue;
        }
        if message.turn_id.as_ref().is_some_and(|turn_id| retained_turn_ids.contains(turn_id)) {
            retained_message_ids.insert(message.message_id.clone());
        }
    }

    for role in ["user", "assistant"] {
        let retained_count = messages
            .iter()
            .filter(|message| message.role == role && !imported(message) && retained_message_ids.contains(&message.message_id))
            .count() as i64;
        let missing = (turn_count - retained_count).max(0) as usize;
        if missing > 0 {
            let mut fallback: Vec<&ProjectionThreadMessage> = messages
                .iter()
                .filter(|message| {
                    message.role == role
                        && !retained_message_ids.contains(&message.message_id)
                        && message.turn_id.as_ref().is_none_or(|turn_id| retained_turn_ids.contains(turn_id))
                })
                .collect();
            fallback.sort_by(|left, right| {
                compare_date_time_strings(&left.created_at, &right.created_at).then_with(|| crate::js::locale_compare(&left.message_id, &right.message_id))
            });
            for message in fallback.into_iter().take(missing) {
                retained_message_ids.insert(message.message_id.clone());
            }
        }
    }

    messages
        .iter()
        .filter(|message| retained_message_ids.contains(&message.message_id))
        .cloned()
        .collect()
}

fn apply_thread_messages(conn: &Conn, event: &ProjectionEvent, side_effects: &mut AttachmentSideEffects) -> Result<(), DbError> {
    let payload = &event.payload;
    let thread_id = payload_thread_id(event);
    match event.event_type.as_str() {
        // A draft retry re-creates a soft-deleted thread id. Every projector drops its own
        // rows for the old incarnation here.
        "thread.created" => thread_messages::delete_by_thread_id(conn, thread_id),
        "thread.message-sent" => {
            let attachments = payload.get("attachments").cloned();
            let context = payload.get("context").cloned();
            let message_id = payload_str(event, "messageId");
            let turn_id = opt_string(payload, "turnId");
            let role = payload_str(event, "role").to_string();
            let text = payload_str(event, "text").to_string();
            if payload.get("streaming").and_then(Value::as_bool) == Some(true) {
                return thread_messages::append_streaming(
                    conn,
                    &AppendStreamingMessage {
                        message_id: message_id.to_string(),
                        thread_id: thread_id.to_string(),
                        turn_id,
                        role,
                        text,
                        attachments,
                        context,
                        created_at: payload_str(event, "createdAt").to_string(),
                        updated_at: payload_str(event, "updatedAt").to_string(),
                    },
                );
            }
            let previous = thread_messages::get_by_message_id(conn, message_id)?;
            let next_text = match &previous {
                Some(message) if text.is_empty() => message.text.clone(),
                _ => text,
            };
            let next_attachments = attachments.or_else(|| previous.as_ref().and_then(|m| m.attachments.clone()));
            let next_context = context.or_else(|| previous.as_ref().and_then(|m| m.context.clone()));
            thread_messages::upsert(
                conn,
                &ProjectionThreadMessage {
                    message_id: message_id.to_string(),
                    thread_id: thread_id.to_string(),
                    turn_id,
                    role,
                    text: next_text,
                    attachments: next_attachments,
                    context: next_context,
                    is_streaming: false,
                    created_at: previous
                        .as_ref()
                        .map(|message| message.created_at.clone())
                        .unwrap_or_else(|| payload_str(event, "createdAt").to_string()),
                    updated_at: payload_str(event, "updatedAt").to_string(),
                },
            )
        }
        "thread.reverted" => {
            let existing = thread_messages::list_by_thread_id(conn, thread_id)?;
            if existing.is_empty() {
                return Ok(());
            }
            let turns = turns::list_by_thread_id(conn, thread_id)?;
            let turn_count = payload.get("turnCount").and_then(Value::as_i64).unwrap_or(0);
            let kept = retain_messages_after_revert(&existing, &turns, turn_count);
            if kept.len() == existing.len() {
                return Ok(());
            }
            thread_messages::delete_by_thread_id(conn, thread_id)?;
            for message in &kept {
                thread_messages::upsert(conn, message)?;
            }
            side_effects.prune_thread(
                thread_id,
                collect_thread_attachment_relative_paths(thread_id, kept.iter().filter_map(|message| message.attachments.as_ref())),
            );
            Ok(())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// proposed plans
// ---------------------------------------------------------------------------------------------

fn apply_thread_proposed_plans(conn: &Conn, event: &ProjectionEvent) -> Result<(), DbError> {
    let payload = &event.payload;
    let thread_id = payload_thread_id(event);
    match event.event_type.as_str() {
        "thread.created" => proposed_plans::delete_by_thread_id(conn, thread_id),
        "thread.proposed-plan-upserted" => {
            let plan = payload.get("proposedPlan").unwrap_or(&Value::Null);
            proposed_plans::upsert(
                conn,
                &ProjectionThreadProposedPlan {
                    plan_id: str_field(plan, "id").unwrap_or("").to_string(),
                    thread_id: thread_id.to_string(),
                    turn_id: opt_string(plan, "turnId"),
                    plan_markdown: str_field(plan, "planMarkdown").unwrap_or("").to_string(),
                    implemented_at: opt_string(plan, "implementedAt"),
                    implementation_thread_id: opt_string(plan, "implementationThreadId"),
                    created_at: str_field(plan, "createdAt").unwrap_or("").to_string(),
                    updated_at: str_field(plan, "updatedAt").unwrap_or("").to_string(),
                },
            )
        }
        "thread.reverted" => {
            let existing = proposed_plans::list_by_thread_id(conn, thread_id)?;
            if existing.is_empty() {
                return Ok(());
            }
            let turns = turns::list_by_thread_id(conn, thread_id)?;
            let turn_count = payload.get("turnCount").and_then(Value::as_i64).unwrap_or(0);
            let retained = retained_turn_ids(&turns, turn_count);
            let kept: Vec<_> = existing
                .iter()
                .filter(|plan| plan.turn_id.as_ref().is_none_or(|id| retained.contains(id)))
                .cloned()
                .collect();
            if kept.len() == existing.len() {
                return Ok(());
            }
            proposed_plans::delete_by_thread_id(conn, thread_id)?;
            for plan in &kept {
                proposed_plans::upsert(conn, plan)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// activities
// ---------------------------------------------------------------------------------------------

fn apply_thread_activities(conn: &Conn, event: &ProjectionEvent, side_effects: &mut AttachmentSideEffects) -> Result<(), DbError> {
    let payload = &event.payload;
    let thread_id = payload_thread_id(event);
    match event.event_type.as_str() {
        "thread.created" => thread_activities::delete_by_thread_id(conn, thread_id),
        "thread.activity-appended" => {
            let activity = activity(event);
            thread_activities::upsert(
                conn,
                &ProjectionThreadActivity {
                    activity_id: str_field(activity, "id").unwrap_or("").to_string(),
                    thread_id: thread_id.to_string(),
                    turn_id: opt_string(activity, "turnId"),
                    tone: str_field(activity, "tone").unwrap_or("").to_string(),
                    kind: str_field(activity, "kind").unwrap_or("").to_string(),
                    summary: str_field(activity, "summary").unwrap_or("").to_string(),
                    payload: activity.get("payload").cloned().unwrap_or(Value::Null),
                    sequence: activity.get("sequence").and_then(Value::as_i64),
                    created_at: str_field(activity, "createdAt").unwrap_or("").to_string(),
                },
            )
        }
        "thread.reverted" => {
            let existing = thread_activities::list_by_thread_id(conn, thread_id, None, None)?;
            if existing.is_empty() {
                return Ok(());
            }
            let turns = turns::list_by_thread_id(conn, thread_id)?;
            let turn_count = payload.get("turnCount").and_then(Value::as_i64).unwrap_or(0);
            let retained = retained_turn_ids(&turns, turn_count);
            let kept: Vec<_> = existing
                .iter()
                .filter(|row| row.turn_id.as_ref().is_none_or(|id| retained.contains(id)))
                .cloned()
                .collect();
            if kept.len() == existing.len() {
                return Ok(());
            }
            thread_activities::delete_by_thread_id(conn, thread_id)?;
            for row in &kept {
                thread_activities::upsert(conn, row)?;
            }
            side_effects.prune_thread(thread_id, HashSet::new());
            Ok(())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// sessions
// ---------------------------------------------------------------------------------------------

fn apply_thread_sessions(conn: &Conn, event: &ProjectionEvent) -> Result<(), DbError> {
    let thread_id = payload_thread_id(event);
    match event.event_type.as_str() {
        "thread.created" => thread_sessions::delete_by_thread_id(conn, thread_id),
        "thread.session-set" => {
            let session = event.payload.get("session").unwrap_or(&Value::Null);
            thread_sessions::upsert(
                conn,
                &ProjectionThreadSession {
                    thread_id: thread_id.to_string(),
                    status: str_field(session, "status").unwrap_or("").to_string(),
                    provider_name: opt_string(session, "providerName"),
                    provider_instance_id: opt_string(session, "providerInstanceId"),
                    runtime_mode: str_field(session, "runtimeMode").unwrap_or("").to_string(),
                    active_turn_id: opt_string(session, "activeTurnId"),
                    last_error: opt_string(session, "lastError"),
                    updated_at: str_field(session, "updatedAt").unwrap_or("").to_string(),
                },
            )
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// turns
// ---------------------------------------------------------------------------------------------

/// The turn state to settle still-running turns with when their session leaves "running",
/// or `None` while it is (re)starting or running.
fn settled_turn_state_for_session_status(status: &str) -> Option<&'static str> {
    match status {
        "idle" | "ready" => Some("completed"),
        "error" => Some("error"),
        "interrupted" | "stopped" => Some("interrupted"),
        _ => None,
    }
}

fn new_turn(thread_id: &str, turn_id: &str) -> ProjectionTurn {
    ProjectionTurn {
        thread_id: thread_id.to_string(),
        turn_id: Some(turn_id.to_string()),
        pending_message_id: None,
        source_proposed_plan_thread_id: None,
        source_proposed_plan_id: None,
        assistant_message_id: None,
        state: "running".to_string(),
        requested_at: String::new(),
        started_at: None,
        completed_at: None,
        checkpoint_turn_count: None,
        checkpoint_ref: None,
        checkpoint_status: None,
        checkpoint_files: Value::Array(vec![]),
    }
}

fn apply_thread_turns(conn: &Conn, event: &ProjectionEvent) -> Result<(), DbError> {
    let payload = &event.payload;
    let thread_id = payload_thread_id(event);
    match event.event_type.as_str() {
        "thread.created" => turns::delete_by_thread_id(conn, thread_id),
        "thread.turn-start-requested" => {
            if let Some(pending) = turns::get_pending_turn_start_by_thread_id(conn, thread_id)? {
                if let Some(message) = thread_messages::get_by_message_id(conn, &pending.message_id)? {
                    let attachment_count = message.attachments.as_ref().and_then(Value::as_array).map(Vec::len).unwrap_or(0);
                    if message.role == "user" && attachment_count == 0 && crate::js::trim(&message.text).to_lowercase() == "/compact" {
                        return Ok(());
                    }
                }
            }
            let source = payload.get("sourceProposedPlan").unwrap_or(&Value::Null);
            turns::replace_pending_turn_start(
                conn,
                &PendingTurnStart {
                    thread_id: thread_id.to_string(),
                    message_id: payload_str(event, "messageId").to_string(),
                    source_proposed_plan_thread_id: opt_string(source, "threadId"),
                    source_proposed_plan_id: opt_string(source, "planId"),
                    requested_at: payload_str(event, "createdAt").to_string(),
                },
            )
        }
        "thread.activity-appended" => {
            let kind = activity_kind(event);
            if kind != "context-compaction" && kind != "provider.turn.start.failed" {
                return Ok(());
            }
            let Some(pending) = turns::get_pending_turn_start_by_thread_id(conn, thread_id)? else {
                return Ok(());
            };
            let request_id = activity(event).get("payload").and_then(extract_activity_request_id);
            if request_id.as_deref() != Some(pending.message_id.as_str()) {
                return Ok(());
            }
            turns::delete_pending_turn_start_by_thread_id(conn, thread_id)
        }
        "thread.session-set" => {
            let session = payload.get("session").unwrap_or(&Value::Null);
            let status = str_field(session, "status").unwrap_or("");
            let session_updated_at = str_field(session, "updatedAt").unwrap_or("").to_string();
            let active_turn_id = str_field(session, "activeTurnId");
            let Some(turn_id) = active_turn_id.filter(|_| status == "running") else {
                let from_provider_session_set = event.command_id.as_deref().is_some_and(|id| id.starts_with("server:provider-session-set:"));
                if (status == "ready" && from_provider_session_set) || status == "error" || status == "stopped" || status == "interrupted" {
                    turns::delete_pending_turn_start_by_thread_id(conn, thread_id)?;
                }
                // Leaving the "running" session status is the turn-end signal: settle
                // still-running turns so their duration reflects the whole turn.
                let Some(settled_state) = settled_turn_state_for_session_status(status) else {
                    return Ok(());
                };
                for turn in turns::list_by_thread_id(conn, thread_id)? {
                    if turn.turn_id.is_some() && turn.state == "running" {
                        let mut turn = turn;
                        turn.state = settled_state.to_string();
                        // A running turn's completedAt can only hold a mid-turn placeholder
                        // checkpoint timestamp: the session leaving "running" is the end.
                        turn.completed_at = Some(session_updated_at.clone());
                        turns::upsert_by_turn_id(conn, &turn)?;
                    }
                }
                return Ok(());
            };

            // A new active turn supersedes any still-running turn on the same thread.
            for turn in turns::list_by_thread_id(conn, thread_id)? {
                if turn.turn_id.as_deref().is_some_and(|id| id != turn_id) && turn.state == "running" {
                    let mut turn = turn;
                    turn.state = "completed".to_string();
                    turn.completed_at = Some(session_updated_at.clone());
                    turns::upsert_by_turn_id(conn, &turn)?;
                }
            }

            let existing = turns::get_by_turn_id(conn, thread_id, turn_id)?;
            let pending = turns::get_pending_turn_start_by_thread_id(conn, thread_id)?;
            let pending_requested_at = pending.as_ref().map(|p| p.requested_at.clone());
            match existing {
                Some(mut turn) => {
                    if turn.state != "completed" && turn.state != "error" {
                        turn.state = "running".to_string();
                    }
                    if turn.pending_message_id.is_none() {
                        turn.pending_message_id = pending.as_ref().map(|p| p.message_id.clone());
                    }
                    if turn.source_proposed_plan_thread_id.is_none() {
                        turn.source_proposed_plan_thread_id = pending.as_ref().and_then(|p| p.source_proposed_plan_thread_id.clone());
                    }
                    if turn.source_proposed_plan_id.is_none() {
                        turn.source_proposed_plan_id = pending.as_ref().and_then(|p| p.source_proposed_plan_id.clone());
                    }
                    if turn.started_at.is_none() {
                        turn.started_at = Some(pending_requested_at.clone().unwrap_or_else(|| event.occurred_at.clone()));
                    }
                    // `requestedAt` is NOT NULL, so `?? …` never applies.
                    turns::upsert_by_turn_id(conn, &turn)?;
                }
                None => {
                    let requested_at = pending_requested_at.unwrap_or_else(|| event.occurred_at.clone());
                    let mut turn = new_turn(thread_id, turn_id);
                    turn.pending_message_id = pending.as_ref().map(|p| p.message_id.clone());
                    turn.source_proposed_plan_thread_id = pending.as_ref().and_then(|p| p.source_proposed_plan_thread_id.clone());
                    turn.source_proposed_plan_id = pending.as_ref().and_then(|p| p.source_proposed_plan_id.clone());
                    turn.requested_at = requested_at.clone();
                    turn.started_at = Some(requested_at);
                    turns::upsert_by_turn_id(conn, &turn)?;
                }
            }
            turns::delete_pending_turn_start_by_thread_id(conn, thread_id)
        }
        "thread.message-sent" => {
            let Some(turn_id) = str_field(payload, "turnId") else {
                return Ok(());
            };
            if str_field(payload, "role") != Some("assistant") {
                return Ok(());
            }
            // A completed assistant message only settles the turn once the session is no
            // longer running it: providers emit several assistant messages per turn.
            let session = thread_sessions::get_by_thread_id(conn, thread_id)?;
            let turn_still_running = session.is_some_and(|session| session.status == "running" && session.active_turn_id.as_deref() == Some(turn_id));
            let streaming = payload.get("streaming").and_then(Value::as_bool) == Some(true);
            let settles_turn = !streaming && !turn_still_running;
            let message_id = payload_str(event, "messageId").to_string();
            let created_at = payload_str(event, "createdAt").to_string();
            let updated_at = payload_str(event, "updatedAt").to_string();
            match turns::get_by_turn_id(conn, thread_id, turn_id)? {
                Some(mut turn) => {
                    turn.assistant_message_id = Some(message_id);
                    if settles_turn {
                        turn.state = match turn.state.as_str() {
                            "interrupted" => "interrupted",
                            "error" => "error",
                            _ => "completed",
                        }
                        .to_string();
                        if turn.completed_at.is_none() {
                            turn.completed_at = Some(updated_at);
                        }
                    }
                    if turn.started_at.is_none() {
                        turn.started_at = Some(created_at);
                    }
                    turns::upsert_by_turn_id(conn, &turn)
                }
                None => {
                    let mut turn = new_turn(thread_id, turn_id);
                    turn.assistant_message_id = Some(message_id);
                    turn.state = if settles_turn { "completed" } else { "running" }.to_string();
                    turn.requested_at = created_at.clone();
                    turn.started_at = Some(created_at);
                    turn.completed_at = settles_turn.then_some(updated_at);
                    turns::upsert_by_turn_id(conn, &turn)
                }
            }
        }
        "thread.turn-interrupt-requested" => {
            let Some(turn_id) = str_field(payload, "turnId") else {
                return Ok(());
            };
            let created_at = payload_str(event, "createdAt").to_string();
            match turns::get_by_turn_id(conn, thread_id, turn_id)? {
                Some(mut turn) => {
                    turn.state = "interrupted".to_string();
                    if turn.completed_at.is_none() {
                        turn.completed_at = Some(created_at.clone());
                    }
                    if turn.started_at.is_none() {
                        turn.started_at = Some(created_at);
                    }
                    turns::upsert_by_turn_id(conn, &turn)
                }
                None => {
                    let mut turn = new_turn(thread_id, turn_id);
                    turn.state = "interrupted".to_string();
                    turn.requested_at = created_at.clone();
                    turn.started_at = Some(created_at.clone());
                    turn.completed_at = Some(created_at);
                    turns::upsert_by_turn_id(conn, &turn)
                }
            }
        }
        "thread.turn-diff-completed" => {
            let turn_id = payload_str(event, "turnId");
            let status = payload_str(event, "status");
            let completed_at = payload_str(event, "completedAt").to_string();
            // Mid-turn diff updates produce placeholder checkpoints; record the checkpoint,
            // but don't settle a turn its session is still running.
            let session = thread_sessions::get_by_thread_id(conn, thread_id)?;
            let turn_still_running = session.is_some_and(|session| session.status == "running" && session.active_turn_id.as_deref() == Some(turn_id));
            let existing = turns::get_by_turn_id(conn, thread_id, turn_id)?;
            // Do not let a placeholder ("missing") overwrite a captured checkpoint.
            if existing
                .as_ref()
                .is_some_and(|turn| turn.checkpoint_status.as_deref().is_some_and(|current| current != "missing"))
                && status == "missing"
            {
                return Ok(());
            }
            let next_state = if status == "error" { "error" } else { "completed" };
            let checkpoint_turn_count = payload.get("checkpointTurnCount").and_then(Value::as_i64).unwrap_or(0);
            turns::clear_checkpoint_turn_conflict(conn, thread_id, turn_id, checkpoint_turn_count)?;
            let assistant_message_id = opt_string(payload, "assistantMessageId");
            let checkpoint_ref = opt_string(payload, "checkpointRef");
            let files = payload.get("files").cloned().unwrap_or(Value::Array(vec![]));
            match existing {
                Some(mut turn) => {
                    turn.assistant_message_id = assistant_message_id;
                    if !(turn_still_running || turn.state == "interrupted") {
                        turn.state = next_state.to_string();
                    }
                    turn.checkpoint_turn_count = Some(checkpoint_turn_count);
                    turn.checkpoint_ref = checkpoint_ref;
                    turn.checkpoint_status = Some(status.to_string());
                    turn.checkpoint_files = files;
                    if turn.started_at.is_none() {
                        turn.started_at = Some(completed_at.clone());
                    }
                    turn.completed_at = Some(completed_at);
                    turns::upsert_by_turn_id(conn, &turn)
                }
                None => {
                    let mut turn = new_turn(thread_id, turn_id);
                    turn.assistant_message_id = assistant_message_id;
                    turn.state = if turn_still_running { "running" } else { next_state }.to_string();
                    turn.requested_at = completed_at.clone();
                    turn.started_at = Some(completed_at.clone());
                    turn.completed_at = Some(completed_at);
                    turn.checkpoint_turn_count = Some(checkpoint_turn_count);
                    turn.checkpoint_ref = checkpoint_ref;
                    turn.checkpoint_status = Some(status.to_string());
                    turn.checkpoint_files = files;
                    turns::upsert_by_turn_id(conn, &turn)
                }
            }
        }
        "thread.reverted" => {
            let turn_count = payload.get("turnCount").and_then(Value::as_i64).unwrap_or(0);
            let existing = turns::list_by_thread_id(conn, thread_id)?;
            let kept: Vec<ProjectionTurn> = existing
                .into_iter()
                .filter(|turn| turn.turn_id.is_some() && turn.checkpoint_turn_count.is_some_and(|count| count <= turn_count))
                .collect();
            turns::delete_by_thread_id(conn, thread_id)?;
            for turn in &kept {
                turns::upsert_by_turn_id(conn, turn)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// pending approvals
// ---------------------------------------------------------------------------------------------

const APPROVAL_DECISIONS: &[&str] = &["accept", "acceptForSession", "acceptAlways", "decline", "cancel"];

fn apply_pending_approvals(conn: &Conn, event: &ProjectionEvent) -> Result<(), DbError> {
    let payload = &event.payload;
    let thread_id = payload_thread_id(event);
    match event.event_type.as_str() {
        "thread.created" => pending_approvals::delete_by_thread_id(conn, thread_id),
        "thread.activity-appended" => {
            let activity = activity(event);
            let activity_payload = activity.get("payload").unwrap_or(&Value::Null);
            let Some(request_id) = extract_activity_request_id(activity_payload).or_else(|| str_field(&event.metadata, "requestId").map(str::to_owned)) else {
                return Ok(());
            };
            let existing = pending_approvals::get_by_request_id(conn, &request_id)?;
            let activity_created_at = str_field(activity, "createdAt").unwrap_or("").to_string();
            let activity_turn_id = opt_string(activity, "turnId");
            match activity_kind(event) {
                "approval.resolved" => {
                    let decision = activity_payload
                        .as_object()
                        .and_then(|object| object.get("decision"))
                        .and_then(Value::as_str)
                        .filter(|decision| APPROVAL_DECISIONS.contains(decision))
                        .map(str::to_owned);
                    pending_approvals::upsert(
                        conn,
                        &ProjectionPendingApproval {
                            request_id,
                            thread_id: existing.as_ref().map(|row| row.thread_id.clone()).unwrap_or_else(|| thread_id.to_string()),
                            turn_id: match &existing {
                                Some(row) => row.turn_id.clone(),
                                None => activity_turn_id,
                            },
                            status: "resolved".to_string(),
                            decision,
                            created_at: existing
                                .as_ref()
                                .map(|row| row.created_at.clone())
                                .unwrap_or_else(|| activity_created_at.clone()),
                            resolved_at: Some(activity_created_at),
                        },
                    )
                }
                "provider.approval.respond.failed" => {
                    let detail = activity_detail_lower(activity_payload);
                    if is_stale_pending_approval_failure_detail(detail.as_deref()) {
                        let Some(row) = existing else {
                            return Ok(());
                        };
                        if row.status == "resolved" {
                            return Ok(());
                        }
                        return pending_approvals::upsert(
                            conn,
                            &ProjectionPendingApproval {
                                request_id,
                                thread_id: row.thread_id,
                                turn_id: row.turn_id,
                                status: "resolved".to_string(),
                                decision: None,
                                created_at: row.created_at,
                                resolved_at: Some(activity_created_at),
                            },
                        );
                    }
                    let Some(row) = existing.filter(|row| row.status == "resolved") else {
                        return Ok(());
                    };
                    // Sending a reply clears the badge before the provider accepts it. A
                    // failed reply restores the request unless a terminal event closed it.
                    let request_activities: Vec<ProjectionThreadActivity> = thread_activities::list_by_thread_id(conn, &row.thread_id, None, None)?
                        .into_iter()
                        .filter(|activity| extract_activity_request_id(&activity.payload).as_deref() == Some(request_id.as_str()))
                        .collect();
                    let was_requested = request_activities.iter().any(|activity| activity.kind == "approval.requested");
                    let was_resolved = request_activities.iter().any(|activity| {
                        activity.kind == "approval.resolved"
                            || (activity.kind == "provider.approval.respond.failed"
                                && is_stale_pending_approval_failure_detail(activity_detail_lower(&activity.payload).as_deref()))
                    });
                    if was_requested && !was_resolved {
                        let mut row = row;
                        row.status = "pending".to_string();
                        row.decision = None;
                        row.resolved_at = None;
                        pending_approvals::upsert(conn, &row)?;
                    }
                    Ok(())
                }
                // Only approval-requested activities create pending-approval rows; other
                // kinds carrying a requestId have their own accounting.
                "approval.requested" => {
                    if existing.as_ref().is_some_and(|row| row.status == "resolved") {
                        return Ok(());
                    }
                    pending_approvals::upsert(
                        conn,
                        &ProjectionPendingApproval {
                            request_id,
                            thread_id: thread_id.to_string(),
                            turn_id: activity_turn_id,
                            status: "pending".to_string(),
                            decision: None,
                            created_at: existing.map(|row| row.created_at).unwrap_or(activity_created_at),
                            resolved_at: None,
                        },
                    )
                }
                _ => Ok(()),
            }
        }
        "thread.approval-response-requested" => {
            let request_id = payload_str(event, "requestId").to_string();
            let created_at = payload_str(event, "createdAt").to_string();
            let existing = pending_approvals::get_by_request_id(conn, &request_id)?;
            pending_approvals::upsert(
                conn,
                &ProjectionPendingApproval {
                    request_id,
                    thread_id: existing.as_ref().map(|row| row.thread_id.clone()).unwrap_or_else(|| thread_id.to_string()),
                    turn_id: existing.as_ref().and_then(|row| row.turn_id.clone()),
                    status: "resolved".to_string(),
                    decision: opt_string(payload, "decision"),
                    created_at: existing.map(|row| row.created_at).unwrap_or_else(|| created_at.clone()),
                    resolved_at: Some(created_at),
                },
            )
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// attachment cleanup
// ---------------------------------------------------------------------------------------------

/// `applyAttachmentSideEffects`: re-checks deletions against later re-creations, recomputes
/// what pruned threads still reference (later events of the same transaction can add
/// references), then removes files. Never fails: errors are logged and give `false`.
fn apply_attachment_side_effects(conn: &Conn, attachments_dir: &Path, sequence: i64, event_type: &str, side_effects: &AttachmentSideEffects) -> bool {
    let run = || -> Result<(), Box<dyn std::error::Error>> {
        let mut effective = AttachmentSideEffects::default();
        for thread_id in &side_effects.deleted_thread_ids {
            let recreated_later = event_store::has_event_after(conn, "thread", thread_id, Some("thread.created"), sequence)?;
            if !recreated_later {
                effective.delete_thread(thread_id);
            }
        }
        for (thread_id, _) in &side_effects.pruned_thread_relative_paths {
            let messages = thread_messages::list_by_thread_id(conn, thread_id)?;
            let mut retained = collect_thread_attachment_relative_paths(thread_id, messages.iter().filter_map(|message| message.attachments.as_ref()));
            for activity in thread_activities::list_by_thread_id(conn, thread_id, None, None)? {
                if activity.kind != "user-input.answer-submitted" {
                    continue;
                }
                let Ok(_) = serde_json::from_value::<zc_contracts::UserInputAttachmentAnswerPayload>(activity.payload.clone()) else {
                    continue;
                };
                let by_question = activity.payload.get("attachmentsByQuestionId").and_then(Value::as_object);
                for attachments in by_question.into_iter().flat_map(|map| map.values()) {
                    for attachment in attachments.as_array().into_iter().flatten() {
                        if let Some(path) = attachment_relative_path(attachment) {
                            retained.insert(path);
                        }
                    }
                }
            }
            effective.prune_thread(thread_id, retained);
        }
        run_attachment_side_effects(attachments_dir, &effective)?;
        Ok(())
    };
    match run() {
        Ok(()) => true,
        Err(cause) => {
            tracing::warn!(sequence, event_type, %cause, "failed to apply projected attachment side-effects");
            false
        }
    }
}
