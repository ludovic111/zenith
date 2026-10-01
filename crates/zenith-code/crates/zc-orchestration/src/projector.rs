//! `orchestration/projector.ts`: folds events into the in-memory command read model the
//! decider works on.
//!
//! The TS projector returns a new immutable model per event; this one updates the model in
//! place (callers that need the old model clone it first, as the engine and
//! `decideCommandSequence` do). It decodes nothing: events arrive decoded, so where TS could
//! fail with `OrchestrationProjectorDecodeError`, Rust has already failed at the row decoder.
//!
//! Caps and retention match the TS projector and the SQL snapshot: 2,000 messages, 500
//! checkpoints and 200 proposed plans per thread; the 500 most recent activities plus pending
//! async questions and the worktree setup record.

use std::cmp::Ordering;
use std::collections::HashSet;

use serde_json::Value;
use zc_contracts::*;

use crate::event::OrchestrationEventExt;
use crate::support::{
    canonical_project_icon, compare_date_time_strings, is_imported_agent_session_message_id, keys_equal, legacy_linked_pull_request_of,
    legacy_thread_pull_request_key, locale_compare, KeySource, WORKTREE_SETUP_ACTIVITY_KIND,
};

pub const MAX_THREAD_MESSAGES: usize = 2_000;
pub const MAX_THREAD_CHECKPOINTS: usize = 500;
pub const MAX_THREAD_PROPOSED_PLANS: usize = 200;
pub const MAX_RECENT_THREAD_ACTIVITIES: usize = 500;

/// `createEmptyReadModel`.
pub fn create_empty_read_model(now_iso: impl Into<String>) -> OrchestrationReadModel {
    OrchestrationReadModel {
        snapshot_sequence: 0,
        projects: Vec::new(),
        threads: Vec::new(),
        updated_at: now_iso.into(),
    }
}

/// `array.slice(-n)`: the last `n` items.
fn keep_last<T>(items: &mut Vec<T>, n: usize) {
    if items.len() > n {
        items.drain(..items.len() - n);
    }
}

/// `retainThreadActivities`: the 500 most recent activities, plus async questions that are
/// still pending and the worktree setup record, whatever their age.
fn retain_thread_activities(activities: &mut Vec<OrchestrationThreadActivity>) {
    if activities.len() <= MAX_RECENT_THREAD_ACTIVITIES {
        return;
    }
    let recent_start = activities.len() - MAX_RECENT_THREAD_ACTIVITIES;
    // requestId → index of the pending question, in Map insertion order semantics.
    let mut pending: Vec<(String, usize)> = Vec::new();
    for (index, activity) in activities.iter().enumerate() {
        let Some(payload) = activity.payload.as_object() else { continue };
        let Some(request_id) = payload.get("requestId").and_then(Value::as_str) else {
            continue;
        };
        if activity.kind == "user-input.requested" && payload.get("responseMode").and_then(Value::as_str) == Some("message") {
            match pending.iter_mut().find(|(id, _)| id == request_id) {
                Some(entry) => entry.1 = index,
                None => pending.push((request_id.to_owned(), index)),
            }
        } else if activity.kind == "user-input.resolved" {
            pending.retain(|(id, _)| id != request_id);
        }
    }
    let pending_indexes: HashSet<usize> = pending.into_iter().map(|(_, index)| index).collect();
    let mut index = 0;
    activities.retain(|activity| {
        let keep = index >= recent_start || pending_indexes.contains(&index) || activity.kind == WORKTREE_SETUP_ACTIVITY_KIND;
        index += 1;
        keep
    });
}

/// `checkpointStatusToLatestTurnState`: a missing git ref is not an interruption.
fn checkpoint_status_to_latest_turn_state(status: OrchestrationCheckpointStatus) -> OrchestrationLatestTurnState {
    match status {
        OrchestrationCheckpointStatus::Error => OrchestrationLatestTurnState::Error,
        _ => OrchestrationLatestTurnState::Completed,
    }
}

