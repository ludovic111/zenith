//! Which projects the service can read (`listWorkspaceProjects`, `refineUnknownProjectKinds`) and
//! which one serves a reference (`requireProject`, `canonicalRef`).

use std::collections::HashMap;
use std::sync::Arc;

use zc_contracts::{
    OrchestrationProjectShell, ProjectId, PullRequestRef, PullRequestUnavailableReason, RepositoryIdentity, SourceControlProviderInfo,
    SourceControlProviderKind,
};
use zc_db::pr_keys::canonical_repository_key;
use zc_sourcecontrol::registry::detect_provider_from_remote_url;
use zc_sourcecontrol::util::{js_trim, parse_url, url_host};
use zc_sourcecontrol::SourceControlProviderContext;
use zc_vcs::shared_git::{is_ssh_remote_url, normalize_git_remote_url};

use super::listing::list_cursor_key;
use super::rate_limited::with_rate_limit_backoff;
use super::{PullRequestService, REPOSITORY_CONCURRENCY};
use crate::contract::pull_request_host_of;
use crate::error::{Cause, PullRequestError};
use crate::provider::SharedProvider;
use crate::util::{first_success_of, for_each_concurrent, lower, OrderedMap};

/// A project this service can read: its remote is on a host with an implementation.
#[derive(Clone)]
pub struct SupportedProject {
    /// What a listing cursor of its repository is filed under.
    pub cursor_key: String,
    pub project: Arc<OrchestrationProjectShell>,
    /// The host's provider, behind its rate limit.
    pub api: SharedProvider,
    pub repository: String,
    /// The host the repository lives on: the account boundary, rather than the kind.
    pub host: String,
    /// The identity's canonical key, which this environment's own records are keyed by (unique
    /// where `repository` is not: Azure's is a bare name that repeats across an organisation).
    pub remote: String,
}

impl std::fmt::Debug for SupportedProject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupportedProject")
            .field("project", &self.project.id)
            .field("repository", &self.repository)
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

/// `Pick<PullRequestListInput, "projectId" | "projectIds" | "host">`.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProjectFilter {
    pub project_id: Option<ProjectId>,
    pub project_ids: Option<Vec<ProjectId>>,
    pub host: Option<String>,
}

/// What the workspace has, split by whether this build can read it.
pub(crate) struct WorkspaceProjects {
    pub supported: Vec<SupportedProject>,
    /// Hosts with no implementation, keyed by host: `(kind, project count)`.
    pub unimplemented: OrderedMap<String, (SourceControlProviderKind, i64)>,
    /// Every checkout on a host, including the ones the listing de-duplicated away: asking who is
    /// signed in is a question about the host, and any checkout can answer it.
    pub viewer_roots: HashMap<String, Vec<String>>,
}

/// The identity a project records, when it has one (`Option<Option<_>>` flattened).
pub(crate) fn identity_of(project: &OrchestrationProjectShell) -> Option<&RepositoryIdentity> {
    project.repository_identity.as_ref().and_then(Option::as_ref)
}

/// A canonical key, absent on identities persisted before it existed.
fn canonical_key_of(identity: &RepositoryIdentity) -> Option<&str> {
    Some(identity.canonical_key.as_str()).filter(|key| !key.is_empty())
}

/// `pullRequestHostOf(identity, kind)`.
fn host_of(identity: &RepositoryIdentity, kind: SourceControlProviderKind) -> String {
    pull_request_host_of(canonical_key_of(identity), Some(&identity.locator.remote_url), kind)
}

/// `sourceControlRepositorySelector(identity)`.
pub(crate) fn source_control_repository_selector(identity: Option<&RepositoryIdentity>) -> Option<String> {
    let identity = identity?;
    if identity.provider.as_deref() == Some("azure-devops") {
        if let Some(name) = identity.name.as_deref().filter(|name| !name.is_empty()) {
            return Some(name.to_owned());
        }
        let display = identity.display_name.clone().unwrap_or_default();
        return display
            .split('/')
            .rfind(|part| *part != "_git")
            .filter(|segment| !segment.is_empty())
            .map(str::to_owned);
    }
    if let Some(display) = identity.display_name.as_deref().filter(|display| !display.is_empty()) {
        return Some(display.to_owned());
    }
    match (identity.owner.as_deref(), identity.name.as_deref()) {
        (Some(owner), Some(name)) if !owner.is_empty() && !name.is_empty() => Some(format!("{owner}/{name}")),
        _ => None,
    }
}

