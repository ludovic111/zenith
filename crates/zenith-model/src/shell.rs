//! The shell: every project and a summary of every thread (`orchestration.subscribeShell`),
//! and the sidebar built from it.
//!
//! The sidebar is one flat list in four sections, not a tree of projects
//! (`apps/web/src/components/Sidebar.logic.ts`): archived threads are left out, then a thread
//! is **snoozed** while its wake time is ahead and it has not raised its hand, else
//! **settled**, else **pinned**, else **active**. Orders come from
//! `client-runtime/src/state/threadSort.ts`, snooze rules from `threadSettled.ts`.

use serde_json::Value;
use zc_contracts::{
    OrchestrationLatestTurnState, OrchestrationProjectShell, OrchestrationSessionStatus, OrchestrationShellStreamEvent, OrchestrationShellStreamItem,
    OrchestrationThreadShell, OrchestrationThreadShellBackgroundLiveness, OrchestrationThreadShellSettledOverride, ProjectId, ThreadId,
};

use crate::time::millis;

/// The shell as the client knows it.
#[derive(Clone, Debug, Default)]
pub struct Shell {
    pub projects: Vec<OrchestrationProjectShell>,
    pub threads: Vec<OrchestrationThreadShell>,
    /// The last event applied (`snapshotSequence`, then each event's).
    pub sequence: i64,
    /// A snapshot arrived.
    pub loaded: bool,
    /// The stream caught up (`synchronized`).
    pub live: bool,
}

impl Shell {
    /// Applies one stream item. Events at or before the current sequence are ignored.
    pub fn apply(&mut self, item: Value) -> Result<(), String> {
        let item: OrchestrationShellStreamItem = serde_json::from_value(item).map_err(|e| format!("shell item: {e}"))?;
        match item {
            OrchestrationShellStreamItem::Synchronized(_) => self.live = true,
            OrchestrationShellStreamItem::Snapshot(snapshot) => {
                let snapshot = snapshot.snapshot;
                self.projects = snapshot.projects;
                self.threads = snapshot.threads;
                self.sequence = snapshot.snapshot_sequence;
                self.loaded = true;
            }
            OrchestrationShellStreamItem::OrchestrationShellStreamEvent(event) => {
                let sequence: i64 = match &event {
                    OrchestrationShellStreamEvent::ProjectUpserted(e) => e.sequence,
                    OrchestrationShellStreamEvent::ProjectRemoved(e) => e.sequence,
                    OrchestrationShellStreamEvent::ThreadUpserted(e) => e.sequence,
                    OrchestrationShellStreamEvent::ThreadRemoved(e) => e.sequence,
                };
                if sequence <= self.sequence {
                    return Ok(());
                }
                self.sequence = sequence;
                match event {
                    OrchestrationShellStreamEvent::ProjectUpserted(e) => upsert(&mut self.projects, e.project, |p| p.id.clone()),
                    OrchestrationShellStreamEvent::ProjectRemoved(e) => self.projects.retain(|p| p.id != e.project_id),
                    OrchestrationShellStreamEvent::ThreadUpserted(e) => upsert(&mut self.threads, e.thread, |t| t.id.clone()),
                    OrchestrationShellStreamEvent::ThreadRemoved(e) => self.threads.retain(|t| t.id != e.thread_id),
                }
            }
        }
        Ok(())
    }

    pub fn project(&self, id: &ProjectId) -> Option<&OrchestrationProjectShell> {
        self.projects.iter().find(|p| &p.id == id)
    }

    pub fn thread(&self, id: &ThreadId) -> Option<&OrchestrationThreadShell> {
        self.threads.iter().find(|t| &t.id == id)
    }

    /// Projects in the order the sidebar's project picker shows them (by title).
    pub fn projects_sorted(&self) -> Vec<&OrchestrationProjectShell> {
        let mut projects: Vec<_> = self.projects.iter().collect();
        projects.sort_by_key(|p| p.title.to_lowercase());
        projects
    }

