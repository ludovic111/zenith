//! Ports of `checkpointing/CheckpointStore.test.ts` (temp repositories, the real git driver)
//! and `checkpointing/CheckpointDiffQuery.test.ts` (scripted store and projections).

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::*;
use serde_json::json;
use zc_checkpoints::diffs::parse_turn_diff_files_from_numstat;
use zc_checkpoints::errors::CheckpointServiceError;
use zc_checkpoints::{checkpoint_ref_for_thread_turn, CheckpointDiffQuery, CheckpointStore, DiffCheckpointsInput, DiffFormat};
use zc_contracts::{CheckpointRef, OrchestrationGetFullThreadDiffInput, OrchestrationGetTurnDiffInput, ProjectId, ThreadId};
use zc_ports::orchestration::{FullThreadDiffContext, ThreadCheckpointContext};
use zc_vcs::VcsError;

fn input(cwd: &str, from: &CheckpointRef, to: &CheckpointRef, ignore_whitespace: bool, format: DiffFormat) -> DiffCheckpointsInput {
    DiffCheckpointsInput {
        cwd: cwd.to_owned(),
        from_checkpoint_ref: from.clone(),
        to_checkpoint_ref: to.clone(),
        fallback_from_to_head: false,
        ignore_whitespace,
        format,
    }
}

fn summary(files: &[(&str, u64, u64)]) -> Vec<zc_checkpoints::diffs::TurnDiffFileSummary> {
    let mut files: Vec<_> = files
        .iter()
        .map(|(path, additions, deletions)| zc_checkpoints::diffs::TurnDiffFileSummary {
            path: (*path).to_owned(),
            additions: *additions,
            deletions: *deletions,
        })
        .collect();
    files.sort_by(|a, b| zc_vcs::collate::locale_compare(&a.path, &b.path));
    files
}

#[tokio::test]
async fn is_git_repository_detects_repositories() {
    let (_dir, tmp) = temp_dir();
    let store = store();
    assert!(!store.is_git_repository(tmp.to_str().unwrap()).await.unwrap());
    let repo = tmp.join("repo");
    create_git_repository(&repo);
    assert!(store.is_git_repository(repo.to_str().unwrap()).await.unwrap());
}

#[tokio::test]
async fn returns_full_oversized_checkpoint_diffs_without_truncation() {
    let (_dir, tmp) = temp_dir();
    create_git_repository(&tmp);
    let store = store();
    let cwd = tmp.to_str().unwrap();
    let thread = ThreadId::new("thread-checkpoint-store");
    let (from, to) = (checkpoint_ref_for_thread_turn(&thread, 0), checkpoint_ref_for_thread_turn(&thread, 1));
    store.capture_checkpoint(cwd, &from).await.unwrap();
    let large: String = (0..5000).map(|i| format!("line {i:05}\n")).collect();
    std::fs::write(tmp.join("README.md"), large).unwrap();
    store.capture_checkpoint(cwd, &to).await.unwrap();
    let diff = store.diff_checkpoints(&input(cwd, &from, &to, true, DiffFormat::Patch)).await.unwrap();
    assert!(diff.contains("diff --git"));
    assert!(!diff.contains("[truncated]"));
    assert!(diff.contains("+line 04999"));
}

#[tokio::test]
async fn keeps_a_and_b_patch_prefixes_when_the_repository_disables_them() {
    let (_dir, tmp) = temp_dir();
    create_git_repository(&tmp);
    git(&tmp, &["config", "diff.noprefix", "true"]);
    let store = store();
    let cwd = tmp.to_str().unwrap();
    let thread = ThreadId::new("thread-checkpoint-store-noprefix");
    let (from, to) = (checkpoint_ref_for_thread_turn(&thread, 0), checkpoint_ref_for_thread_turn(&thread, 1));
    store.capture_checkpoint(cwd, &from).await.unwrap();
    std::fs::write(tmp.join("README.md"), "# changed\n").unwrap();
    store.capture_checkpoint(cwd, &to).await.unwrap();
    let diff = store.diff_checkpoints(&input(cwd, &from, &to, false, DiffFormat::Patch)).await.unwrap();
    assert!(diff.contains("diff --git a/README.md b/README.md"));
}