/// The identity's provider kind (`identity.provider as SourceControlProviderKind`).
fn kind_of(identity: &RepositoryIdentity) -> Option<SourceControlProviderKind> {
    let provider = identity.provider.as_deref()?;
    Some(
        SourceControlProviderKind::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == provider)
            .unwrap_or(SourceControlProviderKind::Unknown),
    )
}

fn needs_refinement(identity: &RepositoryIdentity) -> bool {
    match identity.provider.as_deref() {
        Some("unknown") => true,
        Some("forgejo") => is_ssh_remote_url(&identity.locator.remote_url),
        _ => false,
    }
}

struct RefinementCandidate {
    workspace_root: String,
    provider: SourceControlProviderInfo,
    remote_name: String,
    remote_url: String,
}

impl PullRequestService {
    /// `refineUnknownProjectKinds`: asks the source control providers which forge an unknown (or
    /// SSH Forgejo) remote is, once per base URL, trying each checkout until one answers.
    async fn refine_unknown_project_kinds(
        &self,
        projects: &[OrchestrationProjectShell],
        filter: &ProjectFilter,
    ) -> HashMap<String, Option<SourceControlProviderInfo>> {
        let mut refinements: OrderedMap<String, Vec<RefinementCandidate>> = OrderedMap::new();
        for project in projects {
            if filter.project_id.as_ref().is_some_and(|id| *id != project.id) {
                continue;
            }
            let Some(identity) = identity_of(project) else {
                continue;
            };
            if !needs_refinement(identity) || source_control_repository_selector(Some(identity)).is_none() {
                continue;
            }
            let host = host_of(identity, SourceControlProviderKind::Unknown);
            // A legacy identity has no canonical host until its provider is refined, so it must
            // reach the refinement before a host filter can decide whether it belongs.
            if let Some(wanted) = filter.host.as_deref() {
                let wanted = lower(wanted);
                if host != "unknown"
                    && host != wanted
                    && host_of(identity, SourceControlProviderKind::Forgejo) != wanted
                    && !is_ssh_remote_url(&identity.locator.remote_url)
                {
                    continue;
                }
            }
            let remote_url = identity.locator.remote_url.clone();
            if let Some(provider) = detect_provider_from_remote_url(&remote_url) {
                let candidate = RefinementCandidate {
                    workspace_root: project.workspace_root.clone(),
                    provider: provider.clone(),
                    remote_name: identity.locator.remote_name.clone(),
                    remote_url,
                };
                match refinements.get_mut(&provider.base_url) {
                    Some(candidates) => candidates.push(candidate),
                    None => refinements.insert(provider.base_url.clone(), vec![candidate]),
                }
            }
        }
        let groups: Vec<(String, Vec<RefinementCandidate>)> = {
            let keys: Vec<String> = refinements.keys().cloned().collect();
            keys.into_iter()
                .map(|key| (key.clone(), refinements.remove(&key).unwrap_or_default()))
                .collect()
        };
        let resolved = for_each_concurrent(groups, REPOSITORY_CONCURRENCY, |(base_url, candidates)| async move {
            let attempts = candidates.into_iter().map(|candidate| async move {
                let mut provider = candidate.provider.clone();
                if provider.kind == SourceControlProviderKind::Forgejo {
                    provider.kind = SourceControlProviderKind::Unknown;
                }
                let requested_host = filter.host.clone().filter(|_| is_ssh_remote_url(&candidate.remote_url));
                let context = SourceControlProviderContext {
                    provider,
                    remote_name: candidate.remote_name.clone(),
                    remote_url: candidate.remote_url.clone(),
                    requested_host,
                };
                let handle = self
                    .inner
                    .source_control
                    .resolve_handle(&candidate.workspace_root, Some(context))
                    .await
                    .map_err(|_| ())?;
                match handle.context.map(|context| context.provider) {
                    Some(refined) if refined.kind != SourceControlProviderKind::Unknown => Ok(refined),
                    _ => Err(()),
                }
            });
            let refined = first_success_of(attempts).await.and_then(Result::ok);
            (base_url, refined)
        })
        .await;
        resolved.into_iter().collect()
    }