    /// The sidebar: its sections in display order (empty ones left out), each sorted.
    /// `project` limits it to one project, `query` to titles and branches containing it.
    pub fn sidebar(&self, project: Option<&ProjectId>, query: &str, now_ms: i64) -> Vec<(Section, Vec<&OrchestrationThreadShell>)> {
        let query = query.trim().to_lowercase();
        let mut buckets: [Vec<&OrchestrationThreadShell>; 4] = Default::default();
        for thread in &self.threads {
            if thread.archived_at.is_some() {
                continue;
            }
            if project.is_some_and(|p| &thread.project_id != p) {
                continue;
            }
            if !query.is_empty() && !matches_query(thread, &query) {
                continue;
            }
            buckets[section(thread, now_ms) as usize].push(thread);
        }
        let [mut pinned, mut active, mut snoozed, mut settled] = buckets;
        sort_pinned(&mut pinned);
        sort_active(&mut active);
        snoozed.sort_by(|a, b| {
            let wake = |t: &OrchestrationThreadShell| t.snoozed_until.clone().flatten().as_deref().and_then(millis).unwrap_or(0);
            wake(a).cmp(&wake(b)).then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        sort_settled(&mut settled);
        [
            (Section::Pinned, pinned),
            (Section::Active, active),
            (Section::Snoozed, snoozed),
            (Section::Settled, settled),
        ]
        .into_iter()
        .filter(|(_, threads)| !threads.is_empty())
        .collect()
    }

    /// Archived threads, newest archived first.
    pub fn archived(&self) -> Vec<&OrchestrationThreadShell> {
        let mut archived: Vec<_> = self.threads.iter().filter(|t| t.archived_at.is_some()).collect();
        archived.sort_by(|a, b| b.archived_at.cmp(&a.archived_at));
        archived
    }
}

fn upsert<T, K: PartialEq>(items: &mut Vec<T>, item: T, key: impl Fn(&T) -> K) {
    let k = key(&item);
    match items.iter_mut().find(|existing| key(existing) == k) {
        Some(existing) => *existing = item,
        None => items.push(item),
    }
}

fn matches_query(thread: &OrchestrationThreadShell, query: &str) -> bool {
    thread.title.to_lowercase().contains(query)
        || thread.branch.as_deref().is_some_and(|b| b.to_lowercase().contains(query))
        || thread.pull_requests.iter().any(|pr| format!("#{}", pr.number).contains(query))
}

/// A sidebar section, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    Pinned = 0,
    Active = 1,
    Snoozed = 2,
    Settled = 3,
}

impl Section {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pinned => "Pinned",
            Self::Active => "Active",
            Self::Snoozed => "Snoozed",
            Self::Settled => "Settled",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pinned => "pinned",
            Self::Active => "active",
            Self::Snoozed => "snoozed",
            Self::Settled => "settled",
        }
    }
}

/// Which section a (not archived) thread goes in.
pub fn section(thread: &OrchestrationThreadShell, now_ms: i64) -> Section {
    if effective_snoozed(thread, now_ms) {
        Section::Snoozed
    } else if thread.settled_override == Some(OrchestrationThreadShellSettledOverride::Settled) {
        Section::Settled
    } else if thread.pinned_at.clone().flatten().is_some() {
        Section::Pinned
    } else {
        Section::Active
    }
}

/// A snoozed thread shows again when it needs the person: a pending request, a failure newer
/// than the snooze, or a turn completed after it (`threadRaisedHandWhileSnoozed`).
pub fn raised_hand_while_snoozed(thread: &OrchestrationThreadShell) -> bool {
    if thread.has_pending_approvals || thread.has_pending_user_input {
        return true;
    }
    let snoozed_at = thread.snoozed_at.clone().flatten().as_deref().and_then(millis);
    if let Some(session) = &thread.session {
        if session.status == OrchestrationSessionStatus::Error {
            let failed_at = millis(&session.updated_at);
            if snoozed_at.is_none() || failed_at > snoozed_at {
                return true;
            }
        }
    }
    if let (Some(snoozed_at), Some(turn)) = (snoozed_at, &thread.latest_turn) {
        if turn.state == OrchestrationLatestTurnState::Completed {
            if let Some(completed) = turn.completed_at.as_deref().and_then(millis) {
                if completed > snoozed_at {
                    return true;
                }
            }
        }
    }
    false
}

/// Hidden from the inbox while the wake time is ahead and the thread has not raised its hand.
pub fn effective_snoozed(thread: &OrchestrationThreadShell, now_ms: i64) -> bool {
    let Some(wake) = thread.snoozed_until.clone().flatten() else {
        return false;
    };
    match millis(&wake) {
        Some(wake) if wake > now_ms => !raised_hand_while_snoozed(thread),
        _ => false,
    }
}