#[tokio::test]
async fn can_hide_indentation_churn_when_changes_wrap_existing_lines() {
    let (_dir, tmp) = temp_dir();
    create_git_repository(&tmp);
    let store = store();
    let cwd = tmp.to_str().unwrap();
    let thread = ThreadId::new("thread-checkpoint-store-whitespace");
    let (from, to) = (checkpoint_ref_for_thread_turn(&thread, 0), checkpoint_ref_for_thread_turn(&thread, 1));
    let component = tmp.join("Component.tsx");
    std::fs::write(
        &component,
        [
            "export function View() {",
            "  return (",
            "    <section>",
            "      <h1>Title</h1>",
            "      <p>Body</p>",
            "    </section>",
            "  );",
            "}",
            "",
        ]
        .join("\n"),
    )
    .unwrap();
    store.capture_checkpoint(cwd, &from).await.unwrap();
    std::fs::write(
        &component,
        [
            "export function View() {",
            "  return (",
            "    <section>",
            "      {isReady ? (",
            "        <div>",
            "          <h1>Title</h1>",
            "          <p>Body</p>",
            "        </div>",
            "      ) : null}",
            "    </section>",
            "  );",
            "}",
            "",
        ]
        .join("\n"),
    )
    .unwrap();
    store.capture_checkpoint(cwd, &to).await.unwrap();
    let normal = store.diff_checkpoints(&input(cwd, &from, &to, false, DiffFormat::Patch)).await.unwrap();
    let ignored = store.diff_checkpoints(&input(cwd, &from, &to, true, DiffFormat::Patch)).await.unwrap();
    assert!(normal.contains("-      <h1>Title</h1>") && normal.contains("+          <h1>Title</h1>"));
    assert!(ignored.contains("+      {isReady ? (") && ignored.contains("+        <div>"));
    assert!(!ignored.contains("-      <h1>Title</h1>") && !ignored.contains("+          <h1>Title</h1>"));
    for ignore_whitespace in [false, true] {
        let numstat = store
            .diff_checkpoints(&input(cwd, &from, &to, ignore_whitespace, DiffFormat::Numstat))
            .await
            .unwrap();
        let expected = if ignore_whitespace { (4, 0) } else { (6, 2) };
        assert_eq!(
            parse_turn_diff_files_from_numstat(&numstat),
            summary(&[("Component.tsx", expected.0, expected.1)])
        );
    }
}

#[tokio::test]
async fn counts_changes_whose_full_patch_exceeds_the_output_limit() {
    let (_dir, tmp) = temp_dir();
    create_git_repository(&tmp);
    let store = store();
    let cwd = tmp.to_str().unwrap();
    let thread = ThreadId::new("large-checkpoint-summary");
    let (from, to) = (checkpoint_ref_for_thread_turn(&thread, 0), checkpoint_ref_for_thread_turn(&thread, 1));
    let lines = 20_000;
    std::fs::write(tmp.join("README.md"), format!("{}\n", "before".repeat(50)).repeat(lines)).unwrap();
    store.capture_checkpoint(cwd, &from).await.unwrap();
    std::fs::write(tmp.join("README.md"), format!("{}\n", "after".repeat(60)).repeat(lines)).unwrap();
    store.capture_checkpoint(cwd, &to).await.unwrap();
    let numstat = store.diff_checkpoints(&input(cwd, &from, &to, false, DiffFormat::Numstat)).await.unwrap();
    assert_eq!(
        parse_turn_diff_files_from_numstat(&numstat),
        summary(&[("README.md", lines as u64, lines as u64)])
    );
    assert!(numstat.len() < 100);
}

