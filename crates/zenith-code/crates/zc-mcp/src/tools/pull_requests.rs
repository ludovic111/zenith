//! `toolkits/pullRequests`: `link_pull_request`, `unlink_pull_request`,
//! `list_thread_pull_requests` on the credential's thread, through the orchestration engine
//! (`thread.pull-request.link` / `.unlink`, `source: "agent"`) and the projection's thread shell.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use url::Url;
use zc_db::pr_keys::{canonical_repository_key, normalize_thread_pull_request_key, parse_change_request_url};
use zc_ports::{OrchestrationDispatch, ProjectionReads, TaggedError};
use zc_projections::pull_requests::{resolve_chains, thread_pull_request_key_of};

use crate::params::{optional, Bound, Field, Spec};
use crate::scope::{require_capability, McpCapability, McpInvocationScope};

/// Why a dispatch failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchFailure {
    /// `OrchestrationCommandInvariantError`: already linked, or not linked.
    Invariant,
    Other(String),
}

/// What the pull request tools read and write (`ProjectionSnapshotQuery` +
/// `OrchestrationEngineService.dispatch`), with shells in their wire form.
#[async_trait]
pub trait PullRequestBackend: Send + Sync {
    async fn thread_shell(&self, thread_id: &str) -> Result<Option<Value>, String>;
    async fn project_shell(&self, project_id: &str) -> Result<Option<Value>, String>;
    /// Dispatches an encoded `OrchestrationCommand`.
    async fn dispatch(&self, command: Value) -> Result<(), DispatchFailure>;
}

/// [`PullRequestBackend`] over the projections and the engine.
pub struct OrchestrationPullRequests {
    pub reads: Arc<dyn ProjectionReads>,
    pub engine: Arc<dyn OrchestrationDispatch>,
}

#[async_trait]
impl PullRequestBackend for OrchestrationPullRequests {
    async fn thread_shell(&self, thread_id: &str) -> Result<Option<Value>, String> {
        let id = zc_ports::contracts::ThreadId::new(thread_id);
        let shell = self.reads.get_thread_shell_by_id(&id).await.map_err(|error| error.to_string())?;
        shell.map(|shell| serde_json::to_value(&shell).map_err(|error| error.to_string())).transpose()
    }

    async fn project_shell(&self, project_id: &str) -> Result<Option<Value>, String> {
        let id = zc_ports::contracts::ProjectId::new(project_id);
        let shell = self.reads.get_project_shell_by_id(&id).await.map_err(|error| error.to_string())?;
        shell.map(|shell| serde_json::to_value(&shell).map_err(|error| error.to_string())).transpose()
    }

    async fn dispatch(&self, command: Value) -> Result<(), DispatchFailure> {
        let command: zc_ports::contracts::OrchestrationCommand = serde_json::from_value(command).map_err(|error| DispatchFailure::Other(error.to_string()))?;
        match self.engine.dispatch(command, None).await {
            Ok(_) => Ok(()),
            Err(error) if error.tag == "OrchestrationCommandInvariantError" => Err(DispatchFailure::Invariant),
            Err(error) => Err(DispatchFailure::Other(format!("{}: {}", error.tag, error.message))),
        }
    }
}

/// `PullRequestTargetInput`.
pub fn target_fields() -> Vec<Field> {
    vec![
        optional("url", Spec::Trimmed { max: None }),
        optional("repository", Spec::Trimmed { max: None }),
        optional(
            "number",
            Spec::Int {
                min: Some(Bound::Inclusive(1.0)),
                max: None,
                between: false,
            },
        ),
        optional("host", Spec::Trimmed { max: None }),
    ]
}

fn error(tag: &str, message: &str) -> TaggedError {
    TaggedError::new(tag, message)
}

fn url_invalid() -> TaggedError {
    error(
        "PullRequestUrlInvalidError",
        "This is not a recognised pull request URL. Pass repository and number instead.",
    )
}