/// A thread may be snoozed unless the agent waits on the person.
pub fn can_snooze(thread: &OrchestrationThreadShell) -> bool {
    !(thread.has_pending_approvals || thread.has_pending_user_input)
}

/// What a thread is doing, by priority (`resolveSidebarThreadStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadStatus {
    /// Waits for an approval.
    Approval,
    /// Waits for an answer.
    Input,
    Working,
    Failed,
    /// Background work (a monitor, a subagent) while the session is idle.
    Monitoring,
    /// A plan waits to be implemented.
    PlanReady,
    Ready,
}

impl ThreadStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Approval => "Approval",
            Self::Input => "Input",
            Self::Working => "Working",
            Self::Failed => "Failed",
            Self::Monitoring => "Monitoring",
            Self::PlanReady => "Plan ready",
            Self::Ready => "Ready",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Input => "input",
            Self::Working => "working",
            Self::Failed => "failed",
            Self::Monitoring => "monitoring",
            Self::PlanReady => "plan-ready",
            Self::Ready => "ready",
        }
    }

    /// The person has something to do.
    pub fn needs_attention(self) -> bool {
        matches!(self, Self::Approval | Self::Input | Self::Failed | Self::PlanReady)
    }
}

pub fn status(thread: &OrchestrationThreadShell) -> ThreadStatus {
    if thread.has_pending_approvals {
        return ThreadStatus::Approval;
    }
    if thread.has_pending_user_input {
        return ThreadStatus::Input;
    }
    match thread.session.as_ref().map(|s| s.status) {
        Some(OrchestrationSessionStatus::Running | OrchestrationSessionStatus::Starting) => return ThreadStatus::Working,
        Some(OrchestrationSessionStatus::Error) => return ThreadStatus::Failed,
        _ => {}
    }
    match thread.background_liveness.flatten() {
        Some(OrchestrationThreadShellBackgroundLiveness::Working) => return ThreadStatus::Working,
        Some(OrchestrationThreadShellBackgroundLiveness::Monitoring) => return ThreadStatus::Monitoring,
        None => {}
    }
    if thread.has_actionable_proposed_plan {
        return ThreadStatus::PlanReady;
    }
    ThreadStatus::Ready
}

/// When the thread's running turn started (for "Working 2m 05s").
pub fn working_since(thread: &OrchestrationThreadShell) -> Option<i64> {
    let turn = thread.latest_turn.as_ref()?;
    if turn.state != OrchestrationLatestTurnState::Running {
        return None;
    }
    turn.started_at.as_deref().or(Some(turn.requested_at.as_str())).and_then(millis)
}

/// The time the thread last did something, for the sidebar's age.
pub fn activity_at(thread: &OrchestrationThreadShell) -> i64 {
    [
        thread.latest_turn.as_ref().and_then(|t| t.completed_at.clone()),
        thread.latest_turn.as_ref().map(|t| t.requested_at.clone()),
        thread.latest_user_message_at.clone(),
        Some(thread.created_at.clone()),
    ]
    .into_iter()
    .flatten()
    .filter_map(|t| millis(&t))
    .max()
    .unwrap_or(0)
}

/// Pinned: arranged keys first (string order), then keyless threads newest created first.
pub fn sort_pinned(threads: &mut [&OrchestrationThreadShell]) {
    threads.sort_by(|a, b| {
        let ka = a.pin_order_key.clone().flatten();
        let kb = b.pin_order_key.clone().flatten();
        match (ka, kb) {
            (Some(ka), Some(kb)) => ka.cmp(&kb).then_with(|| a.id.as_str().cmp(b.id.as_str())),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => millis(&b.created_at).cmp(&millis(&a.created_at)).then_with(|| a.id.as_str().cmp(b.id.as_str())),
        }
    });
}

