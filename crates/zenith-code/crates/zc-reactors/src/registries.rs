//! The in-memory per-thread registries the ingestion feeds and the shell query reads at mapping
//! time (no persistence): `ThreadBackgroundLiveness.ts` and `ThreadPlanProgress.ts`.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zc_contracts::ThreadId;

/// `MONITOR_TASK_TYPES` (`contracts/providerRuntime.ts`): watch loops.
pub const MONITOR_TASK_TYPES: &[&str] = &["monitor", "monitor_mcp", "local_bash", "shell"];
/// `INERT_TASK_TYPES`: plan-mode bookkeeping.
pub const INERT_TASK_TYPES: &[&str] = &["plan", "dream"];

const TERMINAL_STATUSES: &[&str] = &["completed", "failed", "stopped", "cancelled", "interrupted"];

/// `classifyTaskAgentKind({taskType, agentId})`.
pub fn classify_task_agent_kind(task_type: Option<&str>, agent_id: Option<&str>) -> &'static str {
    let non_agent_type = task_type.is_some_and(|kind| MONITOR_TASK_TYPES.contains(&kind) || INERT_TASK_TYPES.contains(&kind));
    if agent_id.is_some_and(|id| !crate::js::trim(id).is_empty()) {
        return if task_type.is_none() || non_agent_type { "background" } else { "agent" };
    }
    if non_agent_type {
        "background"
    } else {
        "agent"
    }
}

/// `ThreadBackgroundLiveness`: `"working" | "monitoring" | null`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackgroundLivenessState {
    Working,
    Monitoring,
}

/// The kind of task lifecycle transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskTransition {
    Started,
    Progress,
    Updated,
    Completed,
}

/// One `recordTaskLiveness` input.
#[derive(Debug, Clone, Copy)]
pub struct TaskLivenessInput<'a> {
    pub thread_id: &'a str,
    pub task_id: &'a str,
    pub task_type: Option<&'a str>,
    pub status: Option<&'a str>,
    pub kind: TaskTransition,
    pub agent_id: Option<&'a str>,
}

#[derive(Default)]
struct LivenessState {
    agents: HashSet<String>,
    monitors: HashSet<String>,
}

/// `ThreadBackgroundLivenessService`: which threads still run background work after their turn
/// settled (subagent fleets, workflow runs, watch loops). Empty after a restart, which matches
/// reality: orphaned background work is not live.
#[derive(Default)]
pub struct ThreadBackgroundLivenessRegistry {
    threads: Mutex<HashMap<String, LivenessState>>,
}

impl ThreadBackgroundLivenessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn drop_task(threads: &mut HashMap<String, LivenessState>, thread_id: &str, task_id: &str) {
        let Some(state) = threads.get_mut(thread_id) else { return };
        state.agents.remove(task_id);
        state.monitors.remove(task_id);
        if state.agents.is_empty() && state.monitors.is_empty() {
            threads.remove(thread_id);
        }
    }

    /// `recordTaskLiveness(input)`.
    pub fn record_task_liveness(&self, input: TaskLivenessInput<'_>) {
        let mut threads = self.threads.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let task_type = input.task_type;
        if task_type.is_some_and(|kind| INERT_TASK_TYPES.contains(&kind)) {
            Self::drop_task(&mut threads, input.thread_id, input.task_id);
            return;
        }
        // A subagent's own shells and monitors are covered by its liveness; nested agents are not.
        if input.agent_id.is_some() && task_type.is_none_or(|kind| MONITOR_TASK_TYPES.contains(&kind)) {
            Self::drop_task(&mut threads, input.thread_id, input.task_id);
            return;
        }
        let terminal =
            input.kind == TaskTransition::Completed || input.status == Some("idle") || input.status.is_some_and(|status| TERMINAL_STATUSES.contains(&status));
        if terminal {
            Self::drop_task(&mut threads, input.thread_id, input.task_id);
            return;
        }
        // Status-free progress and metadata updates are not restarts.
        if matches!(input.kind, TaskTransition::Progress | TaskTransition::Updated) && input.status.is_none() {
            let still_live = threads
                .get(input.thread_id)
                .is_some_and(|state| state.agents.contains(input.task_id) || state.monitors.contains(input.task_id));
            if !still_live {
                return;
            }
        }
        Self::drop_task(&mut threads, input.thread_id, input.task_id);
        let state = threads.entry(input.thread_id.to_owned()).or_default();
        if task_type.is_some_and(|kind| MONITOR_TASK_TYPES.contains(&kind)) {
            state.monitors.insert(input.task_id.to_owned());
        } else {
            state.agents.insert(input.task_id.to_owned());
        }
    }

    /// `clearThreadLiveness(threadId)`: session death orphans all background work.
    pub fn clear_thread_liveness(&self, thread_id: &str) {
        self.threads.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(thread_id);
    }

    /// `getThreadBackgroundLiveness(threadId)`.
    pub fn get_thread_background_liveness(&self, thread_id: &str) -> Option<BackgroundLivenessState> {
        let threads = self.threads.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = threads.get(thread_id)?;
        if !state.agents.is_empty() {
            Some(BackgroundLivenessState::Working)
        } else if !state.monitors.is_empty() {
            Some(BackgroundLivenessState::Monitoring)
        } else {
            None
        }
    }
}