fn target_incomplete() -> TaggedError {
    error("PullRequestTargetIncompleteError", "Pass either url, or both repository and number.")
}

fn host_required() -> TaggedError {
    error(
        "PullRequestHostRequiredError",
        "This thread's project has no recognised remote. Pass host or url.",
    )
}

fn thread_not_found(thread_id: &str) -> TaggedError {
    error("PullRequestThreadNotFoundError", &format!("Thread {thread_id} was not found.")).with("threadId", thread_id)
}

#[derive(Clone, Copy)]
enum Operation {
    Link,
    Unlink,
    List,
}

impl Operation {
    fn failure(self, cause: String) -> TaggedError {
        let (tag, message) = match self {
            Self::Link => ("PullRequestLinkFailedError", "Could not link the pull request."),
            Self::Unlink => ("PullRequestUnlinkFailedError", "Could not unlink the pull request."),
            Self::List => ("PullRequestListFailedError", "Could not list the pull request."),
        };
        error(tag, message).with("cause", cause)
    }
}

/// WHATWG `URL.host`: host plus a non-default port.
fn url_host(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

fn web_url(raw: &str) -> Option<Url> {
    Url::parse(raw).ok().filter(|url| url.scheme() == "http" || url.scheme() == "https")
}

/// `pullRequestHostOf(identity, kind)`.
fn pull_request_host_of(identity: &Value, kind: &str) -> String {
    if kind == "forgejo" {
        if let Some(remote) = identity.pointer("/locator/remoteUrl").and_then(Value::as_str).and_then(web_url) {
            return url_host(&remote).to_lowercase();
        }
    }
    let host = identity
        .get("canonicalKey")
        .and_then(Value::as_str)
        .and_then(|key| key.split('/').next())
        .map(crate::params::js_trim);
    match host {
        Some(host) if !host.is_empty() => host.to_lowercase(),
        _ => kind.to_owned(),
    }
}

/// `changeRequestUrlFor`.
fn change_request_url_for(kind: Option<&str>, host: &str, repository: &str, number: i64, remote_url: Option<&str>) -> Option<String> {
    match kind? {
        "github" => Some(format!("https://{host}/{repository}/pull/{number}")),
        "forgejo" => {
            if let Some(remote) = remote_url.and_then(web_url) {
                let hostname = remote.host_str().unwrap_or("").to_lowercase();
                if hostname == host.to_lowercase() || url_host(&remote).to_lowercase() == host.to_lowercase() {
                    let origin = format!("{}://{}", remote.scheme(), url_host(&remote));
                    return Some(format!("{origin}/{repository}/pulls/{number}"));
                }
            }
            Some(format!("https://{host}/{repository}/pulls/{number}"))
        }
        "gitlab" => Some(format!("https://{host}/{repository}/-/merge_requests/{number}")),
        "bitbucket" => Some(format!("https://{host}/{repository}/pull-requests/{number}")),
        "azure-devops" => Some(format!(
            "https://{}/pullrequest/{number}",
            canonical_repository_key(&format!("{host}/{repository}").to_lowercase())
        )),
        _ => None,
    }
}

/// A resolved target: one host-level identity plus the URL to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub host: String,
    pub repository: String,
    pub number: i64,
    pub url: String,
}