#[tokio::test]
async fn preserves_file_paths_and_turn_ranges_without_changing_the_user_index() {
    let (_dir, tmp) = temp_dir();
    create_git_repository(&tmp);
    git(&tmp, &["config", "diff.renames", "copies"]);
    let store = store();
    let cwd = tmp.to_str().unwrap();
    let thread = ThreadId::new("checkpoint-summary-paths");
    let (baseline, first, second) = (
        checkpoint_ref_for_thread_turn(&thread, 0),
        checkpoint_ref_for_thread_turn(&thread, 1),
        checkpoint_ref_for_thread_turn(&thread, 2),
    );
    let copied: String = (0..20).map(|i| format!("copy line {i}\n")).collect();
    let renamed = "renamed\tcafé\nname.txt";
    let added = "new\tfile\n名.txt";
    for (path, contents) in [
        ("copy-source.txt", copied.clone()),
        ("deleted.txt", "delete me\n".to_owned()),
        ("rename-old.txt", "before\nkeep one\nkeep two\nkeep three\n".to_owned()),
        ("binary.bin", "\0before".to_owned()),
    ] {
        std::fs::write(tmp.join(path), contents).unwrap();
    }
    store.capture_checkpoint(cwd, &baseline).await.unwrap();
    std::fs::rename(tmp.join("rename-old.txt"), tmp.join(renamed)).unwrap();
    std::fs::remove_file(tmp.join("deleted.txt")).unwrap();
    for (path, contents) in [
        ("copy-source.txt", format!("{copied}one more\n")),
        ("copied.txt", copied.clone()),
        (renamed, "after\nkeep one\nkeep two\nkeep three\n".to_owned()),
        ("binary.bin", "\0after".to_owned()),
        ("empty.txt", String::new()),
        (added, "first\nsecond\n".to_owned()),
    ] {
        std::fs::write(tmp.join(path), contents).unwrap();
    }
    store.capture_checkpoint(cwd, &first).await.unwrap();
    let user_index = std::fs::read(tmp.join(".git/index")).unwrap();
    let first_summary = parse_turn_diff_files_from_numstat(
        &store
            .diff_checkpoints(&input(cwd, &baseline, &first, false, DiffFormat::Numstat))
            .await
            .unwrap(),
    );
    let expected = summary(&[
        ("binary.bin", 0, 0),
        ("copied.txt", 0, 0),
        ("copy-source.txt", 1, 0),
        ("deleted.txt", 0, 1),
        ("empty.txt", 0, 0),
        (added, 2, 0),
        (renamed, 1, 1),
    ]);
    assert_eq!(first_summary, expected);

    std::fs::remove_file(tmp.join("empty.txt")).unwrap();
    std::fs::write(tmp.join("copy-source.txt"), "replacement\n").unwrap();
    store.capture_checkpoint(cwd, &second).await.unwrap();
    let second_summary = parse_turn_diff_files_from_numstat(&store.diff_checkpoints(&input(cwd, &first, &second, false, DiffFormat::Numstat)).await.unwrap());
    assert_eq!(second_summary, summary(&[("copy-source.txt", 1, 21), ("empty.txt", 0, 0)]));

    let inclusive = parse_turn_diff_files_from_numstat(
        &store
            .diff_checkpoints(&input(cwd, &baseline, &second, false, DiffFormat::Numstat))
            .await
            .unwrap(),
    );
    let expected_inclusive: Vec<_> = expected
        .into_iter()
        .filter(|f| f.path != "empty.txt")
        .map(|mut f| {
            if f.path == "copy-source.txt" {
                f.additions = 1;
                f.deletions = 20;
            }
            f
        })
        .collect();
    assert_eq!(inclusive, expected_inclusive);
    assert_eq!(
        store
            .diff_checkpoints(&input(cwd, &baseline, &baseline, false, DiffFormat::Numstat))
            .await
            .unwrap(),
        ""
    );
    assert_eq!(std::fs::read(tmp.join(".git/index")).unwrap(), user_index);
}