    /// `listWorkspaceProjects(filter)`.
    pub(crate) async fn list_workspace_projects(&self, filter: ProjectFilter) -> Result<WorkspaceProjects, PullRequestError> {
        let read = match &filter.project_id {
            None => self.inner.projections.get_project_shells(filter.project_ids.clone()).await,
            Some(project_id) => self
                .inner
                .projections
                .get_project_shell_by_id(project_id)
                .await
                .map(|project| project.into_iter().collect()),
        };
        let projects = read.map_err(|error| {
            PullRequestError::operation("listProjects", "The project list could not be read.")
                .with_cause(Cause::new(zc_core::defect::Defect::error(&error.tag, error.message.clone())))
        })?;
        let refined_providers = self.refine_unknown_project_kinds(&projects, &filter).await;

        let mut supported = Vec::new();
        let mut unimplemented: OrderedMap<String, (SourceControlProviderKind, i64)> = OrderedMap::new();
        let mut viewer_roots: HashMap<String, Vec<String>> = HashMap::new();
        let mut seen = std::collections::HashSet::new();
        for project in projects {
            if filter.project_id.as_ref().is_some_and(|id| *id != project.id) {
                continue;
            }
            if filter.project_ids.as_ref().is_some_and(|ids| !ids.contains(&project.id)) {
                continue;
            }
            let Some(identity) = identity_of(&project).cloned() else {
                continue;
            };
            let Some(mut kind) = kind_of(&identity) else {
                continue;
            };
            let Some(repository) = source_control_repository_selector(Some(&identity)) else {
                continue;
            };
            // Worktrees of one repository are separate projects; reading the remote once keeps
            // the page from repeating every change request per checkout. The host is part of the
            // key, so the same `owner/repo` on two hosts stays two repositories.
            let mut refined_provider: Option<SourceControlProviderInfo> = None;
            if needs_refinement(&identity) {
                refined_provider = detect_provider_from_remote_url(&identity.locator.remote_url)
                    .and_then(|provider| refined_providers.get(&provider.base_url).cloned().flatten());
                if let Some(refined) = &refined_provider {
                    kind = refined.kind;
                }
            }
            let host = match &refined_provider {
                Some(refined) if refined.kind == SourceControlProviderKind::Forgejo => {
                    parse_url(&refined.base_url).map(|url| lower(&url_host(&url))).unwrap_or_default()
                }
                _ => host_of(&identity, kind),
            };
            if filter.host.as_deref().is_some_and(|wanted| host != lower(wanted)) {
                continue;
            }
            let api = self.inner.registry.get(kind);
            // Recorded before the de-duplication below, so the viewer lookup keeps the alternates
            // the listing is about to drop.
            if api.is_some() {
                let roots = viewer_roots.entry(host.clone()).or_default();
                if !roots.contains(&project.workspace_root) {
                    roots.push(project.workspace_root.clone());
                }
            }
            let key = list_cursor_key(
                &host,
                if kind == SourceControlProviderKind::AzureDevops {
                    &identity.canonical_key
                } else {
                    &repository
                },
            );
            if !seen.insert(key.clone()) {
                continue;
            }
            let Some(api) = api else {
                match unimplemented.get_mut(&host) {
                    Some((_, count)) => *count += 1,
                    None => unimplemented.insert(host, (kind, 1)),
                }
                continue;
            };
            let remote = if kind == SourceControlProviderKind::AzureDevops {
                identity.canonical_key.clone()
            } else {
                normalize_git_remote_url(&format!("https://{host}/{repository}"))
            };
            supported.push(SupportedProject {
                cursor_key: key,
                api: with_rate_limit_backoff(api, &host, &self.inner.rate_limits, false),
                project: Arc::new(project),
                repository,
                host,
                remote,
            });
        }
        Ok(WorkspaceProjects {
            supported,
            unimplemented,
            viewer_roots,
        })
    }