/// `resolveTarget`: a URL wins outright; otherwise repository and number, completed with the
/// thread project's host.
pub fn resolve_target(input: &Map<String, Value>, project: Option<&Value>) -> Result<ResolvedTarget, TaggedError> {
    if let Some(url) = input.get("url").and_then(Value::as_str) {
        let parsed = parse_change_request_url(url).ok_or_else(url_invalid)?;
        let key = normalize_thread_pull_request_key(&parsed.host, &parsed.repository, parsed.number, parsed.authority.as_deref(), None);
        return Ok(ResolvedTarget {
            host: key.host,
            repository: key.repository,
            number: key.number,
            url: url.to_owned(),
        });
    }
    let (Some(repository), Some(number)) = (input.get("repository").and_then(Value::as_str), input.get("number").and_then(Value::as_i64)) else {
        return Err(target_incomplete());
    };
    let identity = project
        .and_then(|project| project.get("repositoryIdentity"))
        .filter(|identity| identity.is_object());
    let kind = identity.and_then(|identity| identity.get("provider")).and_then(Value::as_str);
    let (project_host, project_kind) = match (identity, kind) {
        (Some(identity), Some(kind)) => (Some(pull_request_host_of(identity, kind)), Some(kind)),
        _ => (None, None),
    };
    let host = input
        .get("host")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| project_host.clone())
        .map(|host| host.to_lowercase())
        .ok_or_else(host_required)?;
    let repository = repository.to_lowercase();
    // The project's kind only describes its own host; another host gets no URL guess.
    let kind = if Some(&host) == project_host.as_ref() { project_kind } else { None };
    let remote_url = identity.and_then(|identity| identity.pointer("/locator/remoteUrl")).and_then(Value::as_str);
    let url = change_request_url_for(kind, &host, &repository, number, remote_url).unwrap_or_else(|| format!("https://{host}/{repository}/pull/{number}"));
    let key = normalize_thread_pull_request_key(&host, &repository, number, None, Some(&url));
    Ok(ResolvedTarget {
        host: key.host,
        repository: key.repository,
        number: key.number,
        url,
    })
}

fn link_key(link: &Value) -> String {
    thread_pull_request_key_of((
        link.get("host").and_then(Value::as_str).unwrap_or(""),
        link.get("repository").and_then(Value::as_str).unwrap_or(""),
        link.get("number").and_then(Value::as_i64).unwrap_or(0),
        link.get("url").and_then(Value::as_str),
    ))
}

