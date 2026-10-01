//! `orchestration/commandInvariants.ts`: the existence checks every decision starts with.

use zc_contracts::{OrchestrationProject, OrchestrationReadModel, OrchestrationThread, ProjectId, ThreadId};

use crate::errors::CommandRejection;
use crate::support::normalize_project_path_for_comparison;

pub fn find_thread<'a>(model: &'a OrchestrationReadModel, thread_id: &ThreadId) -> Option<&'a OrchestrationThread> {
    model.threads.iter().find(|thread| &thread.id == thread_id)
}

pub fn find_project<'a>(model: &'a OrchestrationReadModel, project_id: &ProjectId) -> Option<&'a OrchestrationProject> {
    model.projects.iter().find(|project| &project.id == project_id)
}

/// `listThreadsByProjectId`.
pub fn list_threads_by_project_id<'a>(model: &'a OrchestrationReadModel, project_id: &'a ProjectId) -> impl Iterator<Item = &'a OrchestrationThread> + 'a {
    model.threads.iter().filter(move |thread| &thread.project_id == project_id)
}

/// `requireProject`.
pub fn require_project<'a>(
    model: &'a OrchestrationReadModel,
    command_type: &str,
    project_id: &ProjectId,
) -> Result<&'a OrchestrationProject, CommandRejection> {
    find_project(model, project_id)
        .ok_or_else(|| CommandRejection::invariant(command_type, format!("Project '{project_id}' does not exist for command '{command_type}'.")))
}

/// `requireProjectAbsent`.
pub fn require_project_absent(model: &OrchestrationReadModel, command_type: &str, project_id: &ProjectId) -> Result<(), CommandRejection> {
    match find_project(model, project_id) {
        None => Ok(()),
        Some(_) => Err(CommandRejection::invariant(
            command_type,
            format!("Project '{project_id}' already exists and cannot be created twice."),
        )),
    }
}

/// `requireActiveProjectWorkspaceRootAbsent`: no other live project may own the same
/// (normalized) workspace root.
pub fn require_active_project_workspace_root_absent(
    model: &OrchestrationReadModel,
    command_type: &str,
    workspace_root: &str,
    except_project_id: Option<&ProjectId>,
) -> Result<(), CommandRejection> {
    let normalized = normalize_project_path_for_comparison(workspace_root);
    let existing = model.projects.iter().find(|project| {
        project.deleted_at.is_none() && normalize_project_path_for_comparison(&project.workspace_root) == normalized && Some(&project.id) != except_project_id
    });
    match existing {
        None => Ok(()),
        Some(project) => Err(CommandRejection::invariant(
            command_type,
            format!("Active project '{}' already exists for workspace root '{normalized}'.", project.id),
        )),
    }
}

/// `requireThread`.
pub fn require_thread<'a>(model: &'a OrchestrationReadModel, command_type: &str, thread_id: &ThreadId) -> Result<&'a OrchestrationThread, CommandRejection> {
    find_thread(model, thread_id)
        .ok_or_else(|| CommandRejection::invariant(command_type, format!("Thread '{thread_id}' does not exist for command '{command_type}'.")))
}

/// `requireThreadArchived`.
pub fn require_thread_archived<'a>(
    model: &'a OrchestrationReadModel,
    command_type: &str,
    thread_id: &ThreadId,
) -> Result<&'a OrchestrationThread, CommandRejection> {
    let thread = require_thread(model, command_type, thread_id)?;
    if thread.archived_at.is_some() {
        Ok(thread)
    } else {
        Err(CommandRejection::invariant(
            command_type,
            format!("Thread '{thread_id}' is not archived for command '{command_type}'."),
        ))
    }
}

/// `requireThreadNotArchived`.
pub fn require_thread_not_archived<'a>(
    model: &'a OrchestrationReadModel,
    command_type: &str,
    thread_id: &ThreadId,
) -> Result<&'a OrchestrationThread, CommandRejection> {
    let thread = require_thread(model, command_type, thread_id)?;
    if thread.archived_at.is_none() {
        Ok(thread)
    } else {
        Err(CommandRejection::invariant(
            command_type,
            format!("Thread '{thread_id}' is already archived and cannot handle command '{command_type}'."),
        ))
    }
}

/// `requireThreadAbsent`: deletion is soft and a draft keeps its client-minted id across
/// retries, so only a live row blocks creation.
pub fn require_thread_absent(model: &OrchestrationReadModel, command_type: &str, thread_id: &ThreadId) -> Result<(), CommandRejection> {
    match find_thread(model, thread_id) {
        Some(thread) if thread.deleted_at.is_none() => Err(CommandRejection::invariant(
            command_type,
            format!("Thread '{thread_id}' already exists and cannot be created twice."),
        )),
        _ => Ok(()),
    }
}
