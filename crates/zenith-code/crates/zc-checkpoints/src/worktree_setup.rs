//! `project/WorktreeSetupTracker.ts`: the live stages of a bootstrap worktree setup, per thread,
//! for the progress card (`subscribeWorktreeSetup`, `worktreeSetup.cancel`).
//!
//! Memory only: an entry exists from `begin` until the setup settles, plus a 30 s grace window
//! so a late subscriber still sees the outcome. The durable record is the thread's worktree
//! path and the `worktree-setup` activity.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zc_contracts::{ThreadId, WorktreeSetupPhase, WorktreeSetupSnapshot, WorktreeSetupStage, WorktreeSetupStageId, WorktreeSetupStageStatus};
use zc_core::pubsub::PubSub;
use zc_ports::EventStream;

use crate::support::now_iso;

/// `WORKTREE_SETUP_DETAIL_MAX_LENGTH`.
pub const DETAIL_MAX_LENGTH: usize = 200;
/// `WORKTREE_SETUP_TAIL_LINE_MAX_LENGTH`.
pub const TAIL_LINE_MAX_LENGTH: usize = 400;
/// `WORKTREE_SETUP_ERROR_MAX_LENGTH`.
pub const ERROR_MAX_LENGTH: usize = 1000;
/// Lines of output kept per stage.
pub const TAIL_LINE_LIMIT: usize = 4;
/// `FINISHED_RETENTION`.
pub const FINISHED_RETENTION: Duration = Duration::from_secs(30);
/// `WORKTREE_SETUP_STAGE_ORDER`.
pub const STAGE_ORDER: [WorktreeSetupStageId; 5] = [
    WorktreeSetupStageId::Fetch,
    WorktreeSetupStageId::Checkout,
    WorktreeSetupStageId::Submodules,
    WorktreeSetupStageId::SetupScript,
    WorktreeSetupStageId::Agent,
];

/// `clampText`: at most `max` UTF-16 units, ending in an ellipsis when cut.
pub fn clamp_text(text: &str, max: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max {
        return text.to_owned();
    }
    let mut out = String::from_utf16_lossy(&units[..max - 1]);
    out.push('\u{2026}');
    out
}

fn clamp_detail(detail: Option<&str>) -> Option<String> {
    detail.map(|d| clamp_text(d, DETAIL_MAX_LENGTH))
}

fn empty_stage(id: WorktreeSetupStageId) -> WorktreeSetupStage {
    WorktreeSetupStage {
        id,
        status: WorktreeSetupStageStatus::Pending,
        started_at: None,
        ended_at: None,
        percent: None,
        detail: None,
        tail: Vec::new(),
    }
}

/// The cancel handle of a running bootstrap (the TS `fiber`): cancelling it interrupts the
/// bootstrap, and [`BootstrapHandle::cancel_and_wait`] returns once its cleanup has run.
#[derive(Clone)]
pub struct BootstrapHandle {
    token: CancellationToken,
    finished: watch::Receiver<bool>,
}

/// Held by the bootstrap task; dropping it (or calling [`BootstrapFinished::finish`]) marks
/// the bootstrap unwound.
pub struct BootstrapFinished(watch::Sender<bool>);

impl BootstrapFinished {
    pub fn finish(self) {}
}

impl Drop for BootstrapFinished {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

impl BootstrapHandle {
    /// A handle, its cancellation token (for the bootstrap to watch) and the completion guard.
    pub fn new() -> (Self, CancellationToken, BootstrapFinished) {
        let token = CancellationToken::new();
        let (sender, finished) = watch::channel(false);
        (
            Self {
                token: token.clone(),
                finished,
            },
            token,
            BootstrapFinished(sender),
        )
    }