/// `listThreadPullRequests`: what the tools report from a thread shell.
pub fn list_thread_pull_requests(thread: &Value) -> Value {
    let links: Vec<&Value> = thread
        .get("pullRequests")
        .and_then(Value::as_array)
        .map(|links| links.iter().collect())
        .unwrap_or_default();
    let visible: Vec<&Value> = links
        .into_iter()
        .filter(|link| link.get("source").and_then(Value::as_str) != Some("stack-dismissed"))
        .collect();
    let chains = resolve_chains(&visible);
    let kind = |native: bool| if native { "native" } else { "derived" };
    let pull_requests: Vec<Value> = visible
        .iter()
        .map(|link| {
            let key = link_key(link);
            let mut stack = Value::Null;
            for chain in &chains {
                if chain.layers.len() < 2 {
                    continue;
                }
                if let Some(index) = chain.layers.iter().position(|layer| link_key(visible[*layer]) == key) {
                    stack = json!({"kind": kind(chain.native), "position": index + 1, "size": chain.layers.len()});
                    break;
                }
            }
            let snapshot = link.get("snapshot").filter(|snapshot| !snapshot.is_null());
            let field = |name: &str| snapshot.and_then(|snapshot| snapshot.get(name)).cloned().unwrap_or(Value::Null);
            let normalized = normalize_thread_pull_request_key(
                link.get("host").and_then(Value::as_str).unwrap_or(""),
                link.get("repository").and_then(Value::as_str).unwrap_or(""),
                link.get("number").and_then(Value::as_i64).unwrap_or(0),
                None,
                link.get("url").and_then(Value::as_str),
            );
            json!({
                "host": normalized.host,
                "repository": link.get("repository").cloned().unwrap_or(Value::Null),
                "number": link.get("number").cloned().unwrap_or(Value::Null),
                "url": link.get("url").cloned().unwrap_or(Value::Null),
                "source": link.get("source").cloned().unwrap_or(Value::Null),
                "state": field("state"),
                "title": field("title"),
                "headBranch": field("headBranch"),
                "baseBranch": field("baseBranch"),
                "isDraft": field("isDraft"),
                "stack": stack,
            })
        })
        .collect();
    let chains: Vec<Value> = chains
        .iter()
        .map(|chain| {
            json!({
                "kind": kind(chain.native),
                "numbers": chain.layers.iter().map(|layer| visible[*layer].get("number").cloned().unwrap_or(Value::Null)).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({"pullRequests": pull_requests, "chains": chains})
}

/// The three handlers over a backend.
pub struct PullRequestTools {
    pub backend: Arc<dyn PullRequestBackend>,
}

impl PullRequestTools {
    async fn require_thread(&self, scope: &McpInvocationScope, operation: Operation) -> Result<Value, TaggedError> {
        require_capability(scope, McpCapability::PullRequests)?;
        match self.backend.thread_shell(&scope.thread_id).await {
            Ok(Some(thread)) => Ok(thread),
            Ok(None) => Err(thread_not_found(&scope.thread_id)),
            Err(cause) => Err(operation.failure(cause)),
        }
    }

    async fn project_of(&self, thread: &Value, operation: Operation) -> Result<Option<Value>, TaggedError> {
        let project_id = thread.get("projectId").and_then(Value::as_str).unwrap_or("");
        self.backend.project_shell(project_id).await.map_err(|cause| operation.failure(cause))
    }

    fn command_id(tag: &str, thread_id: &str) -> String {
        format!("server:{tag}:{thread_id}:{}", zc_core::ids::uuid_v4())
    }

    pub async fn link(&self, scope: &McpInvocationScope, input: &Map<String, Value>) -> Result<Value, TaggedError> {
        let thread = self.require_thread(scope, Operation::Link).await?;
        let project = self.project_of(&thread, Operation::Link).await?;
        let target = resolve_target(input, project.as_ref())?;
        let thread_id = thread.get("id").and_then(Value::as_str).unwrap_or(&scope.thread_id).to_owned();
        let command = json!({
            "type": "thread.pull-request.link",
            "commandId": Self::command_id("mcp-pr-link", &thread_id),
            "threadId": thread_id,
            "host": target.host,
            "repository": target.repository,
            "number": target.number,
            "url": target.url,
            "source": "agent",
        });
        let already_linked = match self.backend.dispatch(command).await {
            Ok(()) => false,
            // The decider rejects a second link of the same PR; for the agent that is the
            // outcome it asked for, not an error.
            Err(DispatchFailure::Invariant) => true,
            Err(DispatchFailure::Other(cause)) => return Err(Operation::Link.failure(cause)),
        };
        Ok(json!({
            "host": target.host,
            "repository": target.repository,
            "number": target.number,
            "url": target.url,
            "alreadyLinked": already_linked,
        }))
    }

    pub async fn unlink(&self, scope: &McpInvocationScope, input: &Map<String, Value>) -> Result<Value, TaggedError> {
        let thread = self.require_thread(scope, Operation::Unlink).await?;
        let project = self.project_of(&thread, Operation::Unlink).await?;
        let target = resolve_target(input, project.as_ref())?;
        let thread_id = thread.get("id").and_then(Value::as_str).unwrap_or(&scope.thread_id).to_owned();
        let command = json!({
            "type": "thread.pull-request.unlink",
            "commandId": Self::command_id("mcp-pr-unlink", &thread_id),
            "threadId": thread_id,
            "host": target.host,
            "repository": target.repository,
            "number": target.number,
        });
        let was_linked = match self.backend.dispatch(command).await {
            Ok(()) => true,
            Err(DispatchFailure::Invariant) => false,
            Err(DispatchFailure::Other(cause)) => return Err(Operation::Unlink.failure(cause)),
        };
        Ok(json!({
            "host": target.host,
            "repository": target.repository,
            "number": target.number,
            "wasLinked": was_linked,
        }))
    }

    pub async fn list(&self, scope: &McpInvocationScope) -> Result<Value, TaggedError> {
        let thread = self.require_thread(scope, Operation::List).await?;
        Ok(list_thread_pull_requests(&thread))
    }
}