#[tokio::test]
async fn uses_head_for_a_missing_baseline_only_when_requested() {
    let (_dir, tmp) = temp_dir();
    create_git_repository(&tmp);
    let store = store();
    let cwd = tmp.to_str().unwrap();
    let thread = ThreadId::new("checkpoint-summary-fallback");
    let (from, to) = (checkpoint_ref_for_thread_turn(&thread, 0), checkpoint_ref_for_thread_turn(&thread, 1));
    std::fs::write(tmp.join("README.md"), "changed\n").unwrap();
    store.capture_checkpoint(cwd, &to).await.unwrap();
    let error = store.diff_checkpoints(&input(cwd, &from, &to, false, DiffFormat::Numstat)).await.unwrap_err();
    assert_eq!(error.tag(), "VcsProcessExitError");
    let mut fallback = input(cwd, &from, &to, false, DiffFormat::Numstat);
    fallback.fallback_from_to_head = true;
    let numstat = store.diff_checkpoints(&fallback).await.unwrap();
    assert_eq!(parse_turn_diff_files_from_numstat(&numstat), summary(&[("README.md", 1, 1)]));
}

// ---------------------------------------------------------------------------------------------
// CheckpointDiffQuery

#[derive(Default)]
struct ScriptedStore {
    diffs: Mutex<Vec<DiffCheckpointsInput>>,
    has_ref_calls: Mutex<usize>,
}

#[async_trait]
impl CheckpointStore for ScriptedStore {
    async fn is_git_repository(&self, _: &str) -> Result<bool, VcsError> {
        Ok(true)
    }
    async fn capture_checkpoint(&self, _: &str, _: &CheckpointRef) -> Result<(), VcsError> {
        Ok(())
    }
    async fn has_checkpoint_ref(&self, _: &str, _: &CheckpointRef) -> Result<bool, VcsError> {
        *self.has_ref_calls.lock().unwrap() += 1;
        Ok(true)
    }
    async fn restore_checkpoint(&self, _: &str, _: &CheckpointRef, _: bool) -> Result<bool, VcsError> {
        Ok(true)
    }
    async fn diff_checkpoints(&self, input: &DiffCheckpointsInput) -> Result<String, VcsError> {
        self.diffs.lock().unwrap().push(input.clone());
        Ok("diff patch".into())
    }
    async fn delete_checkpoint_refs(&self, _: &str, _: &[CheckpointRef]) -> Result<(), VcsError> {
        Ok(())
    }
}

/// Projection reads answering only the two checkpoint contexts (everything else must not be
/// asked for).
#[derive(Default)]
struct ContextReads {
    thread: Option<ThreadCheckpointContext>,
    full: Option<FullThreadDiffContext>,
    thread_calls: Mutex<usize>,
    full_calls: Mutex<usize>,
}

mod reads {
    use super::*;
    use zc_contracts::*;
    use zc_ports::orchestration::*;
    use zc_ports::{ProjectionReads, TaggedError};