    /// `Fiber.interrupt`: cancel and wait until the bootstrap has unwound.
    pub async fn cancel_and_wait(&self) {
        self.token.cancel();
        let mut finished = self.finished.clone();
        let _ = finished.wait_for(|done| *done).await;
    }
}

struct Tracked {
    snapshot: WorktreeSetupSnapshot,
    handle: Option<BootstrapHandle>,
}

#[derive(Clone)]
struct Change {
    thread_id: ThreadId,
    snapshot: Option<WorktreeSetupSnapshot>,
}

#[derive(Default)]
struct State {
    setups: HashMap<ThreadId, Tracked>,
    retention: HashMap<ThreadId, (u64, JoinHandle<()>)>,
    /// Sequences keep increasing across setups of one thread, so a stream opened during a
    /// previous setup accepts the next one's first snapshot.
    last_sequence: HashMap<ThreadId, i64>,
    retention_ids: u64,
}

/// `WorktreeSetupTracker`. Cloning shares it.
#[derive(Clone, Default)]
pub struct WorktreeSetupTracker {
    state: Arc<Mutex<State>>,
    changes: PubSub<Change>,
}

/// `Partial<Omit<WorktreeSetupStage, "id">>` as the bootstrap uses it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StagePatch {
    /// `Some(None)` sets null.
    pub percent: Option<Option<i64>>,
    /// `Some(None)` sets null; clamped to the contract limit.
    pub detail: Option<Option<String>>,
}

impl WorktreeSetupTracker {
    pub fn new() -> Self {
        Self::default()
    }

    fn publish(&self, thread_id: &ThreadId, snapshot: Option<WorktreeSetupSnapshot>) {
        self.changes.publish(Change {
            thread_id: thread_id.clone(),
            snapshot,
        });
    }

    fn clear_retention(state: &mut State, thread_id: &ThreadId) {
        if let Some((_, task)) = state.retention.remove(thread_id) {
            task.abort();
        }
    }

    /// `begin`: a fresh running snapshot (stages in canonical order), replacing any prior one.
    pub fn begin(
        &self,
        thread_id: &ThreadId,
        branch: Option<String>,
        base_ref: Option<String>,
        stages: &[WorktreeSetupStageId],
        handle: Option<BootstrapHandle>,
    ) {
        let mut state = self.state.lock().unwrap();
        Self::clear_retention(&mut state, thread_id);
        let sequence = state.last_sequence.get(thread_id).copied().unwrap_or(-1) + 1;
        let snapshot = WorktreeSetupSnapshot {
            thread_id: thread_id.clone(),
            phase: WorktreeSetupPhase::Running,
            started_at: now_iso(),
            ended_at: None,
            branch,
            base_ref,
            worktree_path: None,
            setup_script: None,
            stages: STAGE_ORDER.iter().filter(|id| stages.contains(id)).map(|id| empty_stage(*id)).collect(),
            error: None,
            sequence,
        };
        state.last_sequence.insert(thread_id.clone(), sequence);
        state.setups.insert(
            thread_id.clone(),
            Tracked {
                snapshot: snapshot.clone(),
                handle,
            },
        );
        self.publish(thread_id, Some(snapshot));
    }

    /// `modify`: mutate a tracked setup, bump its sequence, publish. `None` when untracked.
    fn modify(&self, thread_id: &ThreadId, mutate: impl FnOnce(&mut Tracked)) -> Option<WorktreeSetupSnapshot> {
        let mut state = self.state.lock().unwrap();
        let tracked = state.setups.get_mut(thread_id)?;
        let previous = tracked.snapshot.sequence;
        mutate(tracked);
        tracked.snapshot.sequence = previous + 1;
        let snapshot = tracked.snapshot.clone();
        state.last_sequence.insert(thread_id.clone(), snapshot.sequence);
        // Published under the lock so subscribers see sequences in order.
        self.publish(thread_id, Some(snapshot.clone()));
        Some(snapshot)
    }

    /// `update(threadId, mutate)`.
    pub fn update(&self, thread_id: &ThreadId, mutate: impl FnOnce(&mut WorktreeSetupSnapshot)) {
        self.modify(thread_id, |tracked| mutate(&mut tracked.snapshot));
    }