/// Active: new and reopened threads (no key) lead, newest `max(createdAt, unsettledAt)` first;
/// arranged threads follow their keys.
pub fn sort_active(threads: &mut [&OrchestrationThreadShell]) {
    let anchor = |t: &OrchestrationThreadShell| {
        let created = millis(&t.created_at).unwrap_or(0);
        let unsettled = t.unsettled_at.clone().flatten().as_deref().and_then(millis).unwrap_or(0);
        created.max(unsettled)
    };
    threads.sort_by(|a, b| {
        let ka = a.active_order_key.clone().flatten();
        let kb = b.active_order_key.clone().flatten();
        let order = match (&ka, &kb) {
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
            (Some(ka), Some(kb)) => ka.cmp(kb),
            (None, None) => anchor(b).cmp(&anchor(a)),
        };
        order.then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
}

/// Settled: by when the work ended, newest first.
pub fn sort_settled(threads: &mut [&OrchestrationThreadShell]) {
    let ended = |t: &OrchestrationThreadShell| {
        t.settled_at
            .as_deref()
            .and_then(millis)
            .or_else(|| {
                [
                    t.latest_turn.as_ref().and_then(|turn| turn.completed_at.clone()),
                    t.latest_user_message_at.clone(),
                ]
                .into_iter()
                .flatten()
                .filter_map(|s| millis(&s))
                .max()
            })
            .or_else(|| millis(&t.updated_at))
            .unwrap_or(0)
    };
    threads.sort_by(|a, b| ended(b).cmp(&ended(a)).then_with(|| a.id.as_str().cmp(b.id.as_str())));
}

const ORDER_DIGITS: &[u8] = b"abcdefghijklmnopqrstuvwxyz";

fn valid_order_key(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|c| c.is_ascii_lowercase()) && !key.ends_with('a')
}

fn order_midpoint(a: &[u8], b: &[u8]) -> Vec<u8> {
    if !b.is_empty() {
        let mut n = 0;
        while n < b.len() && a.get(n).copied().unwrap_or(ORDER_DIGITS[0]) == b[n] {
            n += 1;
        }
        if n > 0 {
            let mut out = b[..n].to_vec();
            out.extend(order_midpoint(a.get(n..).unwrap_or(&[]), &b[n..]));
            return out;
        }
    }
    let digit = |c: u8| (c - b'a') as usize;
    let da = a.first().map(|c| digit(*c)).unwrap_or(0);
    let db = b.first().map(|c| digit(*c)).unwrap_or(ORDER_DIGITS.len());
    if db - da > 1 {
        // Math.round of the half: .5 rounds up.
        return vec![ORDER_DIGITS[(da + db).div_ceil(2)]];
    }
    if b.len() > 1 {
        return vec![b[0]];
    }
    let mut out = vec![ORDER_DIGITS[da]];
    out.extend(order_midpoint(a.get(1..).unwrap_or(&[]), &[]));
    out
}

/// A pin/active order key strictly between two neighbors (`pinOrderKeyBetween`: digits
/// a–z read as a fraction, never ending in "a"). `None` bounds are the open ends; `None` back
/// when the existing keys are corrupt or out of order.
pub fn order_key_between(before: Option<&str>, after: Option<&str>) -> Option<String> {
    let a = before.unwrap_or("");
    let b = after.unwrap_or("");
    if (!a.is_empty() && !valid_order_key(a)) || (!b.is_empty() && !valid_order_key(b)) {
        return None;
    }
    if !b.is_empty() && a >= b {
        return None;
    }
    String::from_utf8(order_midpoint(a.as_bytes(), b.as_bytes())).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn thread(id: &str, extra: Value) -> Value {
        let mut base = json!({
            "id": id, "projectId": "p1", "title": format!("Thread {id}"),
            "modelSelection": {"instanceId": "claudeAgent", "model": "made-up-model"},
            "runtimeMode": "full-access", "interactionMode": "default",
            "branch": null, "worktreePath": null, "pullRequests": [], "latestTurn": null,
            "createdAt": "2026-10-01T10:00:00.000Z", "updatedAt": "2026-10-01T10:00:00.000Z",
            "archivedAt": null, "settledAt": null, "session": null, "latestUserMessageAt": null,
            "hasPendingApprovals": false, "hasPendingUserInput": false, "hasActionableProposedPlan": false
        });
        for (k, v) in extra.as_object().unwrap() {
            base[k] = v.clone();
        }
        base
    }

    fn shell(threads: Vec<Value>) -> Shell {
        let mut shell = Shell::default();
        shell
            .apply(json!({"kind": "snapshot", "snapshot": {
                "snapshotSequence": 5,
                "projects": [{"id": "p1", "title": "Demo", "workspaceRoot": "/tmp/demo", "defaultModelSelection": null, "scripts": [], "createdAt": "2026-10-01T09:00:00.000Z", "updatedAt": "2026-10-01T09:00:00.000Z"}],
                "threads": threads,
                "updatedAt": "2026-10-01T10:00:00.000Z"
            }}))
            .unwrap();
        shell
    }

    const NOW: &str = "2026-10-01T12:00:00.000Z";

    #[test]
    fn sections_and_order() {
        let s = shell(vec![
            thread("a", json!({"createdAt": "2026-10-01T09:00:00.000Z"})),
            thread("b", json!({"createdAt": "2026-10-01T11:00:00.000Z"})),
            thread("c", json!({"pinnedAt": "2026-10-01T11:00:00.000Z"})),
            thread("d", json!({"settledOverride": "settled", "settledAt": "2026-10-01T11:30:00.000Z"})),
            thread(
                "e",
                json!({"snoozedUntil": "2026-10-02T09:00:00.000Z", "snoozedAt": "2026-10-01T11:00:00.000Z"}),
            ),
            thread("f", json!({"archivedAt": "2026-10-01T11:00:00.000Z"})),
            // Snoozed but waiting for an approval: back in Active.
            thread(
                "g",
                json!({"snoozedUntil": "2026-10-02T09:00:00.000Z", "hasPendingApprovals": true, "createdAt": "2026-10-01T08:00:00.000Z"}),
            ),
        ]);
        let now = millis(NOW).unwrap();
        let sidebar = s.sidebar(None, "", now);
        let names: Vec<(Section, Vec<&str>)> = sidebar
            .iter()
            .map(|(section, threads)| (*section, threads.iter().map(|t| t.id.as_str()).collect()))
            .collect();
        assert_eq!(
            names,
            vec![
                (Section::Pinned, vec!["c"]),
                (Section::Active, vec!["b", "a", "g"]),
                (Section::Snoozed, vec!["e"]),
                (Section::Settled, vec!["d"]),
            ]
        );
        assert_eq!(s.archived().len(), 1);
        assert_eq!(status(s.thread(&ThreadId::from("g")).unwrap()), ThreadStatus::Approval);
        assert_eq!(s.sidebar(None, "thread b", now).len(), 1);
    }

    #[test]
    fn events_after_the_snapshot_apply_once() {
        let mut s = shell(vec![thread("a", json!({}))]);
        s.apply(json!({"kind": "thread-upserted", "sequence": 6, "thread": thread("a", json!({"title": "Renamed"}))}))
            .unwrap();
        // An older event is ignored.
        s.apply(json!({"kind": "thread-upserted", "sequence": 4, "thread": thread("a", json!({"title": "Old"}))}))
            .unwrap();
        assert_eq!(s.threads[0].title, "Renamed");
        s.apply(json!({"kind": "thread-removed", "sequence": 7, "threadId": "a"})).unwrap();
        assert!(s.threads.is_empty());
        s.apply(json!({"kind": "synchronized"})).unwrap();
        assert!(s.live);
    }

    #[test]
    fn order_keys_sort_between() {
        let first = order_key_between(None, None).unwrap();
        assert_eq!(first, "n");
        let before = order_key_between(None, Some(&first)).unwrap();
        let after = order_key_between(Some(&first), None).unwrap();
        let mid = order_key_between(Some(&first), Some(&after)).unwrap();
        let tight = order_key_between(Some("b"), Some("c")).unwrap();
        assert!("b" < tight.as_str() && tight.as_str() < "c", "{tight}");
        assert_eq!(order_key_between(Some("c"), Some("b")), None);
        assert!(before < first, "{before} < {first}");
        assert!(first < after, "{first} < {after}");
        assert!(first < mid && mid < after, "{first} < {mid} < {after}");
    }
}

/// The state a pull request badge shows, folded over every link of a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullRequestTone {
    Open,
    Draft,
    Closed,
    Merged,
    /// Not synced yet (a lone link without a snapshot).
    Unknown,
}