impl zc_orchestration::BackgroundLiveness for ThreadBackgroundLivenessRegistry {
    fn has_live_background_work(&self, thread_id: &ThreadId) -> bool {
        self.get_thread_background_liveness(thread_id.as_str()).is_some()
    }
}

/// `ThreadPlanProgress`: the current plan step for the working indicators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPlanProgress {
    pub step: String,
    pub completed_steps: usize,
    pub total_steps: usize,
}

/// `ThreadPlanProgressService`.
#[derive(Default)]
pub struct ThreadPlanProgressRegistry {
    threads: Mutex<HashMap<String, ThreadPlanProgress>>,
}

impl ThreadPlanProgressRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// `recordPlanProgress(threadId, plan)`, `plan` being `[{step, status}]`. An all-completed
    /// (or empty) plan clears the entry.
    pub fn record_plan_progress(&self, thread_id: &str, plan: &[Value]) {
        let status = |step: &Value| step.get("status").and_then(Value::as_str).unwrap_or("").to_owned();
        let total_steps = plan.len();
        let completed_steps = plan.iter().filter(|step| status(step) == "completed").count();
        let current = plan
            .iter()
            .find(|step| status(step) == "inProgress")
            .or_else(|| plan.iter().find(|step| status(step) != "completed"));
        let mut threads = self.threads.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match current {
            Some(current) if total_steps > 0 && completed_steps != total_steps => {
                threads.insert(
                    thread_id.to_owned(),
                    ThreadPlanProgress {
                        step: current.get("step").and_then(Value::as_str).unwrap_or("").to_owned(),
                        completed_steps,
                        total_steps,
                    },
                );
            }
            _ => {
                threads.remove(thread_id);
            }
        }
    }

    /// `clearThreadPlanProgress(threadId)`.
    pub fn clear_thread_plan_progress(&self, thread_id: &str) {
        self.threads.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(thread_id);
    }

    /// `getThreadPlanProgress(threadId)`.
    pub fn get_thread_plan_progress(&self, thread_id: &str) -> Option<ThreadPlanProgress> {
        self.threads.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(thread_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    //! Port of `ThreadBackgroundLiveness.test.ts` and `ThreadPlanProgress.test.ts`.
    use super::*;
    use serde_json::json;

    fn record(
        registry: &ThreadBackgroundLivenessRegistry,
        thread_id: &str,
        task_id: &str,
        task_type: Option<&str>,
        status: Option<&str>,
        kind: TaskTransition,
    ) {
        record_with(registry, thread_id, task_id, task_type, status, kind, None);
    }

    fn record_with(
        registry: &ThreadBackgroundLivenessRegistry,
        thread_id: &str,
        task_id: &str,
        task_type: Option<&str>,
        status: Option<&str>,
        kind: TaskTransition,
        agent_id: Option<&str>,
    ) {
        registry.record_task_liveness(TaskLivenessInput {
            thread_id,
            task_id,
            task_type,
            status,
            kind,
            agent_id,
        });
    }

    use BackgroundLivenessState::*;
    use TaskTransition::*;

    #[test]
    fn status_free_progress_or_metadata_does_not_restart_an_idle_task() {
        let liveness = ThreadBackgroundLivenessRegistry::new();
        record(&liveness, "thread", "task", None, None, Started);
        record(&liveness, "thread", "task", None, Some("idle"), Updated);
        record(&liveness, "thread", "task", None, None, Progress);
        record(&liveness, "thread", "task", None, None, Updated);
        assert_eq!(liveness.get_thread_background_liveness("thread"), None);
        record(&liveness, "thread", "completed-task", None, None, Started);
        record(&liveness, "thread", "completed-task", None, Some("completed"), Completed);
        record(&liveness, "thread", "completed-task", None, None, Updated);
        assert_eq!(liveness.get_thread_background_liveness("thread"), None);
    }

    #[test]
    fn agents_work_monitors_monitor_agents_win() {
        let liveness = ThreadBackgroundLivenessRegistry::new();
        let thread = "t-live-1";
        record(&liveness, thread, "m1", Some("local_bash"), None, Started);
        assert_eq!(liveness.get_thread_background_liveness(thread), Some(Monitoring));
        record(&liveness, thread, "a1", Some("subagent"), None, Started);
        assert_eq!(liveness.get_thread_background_liveness(thread), Some(Working));
        record(&liveness, thread, "a1", Some("subagent"), Some("completed"), Completed);
        assert_eq!(liveness.get_thread_background_liveness(thread), Some(Monitoring));
        record(&liveness, thread, "m1", Some("local_bash"), Some("completed"), Completed);
        assert_eq!(liveness.get_thread_background_liveness(thread), None);
    }

    #[test]
    fn terminal_rows_without_a_task_type_clear_monitors() {
        let liveness = ThreadBackgroundLivenessRegistry::new();
        record(&liveness, "t-live-2", "m1", Some("local_bash"), None, Started);
        record(&liveness, "t-live-2", "m1", None, Some("completed"), Completed);
        assert_eq!(liveness.get_thread_background_liveness("t-live-2"), None);
    }

    #[test]
    fn nested_agents_still_count() {
        let liveness = ThreadBackgroundLivenessRegistry::new();
        record_with(&liveness, "t", "n1", Some("local_agent"), None, Started, Some("owner"));
        assert_eq!(liveness.get_thread_background_liveness("t"), Some(Working));
        record_with(&liveness, "t", "n1", Some("local_agent"), Some("completed"), Completed, Some("owner"));
        assert_eq!(liveness.get_thread_background_liveness("t"), None);
    }

    #[test]
    fn untyped_rows_are_agents_idle_is_not_live_agent_owned_ignored() {
        let liveness = ThreadBackgroundLivenessRegistry::new();
        record(&liveness, "t", "wf:1", None, Some("running"), Progress);
        assert_eq!(liveness.get_thread_background_liveness("t"), Some(Working));
        record(&liveness, "t", "wf:1", None, Some("idle"), Updated);
        assert_eq!(liveness.get_thread_background_liveness("t"), None);
        record_with(&liveness, "t", "sh:1", Some("local_bash"), None, Started, Some("owner"));
        assert_eq!(liveness.get_thread_background_liveness("t"), None);
    }

    #[test]
    fn reclassification_moves_between_buckets() {
        let liveness = ThreadBackgroundLivenessRegistry::new();
        record(&liveness, "t", "x1", None, Some("running"), Started);
        assert_eq!(liveness.get_thread_background_liveness("t"), Some(Working));
        record(&liveness, "t", "x1", Some("local_bash"), Some("running"), Progress);
        assert_eq!(liveness.get_thread_background_liveness("t"), Some(Monitoring));
        record_with(&liveness, "t", "x1", Some("local_bash"), Some("running"), Progress, Some("owner"));
        assert_eq!(liveness.get_thread_background_liveness("t"), None);
    }

    #[test]
    fn plan_tasks_are_inert_clear_removes_instances_isolated() {
        let a = ThreadBackgroundLivenessRegistry::new();
        let b = ThreadBackgroundLivenessRegistry::new();
        record(&a, "t", "p1", Some("plan"), None, Started);
        assert_eq!(a.get_thread_background_liveness("t"), None);
        record(&a, "t", "a1", Some("local_workflow"), None, Started);
        assert_eq!(a.get_thread_background_liveness("t"), Some(Working));
        assert_eq!(b.get_thread_background_liveness("t"), None);
        a.clear_thread_liveness("t");
        assert_eq!(a.get_thread_background_liveness("t"), None);
    }

    #[test]
    fn classifies_task_agent_kinds() {
        assert_eq!(classify_task_agent_kind(Some("subagent"), None), "agent");
        assert_eq!(classify_task_agent_kind(Some("local_bash"), None), "background");
        assert_eq!(classify_task_agent_kind(None, Some("owner")), "background");
        assert_eq!(classify_task_agent_kind(Some("local_agent"), Some("owner")), "agent");
        assert_eq!(classify_task_agent_kind(Some("plan"), Some(" ")), "background");
        assert_eq!(classify_task_agent_kind(None, None), "agent");
    }

    #[test]
    fn plan_progress_tracks_the_step_and_clears_on_completion() {
        let progress = ThreadPlanProgressRegistry::new();
        progress.record_plan_progress(
            "t",
            &[
                json!({"step": "Audit failure paths", "status": "completed"}),
                json!({"step": "Implement the fix", "status": "inProgress"}),
                json!({"step": "Run targeted tests", "status": "pending"}),
            ],
        );
        assert_eq!(
            progress.get_thread_plan_progress("t"),
            Some(ThreadPlanProgress {
                step: "Implement the fix".into(),
                completed_steps: 1,
                total_steps: 3
            })
        );
        progress.record_plan_progress(
            "t",
            &[
                json!({"step": "Audit failure paths", "status": "completed"}),
                json!({"step": "Implement the fix", "status": "completed"}),
                json!({"step": "Run targeted tests", "status": "completed"}),
            ],
        );
        assert_eq!(progress.get_thread_plan_progress("t"), None);
    }

    #[test]
    fn plan_progress_falls_back_to_the_first_unfinished_step() {
        let progress = ThreadPlanProgressRegistry::new();
        progress.record_plan_progress(
            "t",
            &[json!({"step": "First", "status": "pending"}), json!({"step": "Second", "status": "pending"})],
        );
        assert_eq!(progress.get_thread_plan_progress("t").unwrap().step, "First");
    }

    #[test]
    fn plan_progress_clear_removes_the_entry() {
        let progress = ThreadPlanProgressRegistry::new();
        progress.record_plan_progress("t", &[json!({"step": "Only step", "status": "inProgress"})]);
        progress.clear_thread_plan_progress("t");
        assert_eq!(progress.get_thread_plan_progress("t"), None);
    }
}