    /// `stage(threadId, stageId, patch)`.
    pub fn stage(&self, thread_id: &ThreadId, stage_id: WorktreeSetupStageId, patch: StagePatch) {
        self.update(thread_id, |snapshot| {
            for stage in snapshot.stages.iter_mut().filter(|s| s.id == stage_id) {
                if let Some(percent) = patch.percent {
                    stage.percent = percent;
                }
                if let Some(detail) = &patch.detail {
                    stage.detail = clamp_detail(detail.as_deref());
                }
            }
        });
    }

    /// `stageStatus(threadId, stageId, status, detail?)`: stamps `startedAt` on the first
    /// non-pending status and `endedAt` on the first settled one. `detail: None` keeps the
    /// current detail; `Some(None)` clears it.
    pub fn stage_status(&self, thread_id: &ThreadId, stage_id: WorktreeSetupStageId, status: WorktreeSetupStageStatus, detail: Option<Option<&str>>) {
        let at = now_iso();
        self.update(thread_id, |snapshot| {
            for stage in snapshot.stages.iter_mut().filter(|s| s.id == stage_id) {
                if stage.started_at.is_none() && status != WorktreeSetupStageStatus::Pending {
                    stage.started_at = Some(at.clone());
                }
                if matches!(status, WorktreeSetupStageStatus::Running | WorktreeSetupStageStatus::Pending) {
                    stage.ended_at = None;
                } else if stage.ended_at.is_none() {
                    stage.ended_at = Some(at.clone());
                }
                stage.status = status;
                if let Some(detail) = detail {
                    stage.detail = clamp_detail(detail);
                }
            }
        });
    }

    /// `appendTail(threadId, stageId, line)`: keeps the last four lines.
    pub fn append_tail(&self, thread_id: &ThreadId, stage_id: WorktreeSetupStageId, line: &str) {
        self.update(thread_id, |snapshot| {
            for stage in snapshot.stages.iter_mut().filter(|s| s.id == stage_id) {
                stage.tail.push(clamp_text(line, TAIL_LINE_MAX_LENGTH));
                let excess = stage.tail.len().saturating_sub(TAIL_LINE_LIMIT);
                stage.tail.drain(..excess);
            }
        });
    }