/// `settledTurnStateForSessionStatus`: the state to settle a still-running latest turn with
/// when its session leaves "running", or `None` while it is (re)starting or running.
fn settled_turn_state_for_session_status(status: OrchestrationSessionStatus) -> Option<OrchestrationLatestTurnState> {
    match status {
        OrchestrationSessionStatus::Idle | OrchestrationSessionStatus::Ready => Some(OrchestrationLatestTurnState::Completed),
        OrchestrationSessionStatus::Error => Some(OrchestrationLatestTurnState::Error),
        OrchestrationSessionStatus::Interrupted | OrchestrationSessionStatus::Stopped => Some(OrchestrationLatestTurnState::Interrupted),
        OrchestrationSessionStatus::Starting | OrchestrationSessionStatus::Running => None,
    }
}

fn find_thread_mut<'a>(model: &'a mut OrchestrationReadModel, thread_id: &ThreadId) -> Option<&'a mut OrchestrationThread> {
    model.threads.iter_mut().find(|thread| &thread.id == thread_id)
}

fn find_thread_index(model: &OrchestrationReadModel, thread_id: &ThreadId) -> Option<usize> {
    model.threads.iter().position(|thread| &thread.id == thread_id)
}

fn project_identity<'a>(projects: &'a [OrchestrationProject], project_id: &ProjectId) -> Option<&'a RepositoryIdentity> {
    projects
        .iter()
        .find(|project| &project.id == project_id)
        .and_then(|project| project.repository_identity.as_ref())
        .and_then(Option::as_ref)
}

/// `pullRequestsPatch`: swaps the thread's links and re-derives the legacy single link.
fn apply_pull_requests(thread: &mut OrchestrationThread, pull_requests: Vec<ThreadPullRequestLink>, projects: &[OrchestrationProject]) {
    let linked = legacy_linked_pull_request_of(&pull_requests, &thread.project_id, project_identity(projects, &thread.project_id));
    thread.pull_requests = pull_requests;
    thread.linked_pull_request = Some(linked);
}

/// `upsertPullRequestLink`.
fn upsert_pull_request_link(pull_requests: &[ThreadPullRequestLink], link: ThreadPullRequestLink) -> Vec<ThreadPullRequestLink> {
    let index = pull_requests
        .iter()
        .position(|entry| keys_equal(KeySource::of_link(entry), KeySource::of_link(&link)));
    let mut next = pull_requests.to_vec();
    match index {
        Some(index) => next[index] = link,
        None => next.push(link),
    }
    next
}

/// `legacyPullRequestHost`: the project's canonical host, then the link URL's host.
fn legacy_pull_request_host(project: Option<&OrchestrationProject>, linked: &ThreadLinkedPullRequest) -> String {
    let canonical_host = project
        .and_then(|project| project.repository_identity.as_ref())
        .and_then(Option::as_ref)
        .and_then(|identity| identity.canonical_key.split('/').next())
        .filter(|host| !host.is_empty());
    if let Some(host) = canonical_host {
        return host.to_lowercase();
    }
    match url::Url::parse(&linked.url) {
        Ok(url) => url.host_str().unwrap_or("").to_lowercase(),
        Err(_) => "unknown".to_owned(),
    }
}

/// `legacyLinkToPullRequests`: the legacy field held one user-chosen link, so `null` clears
/// exactly the manual ones.
fn legacy_link_to_pull_requests(
    thread: &OrchestrationThread,
    project: Option<&OrchestrationProject>,
    linked: Option<&ThreadLinkedPullRequest>,
    linked_at: &str,
) -> Vec<ThreadPullRequestLink> {
    let without_manual: Vec<ThreadPullRequestLink> = thread
        .pull_requests
        .iter()
        .filter(|entry| entry.source != ThreadPullRequestLinkSource::Manual)
        .cloned()
        .collect();
    let Some(linked) = linked else { return without_manual };
    let key = legacy_thread_pull_request_key(linked, Some(&legacy_pull_request_host(project, linked)));
    upsert_pull_request_link(
        &without_manual,
        ThreadPullRequestLink {
            host: key.host,
            repository: key.repository,
            number: key.number,
            url: linked.url.clone(),
            source: ThreadPullRequestLinkSource::Manual,
            linked_at: linked_at.to_owned(),
            snapshot: None,
            stack: None,
        },
    )
}

fn compare_by_created_then_id(left_created: &str, left_id: &str, right_created: &str, right_id: &str) -> Ordering {
    compare_date_time_strings(left_created, right_created).then_with(|| locale_compare(left_id, right_id))
}