    /// `requireProject(ref)`: the project whose checkout and credentials serve a reference. The
    /// project's own repository is the default; a reference naming a `host` may point at any
    /// repository on that host, served by another checkout there. Azure derives its organisation
    /// from the checkout, so it needs one of the very repository.
    pub(crate) async fn require_project(&self, reference: &PullRequestRef) -> Result<SupportedProject, PullRequestError> {
        let projects = self
            .list_workspace_projects(ProjectFilter {
                project_id: Some(reference.project_id.clone()),
                ..ProjectFilter::default()
            })
            .await?;
        let own = projects.supported.into_iter().next();
        let repository = js_trim(&reference.repository).to_owned();
        let host = reference.host.as_deref().map(|host| lower(js_trim(host)));
        if let Some(own) = &own {
            if lower(&own.repository) == lower(&repository) && host.as_ref().is_none_or(|host| *host == own.host) {
                return Ok(own.clone());
            }
        }
        let Some(host) = host else {
            return Err(match own {
                None => PullRequestError::unavailable(PullRequestUnavailableReason::ProviderUnsupported),
                // The repository travels through the client, so it is checked against the
                // project's own remote rather than handed to a provider verbatim.
                Some(_) => PullRequestError::operation("resolveRepository", "The change request does not belong to the selected project."),
            });
        };
        let repository_key = canonical_repository_key(&lower(&format!("{host}/{repository}")));
        // Azure SSH and legacy clone hosts differ from the browser URL's host: compare the whole
        // repository identity before narrowing those checkouts by host.
        let filter = if repository_key.starts_with("dev.azure.com/") {
            ProjectFilter::default()
        } else {
            ProjectFilter {
                host: Some(host.clone()),
                ..ProjectFilter::default()
            }
        };
        let supported = self.list_workspace_projects(filter).await?.supported;
        let on_host: Vec<&SupportedProject> = supported.iter().filter(|candidate| candidate.host == host).collect();
        let route = supported
            .iter()
            .find(|candidate| {
                candidate.api.kind() == SourceControlProviderKind::AzureDevops
                    && identity_of(&candidate.project).is_some_and(|identity| canonical_repository_key(&lower(&identity.canonical_key)) == repository_key)
            })
            .or_else(|| {
                on_host
                    .iter()
                    .find(|candidate| candidate.api.kind() != SourceControlProviderKind::AzureDevops && lower(&candidate.repository) == lower(&repository))
                    .copied()
            })
            .or_else(|| {
                on_host
                    .iter()
                    .find(|candidate| candidate.api.kind() != SourceControlProviderKind::AzureDevops)
                    .copied()
            });
        let Some(route) = route else {
            return Err(PullRequestError::unavailable(PullRequestUnavailableReason::ProviderUnsupported));
        };
        if route.api.kind() == SourceControlProviderKind::AzureDevops || lower(&route.repository) == lower(&repository) {
            return Ok(route.clone());
        }
        Ok(SupportedProject {
            remote: normalize_git_remote_url(&format!("https://{host}/{repository}")),
            repository,
            ..route.clone()
        })
    }

    /// `canonicalRef(input)`: the reference as its serving project names it.
    pub(crate) async fn canonical_ref(&self, input: &PullRequestRef) -> Result<PullRequestRef, PullRequestError> {
        let project = self.require_project(input).await?;
        Ok(PullRequestRef {
            project_id: project.project.id.clone(),
            host: Some(project.host.clone()),
            repository: project.repository.clone(),
            ..input.clone()
        })
    }
}