    /// `finish(threadId, phase, error?)`: settles the setup (running stages become done,
    /// skipped or failed with it), drops the cancel handle and schedules the removal. Returns
    /// the settled snapshot, or `None` when nothing was tracked.
    pub fn finish(&self, thread_id: &ThreadId, phase: WorktreeSetupPhase, error: Option<&str>) -> Option<WorktreeSetupSnapshot> {
        let ended_at = now_iso();
        let settled_status = match phase {
            WorktreeSetupPhase::Done => WorktreeSetupStageStatus::Done,
            WorktreeSetupPhase::Cancelled => WorktreeSetupStageStatus::Skipped,
            _ => WorktreeSetupStageStatus::Failed,
        };
        let snapshot = self.modify(thread_id, |tracked| {
            tracked.handle = None;
            let snapshot = &mut tracked.snapshot;
            snapshot.phase = phase;
            snapshot.ended_at = Some(ended_at.clone());
            snapshot.error = error.map(|e| clamp_text(e, ERROR_MAX_LENGTH));
            for stage in snapshot.stages.iter_mut().filter(|s| s.status == WorktreeSetupStageStatus::Running) {
                stage.status = settled_status;
                stage.ended_at = Some(ended_at.clone());
            }
        })?;
        let mut state = self.state.lock().unwrap();
        Self::clear_retention(&mut state, thread_id);
        state.retention_ids += 1;
        let id = state.retention_ids;
        let this = self.clone();
        let removed = thread_id.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(FINISHED_RETENTION).await;
            this.remove(&removed, id);
        });
        state.retention.insert(thread_id.clone(), (id, task));
        Some(snapshot)
    }

    /// The retention timer's `remove`: drops the entry, publishes `null`, and lets the
    /// thread's sequence start over (a subscriber that saw `null` accepts any sequence).
    fn remove(&self, thread_id: &ThreadId, retention_id: u64) {
        let mut state = self.state.lock().unwrap();
        // Only drop our own timer's registration: a newer setup may have replaced it.
        if state.retention.get(thread_id).is_some_and(|(id, _)| *id == retention_id) {
            state.retention.remove(thread_id);
        }
        state.setups.remove(thread_id);
        self.publish(thread_id, None);
        state.last_sequence.remove(thread_id);
    }

    /// `markUncancellable`: called right before the turn is dispatched, so a late cancel cannot
    /// roll back a thread whose agent already started.
    pub fn mark_uncancellable(&self, thread_id: &ThreadId) {
        if let Some(tracked) = self.state.lock().unwrap().setups.get_mut(thread_id) {
            tracked.handle = None;
        }
    }

    /// `cancel`: interrupts the running bootstrap and waits for it to unwind. False when
    /// nothing is running or the setup is past cancellation.
    pub async fn cancel(&self, thread_id: &ThreadId) -> bool {
        let handle = {
            let state = self.state.lock().unwrap();
            match state.setups.get(thread_id) {
                Some(tracked) if tracked.snapshot.phase == WorktreeSetupPhase::Running => tracked.handle.clone(),
                _ => None,
            }
        };
        let Some(handle) = handle else { return false };
        handle.cancel_and_wait().await;
        true
    }

    /// `get`.
    pub fn get(&self, thread_id: &ThreadId) -> Option<WorktreeSetupSnapshot> {
        self.state.lock().unwrap().setups.get(thread_id).map(|t| t.snapshot.clone())
    }

    /// `stream`: the current snapshot (or null) first, then every newer change, through a
    /// one-slot latest-value mailbox per subscriber (a slow socket only ever holds the newest
    /// snapshot). Never steps back behind a sequence already delivered.
    pub fn stream(&self, thread_id: &ThreadId) -> EventStream<Option<WorktreeSetupSnapshot>> {
        let mut subscription = self.changes.subscribe();
        let initial = self.get(thread_id);
        let mut last_sequence = initial.as_ref().map_or(-1, |s| s.sequence);
        let (sender, receiver) = watch::channel(initial);
        let thread_id = thread_id.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = sender.closed() => break,
                    change = subscription.recv() => {
                        let Some(change) = change else { break };
                        if change.thread_id != thread_id {
                            continue;
                        }
                        if let Some(snapshot) = &change.snapshot {
                            if snapshot.sequence <= last_sequence {
                                continue;
                            }
                        }
                        last_sequence = change.snapshot.as_ref().map_or(-1, |s| s.sequence);
                        sender.send_replace(change.snapshot);
                    }
                }
            }
        });
        futures::stream::unfold((receiver, true), |(mut receiver, first)| async move {
            if !first && receiver.changed().await.is_err() {
                return None;
            }
            let value = receiver.borrow_and_update().clone();
            Some((value, (receiver, false)))
        })
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    //! Port of `project/WorktreeSetupTracker.test.ts`.
    use super::*;
    use futures::StreamExt;

    fn thread() -> ThreadId {
        ThreadId::new("thread-1")
    }

    #[tokio::test]
    async fn records_stage_transitions_checkout_progress_and_the_final_phase() {
        let tracker = WorktreeSetupTracker::new();
        tracker.begin(
            &thread(),
            Some("feature".into()),
            Some("main".into()),
            &[WorktreeSetupStageId::Checkout, WorktreeSetupStageId::Fetch, WorktreeSetupStageId::Agent],
            None,
        );
        let initial = tracker.get(&thread()).unwrap();
        let ids: Vec<_> = initial.stages.iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            vec![WorktreeSetupStageId::Fetch, WorktreeSetupStageId::Checkout, WorktreeSetupStageId::Agent]
        );
        assert_eq!(initial.phase, WorktreeSetupPhase::Running);

        tracker.stage_status(&thread(), WorktreeSetupStageId::Fetch, WorktreeSetupStageStatus::Running, None);
        tracker.stage_status(
            &thread(),
            WorktreeSetupStageId::Fetch,
            WorktreeSetupStageStatus::Done,
            Some(Some("origin/main at abc1234")),
        );
        tracker.stage_status(&thread(), WorktreeSetupStageId::Checkout, WorktreeSetupStageStatus::Running, None);
        tracker.stage(
            &thread(),
            WorktreeSetupStageId::Checkout,
            StagePatch {
                percent: Some(Some(42)),
                detail: Some(Some("42 / 100 files".into())),
            },
        );
        tracker.finish(&thread(), WorktreeSetupPhase::Failed, Some("boom"));

        let last = tracker.get(&thread()).unwrap();
        assert_eq!(last.phase, WorktreeSetupPhase::Failed);
        assert_eq!(last.error.as_deref(), Some("boom"));
        let fetch = &last.stages[0];
        assert_eq!(fetch.status, WorktreeSetupStageStatus::Done);
        assert_eq!(fetch.detail.as_deref(), Some("origin/main at abc1234"));
        assert!(fetch.started_at.is_some() && fetch.ended_at.is_some());
        assert_eq!(last.stages[1].status, WorktreeSetupStageStatus::Failed);
        assert_eq!(last.stages[1].percent, Some(42));
        assert_eq!(last.stages[2].status, WorktreeSetupStageStatus::Pending);
        assert!(last.sequence > initial.sequence);
    }

    #[tokio::test]
    async fn stream_emits_the_current_snapshot_first_and_then_only_newer_ones() {
        let tracker = WorktreeSetupTracker::new();
        tracker.begin(&thread(), None, None, &[WorktreeSetupStageId::Agent], None);
        let mut stream = tracker.stream(&thread());
        tracker.stage_status(&thread(), WorktreeSetupStageId::Agent, WorktreeSetupStageStatus::Running, None);
        tracker.append_tail(&thread(), WorktreeSetupStageId::Agent, "line 1");
        let mut sequences = Vec::new();
        let mut last = None;
        while let Some(snapshot) = stream.next().await {
            let sequence = snapshot.as_ref().map_or(-1, |s| s.sequence);
            sequences.push(sequence);
            last = snapshot;
            if sequence == 2 {
                break;
            }
        }
        let mut sorted = sequences.clone();
        sorted.sort();
        assert_eq!(sequences, sorted);
        assert_eq!(*sequences.last().unwrap(), 2);
        assert_eq!(last.unwrap().stages[0].tail, vec!["line 1".to_owned()]);
    }

    #[tokio::test]
    async fn stream_never_steps_back_behind_the_snapshot_it_started_from() {
        let tracker = WorktreeSetupTracker::new();
        tracker.begin(&thread(), None, None, &[WorktreeSetupStageId::Agent], None);
        tracker.stage_status(&thread(), WorktreeSetupStageId::Agent, WorktreeSetupStageStatus::Running, None);
        tracker.stage_status(&thread(), WorktreeSetupStageId::Agent, WorktreeSetupStageStatus::Done, None);
        let mut stream = tracker.stream(&thread());
        tracker.finish(&thread(), WorktreeSetupPhase::Done, None);
        let mut snapshots = Vec::new();
        while let Some(snapshot) = stream.next().await {
            let done = snapshot.as_ref().is_some_and(|s| s.phase == WorktreeSetupPhase::Done);
            snapshots.push(snapshot);
            if done {
                break;
            }
        }
        assert!(snapshots.iter().all(|s| s.as_ref().map_or(-1, |s| s.sequence) >= 2));
        assert_eq!(snapshots.last().unwrap().as_ref().unwrap().phase, WorktreeSetupPhase::Done);
    }

    #[tokio::test]
    async fn a_new_setup_on_the_same_thread_keeps_sequences_increasing() {
        let tracker = WorktreeSetupTracker::new();
        tracker.begin(&thread(), Some("first".into()), None, &[WorktreeSetupStageId::Agent], None);
        tracker.finish(&thread(), WorktreeSetupPhase::Failed, Some("boom"));
        let failed_sequence = tracker.get(&thread()).unwrap().sequence;
        let mut stream = tracker.stream(&thread());
        tracker.begin(&thread(), Some("second".into()), None, &[WorktreeSetupStageId::Agent], None);
        let mut last = None;
        while let Some(snapshot) = stream.next().await {
            let second = snapshot.as_ref().is_some_and(|s| s.branch.as_deref() == Some("second"));
            last = snapshot;
            if second {
                break;
            }
        }
        let last = last.unwrap();
        assert_eq!(last.phase, WorktreeSetupPhase::Running);
        assert!(last.sequence > failed_sequence);
    }

    #[tokio::test(start_paused = true)]
    async fn finished_setups_are_dropped_after_the_retention_window() {
        let tracker = WorktreeSetupTracker::new();
        tracker.begin(&thread(), None, None, &[WorktreeSetupStageId::Agent], None);
        tracker.finish(&thread(), WorktreeSetupPhase::Done, None);
        assert_eq!(tracker.get(&thread()).unwrap().phase, WorktreeSetupPhase::Done);
        tokio::time::sleep(Duration::from_secs(31)).await;
        assert!(tracker.get(&thread()).is_none());
    }

    #[tokio::test]
    async fn cancel_interrupts_the_bootstrap_and_reports_whether_one_was_running() {
        let tracker = WorktreeSetupTracker::new();
        let (handle, token, finished) = BootstrapHandle::new();
        let unwound = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = unwound.clone();
        let task = tokio::spawn(async move {
            token.cancelled().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            finished.finish();
        });
        tracker.begin(&thread(), None, None, &[WorktreeSetupStageId::Agent], Some(handle));
        assert!(tracker.cancel(&thread()).await);
        // cancel returns only after the bootstrap has unwound.
        assert!(unwound.load(std::sync::atomic::Ordering::SeqCst));
        task.await.unwrap();
        tracker.finish(&thread(), WorktreeSetupPhase::Cancelled, None);
        assert!(!tracker.cancel(&thread()).await);
        assert!(!tracker.cancel(&ThreadId::new("unknown")).await);
    }

    #[tokio::test]
    async fn mark_uncancellable_makes_a_later_cancel_a_no_op_while_the_setup_keeps_running() {
        let tracker = WorktreeSetupTracker::new();
        let (handle, _token, _finished) = BootstrapHandle::new();
        tracker.begin(&thread(), None, None, &[WorktreeSetupStageId::Agent], Some(handle));
        tracker.mark_uncancellable(&thread());
        assert!(!tracker.cancel(&thread()).await);
        assert_eq!(tracker.get(&thread()).unwrap().phase, WorktreeSetupPhase::Running);
    }

    #[tokio::test]
    async fn clamps_free_text_to_the_contract_limits_before_publishing() {
        let tracker = WorktreeSetupTracker::new();
        tracker.begin(
            &thread(),
            None,
            None,
            &[WorktreeSetupStageId::Checkout, WorktreeSetupStageId::SetupScript],
            None,
        );
        let long = "x".repeat(2_000);
        tracker.stage_status(&thread(), WorktreeSetupStageId::Checkout, WorktreeSetupStageStatus::Done, Some(Some(&long)));
        tracker.stage(
            &thread(),
            WorktreeSetupStageId::SetupScript,
            StagePatch {
                percent: None,
                detail: Some(Some(long.clone())),
            },
        );
        tracker.append_tail(&thread(), WorktreeSetupStageId::SetupScript, &long);
        tracker.finish(&thread(), WorktreeSetupPhase::Failed, Some(&long));
        let snapshot = tracker.get(&thread()).unwrap();
        let len = |s: &str| s.encode_utf16().count();
        assert_eq!(len(snapshot.stages[0].detail.as_deref().unwrap()), 200);
        assert_eq!(len(snapshot.stages[1].detail.as_deref().unwrap()), 200);
        assert_eq!(len(&snapshot.stages[1].tail[0]), 400);
        assert_eq!(len(snapshot.error.as_deref().unwrap()), 1000);
        assert!(snapshot.error.unwrap().ends_with('\u{2026}'));
    }
}