/// The pull request badge of a sidebar row or the composer's strip
/// (`resolveThreadPullRequestBadge` and `resolveThreadPullRequestBadgePresentation`): one
/// pull request shows its number, several unrelated ones "+N", a stack its layer count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestBadge {
    pub tone: PullRequestTone,
    pub text: String,
    pub stack: bool,
    pub url: String,
}

pub fn pull_request_badge(thread: &OrchestrationThreadShell) -> Option<PullRequestBadge> {
    use zc_contracts::{PullRequestState, ThreadPullRequestLinkSource};
    let visible: Vec<_> = thread
        .pull_requests
        .iter()
        .filter(|link| link.source != ThreadPullRequestLinkSource::StackDismissed)
        .collect();
    let Some(first) = visible.first() else {
        let legacy = thread.linked_pull_request.clone().flatten()?;
        return Some(PullRequestBadge {
            tone: PullRequestTone::Unknown,
            text: legacy.number.to_string(),
            stack: false,
            url: legacy.url.to_string(),
        });
    };
    let state = |link: &&zc_contracts::ThreadPullRequestLink| link.snapshot.as_ref().map(|s| s.state).unwrap_or(PullRequestState::Open);
    let folded = if visible
        .iter()
        .all(|l| l.snapshot.as_ref().is_some_and(|s| s.state == PullRequestState::Open && s.is_draft))
    {
        PullRequestTone::Draft
    } else if visible.iter().any(|l| state(l) == PullRequestState::Open) {
        PullRequestTone::Open
    } else if visible.iter().all(|l| state(l) == PullRequestState::Merged) {
        PullRequestTone::Merged
    } else {
        PullRequestTone::Closed
    };
    if visible.len() > 1 && pull_request_chains(&visible) == 1 {
        return Some(PullRequestBadge {
            tone: folded,
            text: visible.len().to_string(),
            stack: true,
            url: first.url.to_string(),
        });
    }
    if visible.len() > 1 {
        return Some(PullRequestBadge {
            tone: folded,
            text: format!("+{}", visible.len()),
            stack: false,
            url: first.url.to_string(),
        });
    }
    let tone = match first.snapshot.as_ref() {
        None => PullRequestTone::Unknown,
        Some(s) if s.state == PullRequestState::Open && s.is_draft => PullRequestTone::Draft,
        Some(s) => match s.state {
            PullRequestState::Open => PullRequestTone::Open,
            PullRequestState::Closed => PullRequestTone::Closed,
            PullRequestState::Merged => PullRequestTone::Merged,
        },
    };
    Some(PullRequestBadge {
        tone,
        text: first.number.to_string(),
        stack: false,
        url: first.url.to_string(),
    })
}