/// `retainThreadMessagesAfterRevert`.
fn retain_thread_messages_after_revert(messages: &[OrchestrationMessage], retained_turn_ids: &HashSet<String>, turn_count: i64) -> Vec<OrchestrationMessage> {
    let mut retained: HashSet<String> = HashSet::new();
    for message in messages {
        if message.role == OrchestrationMessageRole::System || is_imported_agent_session_message_id(message.id.as_str()) {
            retained.insert(message.id.0.clone());
            continue;
        }
        if let Some(turn_id) = &message.turn_id {
            if retained_turn_ids.contains(turn_id.as_str()) {
                retained.insert(message.id.0.clone());
            }
        }
    }

    for role in [OrchestrationMessageRole::User, OrchestrationMessageRole::Assistant] {
        let retained_count = messages
            .iter()
            .filter(|message| message.role == role && !is_imported_agent_session_message_id(message.id.as_str()) && retained.contains(message.id.as_str()))
            .count() as i64;
        let missing = (turn_count - retained_count).max(0) as usize;
        if missing == 0 {
            continue;
        }
        let mut fallback: Vec<&OrchestrationMessage> = messages
            .iter()
            .filter(|message| {
                message.role == role
                    && !retained.contains(message.id.as_str())
                    && message.turn_id.as_ref().is_none_or(|turn_id| retained_turn_ids.contains(turn_id.as_str()))
            })
            .collect();
        fallback.sort_by(|left, right| compare_by_created_then_id(&left.created_at, left.id.as_str(), &right.created_at, right.id.as_str()));
        for message in fallback.into_iter().take(missing) {
            retained.insert(message.id.0.clone());
        }
    }

    messages.iter().filter(|message| retained.contains(message.id.as_str())).cloned().collect()
}

/// `compareThreadActivities`: sequenced activities after unsequenced ones, by sequence; then
/// by `createdAt` and id.
fn compare_thread_activities(left: &OrchestrationThreadActivity, right: &OrchestrationThreadActivity) -> Ordering {
    match (left.sequence, right.sequence) {
        (Some(left), Some(right)) if left != right => return left.cmp(&right),
        (Some(_), None) => return Ordering::Greater,
        (None, Some(_)) => return Ordering::Less,
        _ => {}
    }
    locale_compare(&left.created_at, &right.created_at).then_with(|| locale_compare(left.id.as_str(), right.id.as_str()))
}

/// `[...items, item].toSorted(compare)`. When `items` is already sorted (the projector keeps
/// it so), a stable sort puts `item` after every element not greater than it: insert it there
/// instead of sorting everything again.
fn insert_sorted<T>(items: &mut Vec<T>, item: T, compare: impl Fn(&T, &T) -> Ordering) {
    let sorted = items.windows(2).all(|pair| compare(&pair[0], &pair[1]) != Ordering::Greater);
    if sorted {
        let position = items.partition_point(|entry| compare(entry, &item) != Ordering::Greater);
        items.insert(position, item);
    } else {
        items.push(item);
        items.sort_by(compare);
    }
}

fn new_thread(payload: &ThreadCreatedPayload) -> OrchestrationThread {
    OrchestrationThread {
        id: payload.thread_id.clone(),
        project_id: payload.project_id.clone(),
        title: payload.title.clone(),
        model_selection: payload.model_selection.clone(),
        runtime_mode: payload.runtime_mode,
        interaction_mode: payload.interaction_mode,
        branch: payload.branch.clone(),
        worktree_path: payload.worktree_path.clone(),
        linked_pull_request: None,
        pull_requests: Vec::new(),
        branch_pull_request: Some(None),
        latest_turn: None,
        created_at: payload.created_at.clone(),
        updated_at: payload.updated_at.clone(),
        archived_at: None,
        settled_override: None,
        settled_at: None,
        unsettled_at: Some(None),
        snoozed_until: Some(None),
        snoozed_at: Some(None),
        pinned_at: None,
        pin_order_key: None,
        active_order_key: Some(None),
        auto_settle_disabled_at: Some(None),
        title_regeneration: None,
        title_state: None,
        deleted_at: None,
        messages: Vec::new(),
        proposed_plans: Vec::new(),
        activities: Vec::new(),
        checkpoints: Vec::new(),
        session: None,
    }
}