    #[async_trait]
    impl ProjectionReads for ContextReads {
        async fn get_user_input_activity(&self, _: &ThreadId, _: &ApprovalRequestId) -> Result<Option<OrchestrationThreadActivity>, TaggedError> {
            unused()
        }
        async fn list_activities_by_kind(&self, _: &str) -> Result<Vec<OrchestrationThreadActivity>, TaggedError> {
            unused()
        }
        async fn get_command_read_model(&self) -> Result<OrchestrationReadModel, TaggedError> {
            unused()
        }
        async fn get_snapshot(&self) -> Result<OrchestrationReadModel, TaggedError> {
            unused()
        }
        async fn get_shell_snapshot(&self, _: bool) -> Result<OrchestrationShellSnapshot, TaggedError> {
            unused()
        }
        async fn get_archived_shell_snapshot(&self) -> Result<OrchestrationShellSnapshot, TaggedError> {
            unused()
        }
        async fn list_threads_with_pull_requests(&self) -> Result<Vec<zc_ports::orchestration::ThreadPullRequests>, TaggedError> {
            unused()
        }
        async fn get_deleted_worktree_threads(&self) -> Result<Vec<DeletedWorktreeThread>, TaggedError> {
            unused()
        }
        async fn search_threads(&self, _: OrchestrationSearchThreadsInput) -> Result<OrchestrationSearchThreadsResult, TaggedError> {
            unused()
        }
        async fn get_snapshot_sequence(&self) -> Result<i64, TaggedError> {
            Ok(0)
        }
        async fn get_counts(&self) -> Result<SnapshotCounts, TaggedError> {
            unused()
        }
        async fn get_event_replay_stats(&self, _: i64, _: i64) -> Result<ReplayStats, TaggedError> {
            unused()
        }
        async fn get_active_project_by_workspace_root(&self, _: &str) -> Result<Option<OrchestrationProject>, TaggedError> {
            Ok(None)
        }
        async fn get_project_shell_by_id(&self, _: &ProjectId) -> Result<Option<OrchestrationProjectShell>, TaggedError> {
            Ok(None)
        }
        async fn get_project_shells(&self, _: Option<Vec<ProjectId>>) -> Result<Vec<OrchestrationProjectShell>, TaggedError> {
            unused()
        }
        async fn get_first_active_thread_id_by_project_id(&self, _: &ProjectId) -> Result<Option<ThreadId>, TaggedError> {
            Ok(None)
        }
        async fn get_imported_agent_session_sources(&self, _: &ProjectId) -> Result<Vec<ImportedAgentSessionSource>, TaggedError> {
            unused()
        }
        async fn get_thread_checkpoint_context(&self, _: &ThreadId) -> Result<Option<ThreadCheckpointContext>, TaggedError> {
            *self.thread_calls.lock().unwrap() += 1;
            Ok(self.thread.clone())
        }
        async fn get_full_thread_diff_context(&self, _: &ThreadId, _: i64) -> Result<Option<FullThreadDiffContext>, TaggedError> {
            *self.full_calls.lock().unwrap() += 1;
            Ok(self.full.clone())
        }
        async fn get_thread_shell_by_id(&self, _: &ThreadId) -> Result<Option<OrchestrationThreadShell>, TaggedError> {
            Ok(None)
        }
        async fn get_thread_runtime_context(&self, _: &ThreadId) -> Result<Option<zc_ports::orchestration::ThreadRuntimeContext>, TaggedError> {
            unused()
        }
        async fn get_turn_start_message(&self, _: &ThreadId, _: &MessageId) -> Result<Option<TurnStartMessage>, TaggedError> {
            unused()
        }
        async fn get_thread_detail_by_id(&self, _: &ThreadId, _: ThreadDetailQuery) -> Result<Option<OrchestrationThread>, TaggedError> {
            Ok(None)
        }
        async fn get_thread_detail_snapshot(
            &self,
            _: &ThreadId,
            _: Option<OrchestrationThreadDetailWindow>,
        ) -> Result<Option<OrchestrationThreadDetailSnapshot>, TaggedError> {
            Ok(None)
        }
    }
}

fn thread_context(thread_id: &ThreadId, worktree: Option<&str>, turn_count: i64, reference: &CheckpointRef) -> ThreadCheckpointContext {
    ThreadCheckpointContext {
        thread_id: thread_id.clone(),
        project_id: ProjectId::new("project-1"),
        workspace_root: "/tmp/workspace".into(),
        worktree_path: worktree.map(str::to_owned),
        checkpoints: vec![decode(json!({
            "turnId": "turn-1", "checkpointTurnCount": turn_count, "checkpointRef": reference, "status": "ready", "files": [],
            "assistantMessageId": null, "completedAt": NOW,
        }))],
    }
}

