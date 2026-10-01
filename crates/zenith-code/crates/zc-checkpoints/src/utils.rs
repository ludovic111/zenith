//! `checkpointing/Utils.ts`: checkpoint ref naming and the thread workspace cwd.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use zc_contracts::{CheckpointRef, ProjectId, ThreadId};

/// Hidden refs live under this prefix: `refs/t3/checkpoints/<base64url(threadId)>/turn/<n>`.
pub const CHECKPOINT_REFS_PREFIX: &str = "refs/t3/checkpoints";

/// `checkpointRefForThreadTurn(threadId, turnCount)`.
pub fn checkpoint_ref_for_thread_turn(thread_id: &ThreadId, turn_count: i64) -> CheckpointRef {
    CheckpointRef::new(format!(
        "{CHECKPOINT_REFS_PREFIX}/{}/turn/{turn_count}",
        URL_SAFE_NO_PAD.encode(thread_id.as_str().as_bytes())
    ))
}

/// A project as `resolveThreadWorkspaceCwd` sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceProject {
    pub id: ProjectId,
    pub workspace_root: String,
}

/// `resolveThreadWorkspaceCwd({thread, projects})`: the thread's worktree, else its project's
/// workspace root. An empty worktree path counts as unset.
pub fn resolve_thread_workspace_cwd(project_id: &ProjectId, worktree_path: Option<&str>, projects: &[WorkspaceProject]) -> Option<String> {
    if let Some(worktree) = worktree_path.filter(|path| !path.is_empty()) {
        return Some(worktree.to_owned());
    }
    projects
        .iter()
        .find(|project| &project.id == project_id)
        .map(|project| project.workspace_root.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_use_unpadded_base64url_thread_ids() {
        assert_eq!(
            checkpoint_ref_for_thread_turn(&ThreadId::new("thread-1"), 3).as_str(),
            "refs/t3/checkpoints/dGhyZWFkLTE/turn/3"
        );
        // Effect's encodeBase64Url: `-`/`_` alphabet, no padding.
        assert_eq!(
            checkpoint_ref_for_thread_turn(&ThreadId::new("ab?>"), 0).as_str(),
            "refs/t3/checkpoints/YWI_Pg/turn/0"
        );
    }

    #[test]
    fn worktree_wins_over_project_root() {
        let projects = vec![WorkspaceProject {
            id: ProjectId::new("p"),
            workspace_root: "/repo".into(),
        }];
        assert_eq!(
            resolve_thread_workspace_cwd(&ProjectId::new("p"), Some("/wt"), &projects).as_deref(),
            Some("/wt")
        );
        assert_eq!(resolve_thread_workspace_cwd(&ProjectId::new("p"), None, &projects).as_deref(), Some("/repo"));
        assert_eq!(
            resolve_thread_workspace_cwd(&ProjectId::new("p"), Some(""), &projects).as_deref(),
            Some("/repo")
        );
        assert_eq!(resolve_thread_workspace_cwd(&ProjectId::new("q"), None, &projects), None);
    }
}