/// `projectEvent`: applies one event to the model.
pub fn project_event(model: &mut OrchestrationReadModel, event: &OrchestrationEvent) {
    model.snapshot_sequence = event.sequence();
    model.updated_at = event.occurred_at().to_owned();
    let occurred_at = event.occurred_at();

    match event {
        OrchestrationEvent::ProjectCreated(e) => {
            let payload = &e.payload;
            let next = OrchestrationProject {
                id: payload.project_id.clone(),
                title: payload.title.clone(),
                workspace_root: payload.workspace_root.clone(),
                repository_identity: None,
                default_model_selection: payload.default_model_selection.clone(),
                default_thread_env_mode: Some(None),
                auto_pull: Some(false),
                favicon_path: Some(payload.favicon_path.clone().flatten()),
                project_icon: Some(payload.project_icon.clone().flatten().as_ref().map(canonical_project_icon)),
                scripts: payload.scripts.clone(),
                created_at: payload.created_at.clone(),
                updated_at: payload.updated_at.clone(),
                deleted_at: None,
            };
            match model.projects.iter_mut().find(|project| project.id == payload.project_id) {
                Some(existing) => *existing = next,
                None => model.projects.push(next),
            }
        }

        OrchestrationEvent::ProjectMetaUpdated(e) => {
            let payload = &e.payload;
            for project in model.projects.iter_mut().filter(|project| project.id == payload.project_id) {
                if let Some(title) = &payload.title {
                    project.title = title.clone();
                }
                if let Some(workspace_root) = &payload.workspace_root {
                    project.workspace_root = workspace_root.clone();
                }
                if let Some(selection) = &payload.default_model_selection {
                    project.default_model_selection = selection.clone();
                }
                if let Some(mode) = &payload.default_thread_env_mode {
                    project.default_thread_env_mode = Some(*mode);
                }
                if let Some(auto_pull) = payload.auto_pull {
                    project.auto_pull = Some(auto_pull);
                }
                if let Some(favicon_path) = &payload.favicon_path {
                    project.favicon_path = Some(favicon_path.clone());
                }
                if let Some(project_icon) = &payload.project_icon {
                    project.project_icon = Some(project_icon.as_ref().map(canonical_project_icon));
                }
                if let Some(scripts) = &payload.scripts {
                    project.scripts = scripts.clone();
                }
                project.updated_at = payload.updated_at.clone();
            }
        }

        OrchestrationEvent::ProjectDeleted(e) => {
            let payload = &e.payload;
            for project in model.projects.iter_mut().filter(|project| project.id == payload.project_id) {
                project.deleted_at = Some(payload.deleted_at.clone());
                project.updated_at = payload.deleted_at.clone();
            }
        }

        OrchestrationEvent::ThreadCreated(e) => {
            let thread = new_thread(&e.payload);
            match model.threads.iter_mut().find(|entry| entry.id == thread.id) {
                Some(existing) => *existing = thread,
                None => model.threads.push(thread),
            }
        }

        OrchestrationEvent::ThreadDeleted(e) => {
            if let Some(thread) = find_thread_mut(model, &e.payload.thread_id) {
                thread.deleted_at = Some(e.payload.deleted_at.clone());
                thread.updated_at = e.payload.deleted_at.clone();
            }
        }

        OrchestrationEvent::ThreadArchived(e) => {
            if let Some(thread) = find_thread_mut(model, &e.payload.thread_id) {
                thread.archived_at = Some(e.payload.archived_at.clone());
                thread.title_regeneration = Some(None);
                thread.updated_at = e.payload.updated_at.clone();
            }
        }

        OrchestrationEvent::ThreadUnarchived(e) => {
            if let Some(thread) = find_thread_mut(model, &e.payload.thread_id) {
                thread.archived_at = None;
                thread.updated_at = e.payload.updated_at.clone();
            }
        }

        OrchestrationEvent::ThreadSettled(e) => {
            if let Some(thread) = find_thread_mut(model, &e.payload.thread_id) {
                thread.settled_override = Some(OrchestrationThreadSettledOverride::Settled);
                thread.settled_at = Some(e.payload.settled_at.clone());
                thread.unsettled_at = Some(None);
                thread.active_order_key = Some(None);
                thread.updated_at = e.payload.updated_at.clone();
            }
        }

        OrchestrationEvent::ThreadUnsettled(e) => {
            let payload = &e.payload;
            if let Some(thread) = find_thread_mut(model, &payload.thread_id) {
                // A thread already pinned active keeps its re-entry stamp: the activity reset
                // that clears the pin is not a re-entry.
                let unsettled_at = if thread.settled_override == Some(OrchestrationThreadSettledOverride::Active) {
                    thread.unsettled_at.clone().flatten()
                } else {
                    Some(payload.updated_at.clone())
                };
                thread.settled_override = match payload.reason {
                    ThreadUnsettledPayloadReason::User => Some(OrchestrationThreadSettledOverride::Active),
                    ThreadUnsettledPayloadReason::Activity => None,
                };
                thread.settled_at = None;
                thread.unsettled_at = Some(unsettled_at);
                thread.updated_at = payload.updated_at.clone();
            }
        }

        OrchestrationEvent::ThreadSnoozed(e) => {
            if let Some(thread) = find_thread_mut(model, &e.payload.thread_id) {
                thread.snoozed_until = Some(Some(e.payload.snoozed_until.clone()));
                thread.snoozed_at = Some(Some(e.payload.snoozed_at.clone()));
                thread.updated_at = e.payload.updated_at.clone();
            }
        }

        OrchestrationEvent::ThreadUnsnoozed(e) => {
            if let Some(thread) = find_thread_mut(model, &e.payload.thread_id) {
                thread.snoozed_until = Some(None);
                thread.snoozed_at = Some(None);
                thread.updated_at = e.payload.updated_at.clone();
            }
        }

        OrchestrationEvent::ThreadPinned(e) => {
            if let Some(thread) = find_thread_mut(model, &e.payload.thread_id) {
                thread.pinned_at = Some(Some(e.payload.pinned_at.clone()));
                if let Some(order_key) = &e.payload.pin_order_key {
                    thread.pin_order_key = Some(Some(order_key.clone()));
                }
                thread.updated_at = e.payload.updated_at.clone();
            }
        }

        OrchestrationEvent::ThreadUnpinned(e) => {
            if let Some(thread) = find_thread_mut(model, &e.payload.thread_id) {
                thread.pinned_at = Some(None);
                // Re-pinning is "pin again", not "restore an ancient position".
                thread.pin_order_key = Some(None);
                thread.updated_at = e.payload.updated_at.clone();
            }
        }

        OrchestrationEvent::ThreadAutoSettleSet(e) => {
            if let Some(thread) = find_thread_mut(model, &e.payload.thread_id) {
                thread.auto_settle_disabled_at = Some(e.payload.auto_settle_disabled_at.clone());
                thread.updated_at = e.payload.updated_at.clone();
            }
        }

        OrchestrationEvent::ThreadPinReordered(e) => {
            if let Some(thread) = find_thread_mut(model, &e.payload.thread_id) {
                thread.pin_order_key = Some(Some(e.payload.order_key.clone()));
                thread.updated_at = e.payload.updated_at.clone();
            }
        }

        OrchestrationEvent::ThreadMetaUpdated(e) => {
            let payload = &e.payload;
            let Some(index) = find_thread_index(model, &payload.thread_id) else { return };
            // Legacy single-link events replay into the link array so the derived
            // linkedPullRequest and pullRequests never disagree.
            let legacy_links = payload.linked_pull_request.as_ref().map(|linked| {
                let thread = &model.threads[index];
                let project = model.projects.iter().find(|project| project.id == thread.project_id);
                legacy_link_to_pull_requests(thread, project, linked.as_ref(), &payload.updated_at)
            });
            let projects = std::mem::take(&mut model.projects);
            let thread = &mut model.threads[index];
            if let Some(title) = &payload.title {
                thread.title = title.clone();
            }
            if let Some(title_state) = &payload.title_state {
                thread.title_state = Some(title_state.clone());
            }
            if let Some(title_regeneration) = &payload.title_regeneration {
                thread.title_regeneration = Some(title_regeneration.clone());
            }
            if let Some(model_selection) = &payload.model_selection {
                thread.model_selection = model_selection.clone();
            }
            if let Some(branch) = &payload.branch {
                thread.branch = branch.clone();
            }
            if let Some(worktree_path) = &payload.worktree_path {
                thread.worktree_path = worktree_path.clone();
            }
            if let Some(active_order_key) = &payload.active_order_key {
                thread.active_order_key = Some(active_order_key.clone());
            }
            if let Some(branch_pull_request) = &payload.branch_pull_request {
                thread.branch_pull_request = Some(branch_pull_request.clone());
            }
            if let Some(links) = legacy_links {
                apply_pull_requests(thread, links, &projects);
            }
            thread.updated_at = payload.updated_at.clone();
            model.projects = projects;
        }

        OrchestrationEvent::ThreadPullRequestLinked(e) => {
            let payload = &e.payload;
            let projects = std::mem::take(&mut model.projects);
            if let Some(thread) = find_thread_mut(model, &payload.thread_id) {
                let links = upsert_pull_request_link(&thread.pull_requests, payload.link.clone());
                apply_pull_requests(thread, links, &projects);
                thread.updated_at = payload.updated_at.clone();
            }
            model.projects = projects;
        }

        OrchestrationEvent::ThreadPullRequestUnlinked(e) => {
            let payload = &e.payload;
            let projects = std::mem::take(&mut model.projects);
            if let Some(thread) = find_thread_mut(model, &payload.thread_id) {
                let key = KeySource {
                    host: &payload.host,
                    repository: &payload.repository,
                    number: payload.number,
                    url: None,
                };
                let links: Vec<ThreadPullRequestLink> = thread
                    .pull_requests
                    .iter()
                    .filter(|entry| !keys_equal(KeySource::of_link(entry), key))
                    .cloned()
                    .collect();
                apply_pull_requests(thread, links, &projects);
                thread.updated_at = payload.updated_at.clone();
            }
            model.projects = projects;
        }

        OrchestrationEvent::ThreadPullRequestSynced(e) => {
            let payload = &e.payload;
            let key = KeySource {
                host: &payload.host,
                repository: &payload.repository,
                number: payload.number,
                url: None,
            };
            let projects = std::mem::take(&mut model.projects);
            if let Some(thread) = find_thread_mut(model, &payload.thread_id) {
                // A sync for a link the user removed in the meantime is stale; drop it.
                if thread.pull_requests.iter().any(|link| keys_equal(KeySource::of_link(link), key)) {
                    let links: Vec<ThreadPullRequestLink> = thread
                        .pull_requests
                        .iter()
                        .map(|link| {
                            if keys_equal(KeySource::of_link(link), key) {
                                ThreadPullRequestLink {
                                    snapshot: Some(payload.snapshot.clone()),
                                    stack: payload.stack.clone(),
                                    ..link.clone()
                                }
                            } else {
                                link.clone()
                            }
                        })
                        .collect();
                    apply_pull_requests(thread, links, &projects);
                    thread.updated_at = payload.updated_at.clone();
                }
            }
            model.projects = projects;
        }

        OrchestrationEvent::ThreadRuntimeModeSet(e) => {
            if let Some(thread) = find_thread_mut(model, &e.payload.thread_id) {
                thread.runtime_mode = e.payload.runtime_mode;
                thread.updated_at = e.payload.updated_at.clone();
            }
        }

        OrchestrationEvent::ThreadInteractionModeSet(e) => {
            if let Some(thread) = find_thread_mut(model, &e.payload.thread_id) {
                thread.interaction_mode = e.payload.interaction_mode;
                thread.updated_at = e.payload.updated_at.clone();
            }
        }

        OrchestrationEvent::ThreadMessageSent(e) => {
            let payload = &e.payload;
            let Some(thread) = find_thread_mut(model, &payload.thread_id) else { return };
            let turn_id = payload.turn_id.clone().map(TurnId::from);
            match thread.messages.iter_mut().find(|entry| entry.id == payload.message_id) {
                Some(entry) => {
                    if payload.streaming {
                        entry.text.push_str(&payload.text);
                    } else if !payload.text.is_empty() {
                        entry.text = payload.text.clone();
                    }
                    entry.streaming = payload.streaming;
                    entry.updated_at = payload.updated_at.clone();
                    entry.turn_id = turn_id;
                    if let Some(attachments) = &payload.attachments {
                        entry.attachments = Some(attachments.clone());
                    }
                    if let Some(context) = &payload.context {
                        entry.context = Some(context.clone());
                    }
                }
                None => thread.messages.push(OrchestrationMessage {
                    id: payload.message_id.clone(),
                    role: payload.role,
                    text: payload.text.clone(),
                    attachments: payload.attachments.clone(),
                    context: payload.context.clone(),
                    turn_id,
                    streaming: payload.streaming,
                    created_at: payload.created_at.clone(),
                    updated_at: payload.updated_at.clone(),
                }),
            }
            keep_last(&mut thread.messages, MAX_THREAD_MESSAGES);
            thread.updated_at = occurred_at.to_owned();
        }

        OrchestrationEvent::ThreadSessionSet(e) => {
            let payload = &e.payload;
            let Some(thread) = find_thread_mut(model, &payload.thread_id) else { return };
            let session = payload.session.clone();
            // Leaving "running" is the turn-end signal: settle a still-running latest turn so
            // its duration covers the whole turn.
            let settled_state = settled_turn_state_for_session_status(session.status);
            let latest_turn = match (&session.active_turn_id, &thread.latest_turn) {
                (Some(active_turn_id), latest) if session.status == OrchestrationSessionStatus::Running => {
                    let same = latest.as_ref().filter(|turn| &turn.turn_id == active_turn_id);
                    Some(OrchestrationLatestTurn {
                        turn_id: active_turn_id.clone(),
                        state: OrchestrationLatestTurnState::Running,
                        requested_at: same.map_or_else(|| session.updated_at.clone(), |turn| turn.requested_at.clone()),
                        started_at: Some(same.map_or_else(
                            || session.updated_at.clone(),
                            |turn| turn.started_at.clone().unwrap_or_else(|| session.updated_at.clone()),
                        )),
                        completed_at: None,
                        assistant_message_id: same.and_then(|turn| turn.assistant_message_id.clone()),
                        source_proposed_plan: None,
                    })
                }
                (_, Some(latest)) if latest.state == OrchestrationLatestTurnState::Running && settled_state.is_some() => {
                    Some(OrchestrationLatestTurn {
                        state: settled_state.expect("checked"),
                        // The session leaving "running" is the authoritative turn end.
                        completed_at: Some(session.updated_at.clone()),
                        ..latest.clone()
                    })
                }
                (_, latest) => latest.clone(),
            };
            thread.session = Some(session);
            thread.latest_turn = latest_turn;
            thread.updated_at = occurred_at.to_owned();
        }

        OrchestrationEvent::ThreadProposedPlanUpserted(e) => {
            let payload = &e.payload;
            let Some(thread) = find_thread_mut(model, &payload.thread_id) else { return };
            thread.proposed_plans.retain(|entry| entry.id != payload.proposed_plan.id);
            thread.proposed_plans.push(payload.proposed_plan.clone());
            thread
                .proposed_plans
                .sort_by(|left, right| locale_compare(&left.created_at, &right.created_at).then_with(|| locale_compare(&left.id, &right.id)));
            keep_last(&mut thread.proposed_plans, MAX_THREAD_PROPOSED_PLANS);
            thread.updated_at = occurred_at.to_owned();
        }

        OrchestrationEvent::ThreadTurnDiffCompleted(e) => {
            let payload = &e.payload;
            let Some(thread) = find_thread_mut(model, &payload.thread_id) else { return };
            let checkpoint = OrchestrationCheckpointSummary {
                turn_id: payload.turn_id.clone(),
                checkpoint_turn_count: payload.checkpoint_turn_count,
                checkpoint_ref: payload.checkpoint_ref.clone(),
                status: payload.status,
                files: payload.files.clone(),
                assistant_message_id: payload.assistant_message_id.clone(),
                completed_at: payload.completed_at.clone(),
            };
            // A placeholder ("missing") never overwrites a captured checkpoint.
            let existing = thread.checkpoints.iter().find(|entry| entry.turn_id == checkpoint.turn_id);
            if existing.is_some_and(|existing| existing.status != OrchestrationCheckpointStatus::Missing)
                && checkpoint.status == OrchestrationCheckpointStatus::Missing
            {
                return;
            }
            thread.checkpoints.retain(|entry| entry.turn_id != checkpoint.turn_id);
            thread.checkpoints.push(checkpoint);
            thread.checkpoints.sort_by_key(|entry| entry.checkpoint_turn_count);
            keep_last(&mut thread.checkpoints, MAX_THREAD_CHECKPOINTS);

            // Mid-turn diff updates produce placeholders; do not settle a turn its session is
            // still running.
            let turn_still_running = thread
                .session
                .as_ref()
                .is_some_and(|session| session.status == OrchestrationSessionStatus::Running && session.active_turn_id.as_ref() == Some(&payload.turn_id));
            if !turn_still_running {
                let same = thread.latest_turn.as_ref().filter(|turn| turn.turn_id == payload.turn_id);
                thread.latest_turn = Some(OrchestrationLatestTurn {
                    turn_id: payload.turn_id.clone(),
                    state: if same.is_some_and(|turn| turn.state == OrchestrationLatestTurnState::Interrupted) {
                        OrchestrationLatestTurnState::Interrupted
                    } else {
                        checkpoint_status_to_latest_turn_state(payload.status)
                    },
                    requested_at: same.map_or_else(|| payload.completed_at.clone(), |turn| turn.requested_at.clone()),
                    started_at: Some(same.map_or_else(
                        || payload.completed_at.clone(),
                        |turn| turn.started_at.clone().unwrap_or_else(|| payload.completed_at.clone()),
                    )),
                    completed_at: Some(payload.completed_at.clone()),
                    assistant_message_id: payload.assistant_message_id.clone(),
                    source_proposed_plan: None,
                });
            }
            thread.updated_at = occurred_at.to_owned();
        }

        OrchestrationEvent::ThreadReverted(e) => {
            let payload = &e.payload;
            let Some(thread) = find_thread_mut(model, &payload.thread_id) else { return };
            let mut checkpoints: Vec<OrchestrationCheckpointSummary> = thread
                .checkpoints
                .iter()
                .filter(|entry| entry.checkpoint_turn_count <= payload.turn_count)
                .cloned()
                .collect();
            checkpoints.sort_by_key(|entry| entry.checkpoint_turn_count);
            keep_last(&mut checkpoints, MAX_THREAD_CHECKPOINTS);
            let retained_turn_ids: HashSet<String> = checkpoints.iter().map(|checkpoint| checkpoint.turn_id.0.clone()).collect();
            let mut messages = retain_thread_messages_after_revert(&thread.messages, &retained_turn_ids, payload.turn_count);
            keep_last(&mut messages, MAX_THREAD_MESSAGES);
            let retained = |turn_id: &Option<TurnId>| turn_id.as_ref().is_none_or(|turn_id| retained_turn_ids.contains(turn_id.as_str()));
            thread.proposed_plans.retain(|plan| retained(&plan.turn_id));
            keep_last(&mut thread.proposed_plans, MAX_THREAD_PROPOSED_PLANS);
            thread.activities.retain(|activity| retained(&activity.turn_id));
            thread.latest_turn = checkpoints.last().map(|latest| OrchestrationLatestTurn {
                turn_id: latest.turn_id.clone(),
                state: checkpoint_status_to_latest_turn_state(latest.status),
                requested_at: latest.completed_at.clone(),
                started_at: Some(latest.completed_at.clone()),
                completed_at: Some(latest.completed_at.clone()),
                assistant_message_id: latest.assistant_message_id.clone(),
                source_proposed_plan: None,
            });
            thread.checkpoints = checkpoints;
            thread.messages = messages;
            thread.updated_at = occurred_at.to_owned();
        }

        OrchestrationEvent::ThreadActivityAppended(e) => {
            let payload = &e.payload;
            let Some(thread) = find_thread_mut(model, &payload.thread_id) else { return };
            thread.activities.retain(|entry| entry.id != payload.activity.id);
            insert_sorted(&mut thread.activities, payload.activity.clone(), compare_thread_activities);
            retain_thread_activities(&mut thread.activities);
            thread.updated_at = occurred_at.to_owned();
        }

        OrchestrationEvent::ThreadTurnStartRequested(_)
        | OrchestrationEvent::ThreadTurnInterruptRequested(_)
        | OrchestrationEvent::ThreadApprovalResponseRequested(_)
        | OrchestrationEvent::ThreadUserInputResponseRequested(_)
        | OrchestrationEvent::ThreadCheckpointRevertRequested(_)
        | OrchestrationEvent::ThreadSessionStopRequested(_) => {}
    }
}

/// Folds `events` into `model` in order.
pub fn project_events<'a>(model: &mut OrchestrationReadModel, events: impl IntoIterator<Item = &'a OrchestrationEvent>) {
    for event in events {
        project_event(model, event);
    }
}