#[tokio::test]
async fn uses_the_narrow_full_thread_context_lookup_for_all_turns_diffs() {
    let thread_id = ThreadId::new("thread-full-thread");
    let to = checkpoint_ref_for_thread_turn(&thread_id, 4);
    let reads = Arc::new(ContextReads {
        full: Some(FullThreadDiffContext {
            thread_id: thread_id.clone(),
            project_id: ProjectId::new("project-full-thread"),
            workspace_root: "/tmp/workspace".into(),
            worktree_path: Some("/tmp/worktree".into()),
            latest_checkpoint_turn_count: 4,
            to_checkpoint_ref: Some(to.clone()),
        }),
        ..ContextReads::default()
    });
    let store = Arc::new(ScriptedStore::default());
    let query = CheckpointDiffQuery::new(reads.clone(), store.clone());
    let result = query
        .get_full_thread_diff(&OrchestrationGetFullThreadDiffInput {
            thread_id: thread_id.clone(),
            to_turn_count: 4,
            ignore_whitespace: Some(true),
        })
        .await
        .unwrap();
    assert_eq!(*reads.thread_calls.lock().unwrap(), 0);
    assert_eq!(*reads.full_calls.lock().unwrap(), 1);
    let calls = store.diffs.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].cwd, "/tmp/worktree");
    assert_eq!(calls[0].from_checkpoint_ref, checkpoint_ref_for_thread_turn(&thread_id, 0));
    assert_eq!(calls[0].to_checkpoint_ref, to);
    assert!(calls[0].ignore_whitespace);
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        json!({"threadId": "thread-full-thread", "fromTurnCount": 0, "toTurnCount": 4, "diff": "diff patch"})
    );
}

#[tokio::test]
async fn computes_diffs_using_canonical_turn_0_refs_defaults_to_hiding_whitespace_and_does_not_preflight() {
    let thread_id = ThreadId::new("thread-1");
    let to = checkpoint_ref_for_thread_turn(&thread_id, 1);
    let reads = Arc::new(ContextReads {
        thread: Some(thread_context(&thread_id, None, 1, &to)),
        ..ContextReads::default()
    });
    let store = Arc::new(ScriptedStore::default());
    let query = CheckpointDiffQuery::new(reads, store.clone());
    let result = query
        .get_turn_diff(&OrchestrationGetTurnDiffInput {
            from_turn_count: 0,
            to_turn_count: 1,
            thread_id: thread_id.clone(),
            ignore_whitespace: None,
        })
        .await
        .unwrap();
    let calls = store.diffs.lock().unwrap().clone();
    assert_eq!(calls[0].cwd, "/tmp/workspace");
    assert_eq!(calls[0].from_checkpoint_ref, checkpoint_ref_for_thread_turn(&thread_id, 0));
    assert_eq!(calls[0].to_checkpoint_ref, to);
    assert!(calls[0].ignore_whitespace);
    assert_eq!(*store.has_ref_calls.lock().unwrap(), 0);
    assert_eq!(result.diff, "diff patch");
    assert_eq!((result.from_turn_count, result.to_turn_count), (0, 1));

    // Equal turn counts answer an empty diff without reading anything.
    let empty = query
        .get_turn_diff(&OrchestrationGetTurnDiffInput {
            from_turn_count: 2,
            to_turn_count: 2,
            thread_id: thread_id.clone(),
            ignore_whitespace: None,
        })
        .await
        .unwrap();
    assert_eq!(empty.diff, "");

    // Beyond the latest checkpoint, and with a missing from-ref.
    let error = query
        .get_turn_diff(&OrchestrationGetTurnDiffInput {
            from_turn_count: 0,
            to_turn_count: 3,
            thread_id: thread_id.clone(),
            ignore_whitespace: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.tag(), "CheckpointTurnRangeUnavailableError");
    assert_eq!(
        error.message(),
        "Checkpoint unavailable for thread thread-1 turn 3: Turn diff range exceeds current turn count: requested 3, current 1."
    );
}

#[tokio::test]
async fn fails_when_the_thread_is_missing_from_the_snapshot() {
    let query = CheckpointDiffQuery::new(Arc::new(ContextReads::default()), Arc::new(ScriptedStore::default()));
    let error = query
        .get_turn_diff(&OrchestrationGetTurnDiffInput {
            from_turn_count: 0,
            to_turn_count: 1,
            thread_id: ThreadId::new("thread-missing"),
            ignore_whitespace: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(error, CheckpointServiceError::ThreadNotFound { .. }));
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        json!({"_tag": "CheckpointThreadNotFoundError", "operation": "CheckpointDiffQuery.getTurnDiff", "threadId": "thread-missing"})
    );
    assert_eq!(
        error.message(),
        "Checkpoint invariant violation in CheckpointDiffQuery.getTurnDiff: Thread 'thread-missing' not found."
    );
}
