//! `git/linkCreatedPullRequest.ts`: the pull request a `create_pr`-shaped action produced is
//! linked to the thread it ran beside (`thread.pull-request.link`, source `created`).

use serde_json::json;
use zc_contracts::{OrchestrationCommand, OrchestrationProjectShell, ThreadId};
use zc_ports::{OrchestrationDispatch, ProjectionReads};

use crate::types::PrStep;

/// `CreatedPullRequestKey`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedPullRequestKey {
    pub host: String,
    pub repository: String,
    pub number: i64,
    pub url: String,
}

/// `createdPullRequestKey(result, project)`: host and repository from the URL when it can be
/// read (a fork stays host-level), else from the project's repository identity.
pub fn created_pull_request_key(pr: &PrStep, project: Option<&OrchestrationProjectShell>) -> Option<CreatedPullRequestKey> {
    if !pr.has_pull_request() {
        return None;
    }
    let number = pr.number?;
    let url = pr.url.clone().filter(|url| !url.is_empty())?;
    if let Some(parsed) = zc_db::pr_keys::parse_change_request_url(&url) {
        return Some(CreatedPullRequestKey {
            host: parsed.host,
            repository: parsed.repository,
            number,
            url,
        });
    }
    let identity = project?.repository_identity.clone().flatten()?;
    let kind = identity.provider.clone()?;
    let repository = zc_orchestration::support::source_control_repository_selector(&identity)?;
    let host = zc_orchestration::support::pull_request_host_of(&identity, Some(&kind))?;
    Some(CreatedPullRequestKey {
        host,
        repository: repository.to_lowercase(),
        number,
        url,
    })
}

/// `linkCreatedPullRequest({threadId, result, commandId})`: never fails (the action already
/// succeeded); a duplicate link is the decider saying the thread already knew.
pub async fn link_created_pull_request(engine: &dyn OrchestrationDispatch, reads: &dyn ProjectionReads, thread_id: &str, pr: &PrStep, command_id: String) {
    let thread = match reads.get_thread_shell_by_id(&ThreadId::new(thread_id)).await {
        Ok(Some(thread)) => thread,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(thread_id, cause = %error, "failed to link created pull request to thread");
            return;
        }
    };
    let project = match reads.get_project_shell_by_id(&thread.project_id).await {
        Ok(project) => project,
        Err(error) => {
            tracing::warn!(thread_id, cause = %error, "failed to link created pull request to thread");
            return;
        }
    };
    let Some(key) = created_pull_request_key(pr, project.as_ref()) else {
        return;
    };
    let command = json!({
        "type": "thread.pull-request.link",
        "commandId": command_id,
        "threadId": thread_id,
        "host": key.host,
        "repository": key.repository,
        "number": key.number,
        "url": key.url,
        "source": "created",
    });
    let command: OrchestrationCommand = match serde_json::from_value(command) {
        Ok(command) => command,
        Err(error) => {
            tracing::warn!(thread_id, %error, "failed to link created pull request to thread");
            return;
        }
    };
    match engine.dispatch(command, None).await {
        Ok(_) => {}
        Err(error) if error.is("OrchestrationCommandInvariantError") => {}
        Err(error) => tracing::warn!(thread_id, cause = %error, "failed to link created pull request to thread"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(status: &str, number: Option<i64>, url: Option<&str>) -> PrStep {
        PrStep {
            status: status.into(),
            url: url.map(str::to_owned),
            number,
            ..PrStep::skipped()
        }
    }

    pub(crate) fn project() -> OrchestrationProjectShell {
        serde_json::from_value(json!({
            "id": "project-1",
            "title": "Project",
            "workspaceRoot": "/workspace/project",
            "defaultModelSelection": null,
            "scripts": [],
            "repositoryIdentity": {
                "canonicalKey": "github.acme.test/platform/api",
                "locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": "git@github.acme.test:Platform/API.git"},
                "provider": "github",
                "displayName": "Platform/API",
                "owner": "Platform",
                "name": "API"
            },
            "createdAt": "2026-08-01T00:00:00.000Z",
            "updatedAt": "2026-08-01T00:00:00.000Z"
        }))
        .unwrap()
    }

    // linkCreatedPullRequest.test.ts: createdPullRequestKey
    #[test]
    fn reads_host_and_repository_from_a_recognisable_url() {
        assert_eq!(
            created_pull_request_key(&pr("created", Some(12), Some("https://github.com/Other/Fork/pull/12")), Some(&project())),
            Some(CreatedPullRequestKey {
                host: "github.com".into(),
                repository: "other/fork".into(),
                number: 12,
                url: "https://github.com/Other/Fork/pull/12".into(),
            })
        );
    }

    #[test]
    fn falls_back_to_the_project_for_an_unreadable_url() {
        assert_eq!(
            created_pull_request_key(&pr("opened_existing", Some(3), Some("https://ghe.internal/x/3")), Some(&project())),
            Some(CreatedPullRequestKey {
                host: "github.acme.test".into(),
                repository: "platform/api".into(),
                number: 3,
                url: "https://ghe.internal/x/3".into(),
            })
        );
        assert_eq!(created_pull_request_key(&pr("created", Some(3), Some("https://ghe.internal/x/3")), None), None);
    }

    #[test]
    fn yields_nothing_without_a_pull_request() {
        assert_eq!(created_pull_request_key(&PrStep::skipped(), Some(&project())), None);
        assert_eq!(
            created_pull_request_key(&pr("created", None, Some("https://github.com/a/b/pull/1")), Some(&project())),
            None
        );
        assert_eq!(created_pull_request_key(&pr("created", Some(1), None), Some(&project())), None);
    }
}