/// How many chains the links form (`resolveThreadPullRequestChains`): each native stack is one,
/// then links are chained by branches (one's base is another's head, in the same repository),
/// walking down from each link nothing builds on; links left in a cycle count alone.
fn pull_request_chains(links: &[&zc_contracts::ThreadPullRequestLink]) -> usize {
    use std::collections::{HashMap, HashSet};
    type Link = zc_contracts::ThreadPullRequestLink;
    let repo = |l: &Link| format!("{}/{}", l.host.to_lowercase(), l.repository.to_lowercase());
    let key = |l: &Link| format!("{}#{}", repo(l), l.number);
    let branch = |l: &Link, b: &str| format!("{}:{b}", repo(l));
    let mut placed: HashSet<String> = HashSet::new();
    let mut stacks: HashSet<String> = HashSet::new();
    for link in links {
        if let Some(stack) = &link.stack {
            stacks.insert(format!("{}#stack:{}", repo(link), stack.id));
            placed.insert(key(link));
        }
    }
    let mut chains = stacks.len();
    let remaining: Vec<&Link> = links.iter().copied().filter(|l| !placed.contains(&key(l))).collect();
    // A head name used twice cannot name a parent.
    let mut by_head: HashMap<String, Option<&Link>> = HashMap::new();
    for link in &remaining {
        if let Some(snapshot) = &link.snapshot {
            let k = branch(link, &snapshot.head_branch);
            let seen = by_head.contains_key(&k);
            by_head.insert(k, if seen { None } else { Some(*link) });
        }
    }
    let parent = |l: &Link| l.snapshot.as_ref().and_then(|s| by_head.get(&branch(l, &s.base_branch)).copied().flatten());
    let mut has_child: HashSet<String> = HashSet::new();
    for link in &remaining {
        if let Some(p) = parent(link).filter(|p| key(p) != key(link)) {
            has_child.insert(key(p));
        }
    }
    for top in &remaining {
        if has_child.contains(&key(top)) {
            continue;
        }
        let mut layers = 0;
        let mut cursor = Some(*top);
        while let Some(link) = cursor {
            if !placed.insert(key(link)) {
                break;
            }
            layers += 1;
            cursor = parent(link);
        }
        if layers > 0 {
            chains += 1;
        }
    }
    chains + remaining.iter().filter(|l| !placed.contains(&key(l))).count()
}
