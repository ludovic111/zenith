//! The pull request half of `git/GitManager.ts`: which change request belongs to a branch.
//!
//! - [`PullRequestLookup::lookup_status_pr`] (`lookupStatusPr`) is the `pr` of VCS status: a
//!   per-branch cache (one minute for an open PR, five for anything else, exponential backoff
//!   for failures), bypassed by an epoch that explicit refreshes bump, with the last known
//!   answer kept as the fallback of a failed lookup.
//! - [`PullRequestLookup::branch_pull_request`] (`branchPullRequest`) answers the settlement
//!   reactor for a saved branch without touching the checkout, through the same cache, and
//!   verifies the repository identity the cached answer was resolved against.
//! - The head context (`resolveBranchHeadContext`, `resolveLookupHeadContext`) decides which
//!   remote and head selectors to ask, and [`find_open_pr`](PullRequestLookup::find_open_pr) /
//!   `findLatestPrForHeadContext` ask them.
//!
//! [`PullRequestLookup`] is also VCS status' [`PullRequestStatusSource`].

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use regex::Regex;
use zc_sourcecontrol::provider::{ChangeRequestStateFilter, ListChangeRequestsInput};
use zc_sourcecontrol::SourceControlProvider;
use zc_vcs::cache::{BoundedOrderMap, OutcomeCache};
use zc_vcs::contracts::{SourceControlProviderInfo, VcsStatusChangeRequest};
use zc_vcs::driver_core::GitRemoteStatusDetails;
use zc_vcs::git_exec::{ExecuteGitInput, GitTimeout};
use zc_vcs::remote_refs::extract_branch_name_from_remote_ref;
use zc_vcs::shared_git::{detect_source_control_provider_from_remote_url, normalize_git_remote_url};
use zc_vcs::status::canonicalize_existing_path;
use zc_vcs::{GitManagerError, GitManagerServiceError, GitVcsDriver, PullRequestStatusSource};

use crate::helpers::*;
use crate::providers::{kind_str, provider_error, SourceControlProviders};
use crate::types::{BranchHeadContext, PullRequestInfo};

/// What one cached lookup decided: the latest PR (if any) and the head it was resolved for.
#[derive(Debug, Clone, PartialEq)]
pub struct PrLookupOutcome {
    pub latest: Option<PullRequestInfo>,
    pub head_context: BranchHeadContext,
}

/// The branch details a lookup is keyed by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrLookupDetails {
    pub branch: String,
    pub upstream_ref: Option<String>,
    pub default_branch: Option<String>,
    pub local_branch_exists: bool,
    /// Only for a branch without a local ref: the remote that holds it.
    pub remote_name: Option<String>,
}

/// `LastKnownPr`.
#[derive(Debug, Clone)]
struct LastKnownPr {
    pr: Option<VcsStatusChangeRequest>,
    upstream_ref: Option<String>,
    head_branch: String,
    remote_name: Option<String>,
    head_remote_url_key: Option<String>,
}

/// `resolveRemoteRepositoryContext` result.
#[derive(Debug, Clone, Default)]
struct RemoteRepositoryContext {
    remote_url_key: Option<String>,
    repository_name_with_owner: Option<String>,
    owner_login: Option<String>,
}

/// `resolvePrLookupRepositoryIdentity` result.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RepositoryIdentity {
    head_remote_url_key: Option<String>,
    target_remote_url_key: Option<String>,
}

/// `GitBranchPullRequest`.
#[derive(Debug, Clone, PartialEq)]
pub struct BranchPullRequest {
    pub pull_request: VcsStatusChangeRequest,
    pub repository_key: Option<String>,
    pub updated_at: Option<String>,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
}

type LookupCache = OutcomeCache<String, PrLookupOutcome, GitManagerServiceError>;

struct Inner {
    git: GitVcsDriver,
    providers: Arc<dyn SourceControlProviders>,
    cache: LookupCache,
    epochs: Mutex<HashMap<String, u64>>,
    last_known: Mutex<BoundedOrderMap<LastKnownPr>>,
}

/// The PR lookups of GitManager.
#[derive(Clone)]
pub struct PullRequestLookup {
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn manager_error(operation: &str, cwd: &str, detail: impl Into<String>) -> GitManagerServiceError {
    GitManagerError::new(operation, cwd, detail).into()
}

impl PullRequestLookup {
    pub fn new(git: GitVcsDriver, providers: Arc<dyn SourceControlProviders>) -> Self {
        let streaks: Arc<Mutex<BoundedOrderMap<u32>>> = Arc::new(Mutex::new(BoundedOrderMap::new(PR_LOOKUP_CACHE_CAPACITY)));
        let cache = OutcomeCache::new(
            PR_LOOKUP_CACHE_CAPACITY,
            move |result: &Result<PrLookupOutcome, GitManagerServiceError>, key: &String| {
                let mut streaks = lock(&streaks);
                match result {
                    Ok(outcome) => {
                        streaks.remove(key);
                        if outcome.latest.as_ref().is_some_and(PullRequestInfo::is_open) {
                            PR_LOOKUP_CACHE_TTL
                        } else {
                            PR_LOOKUP_NO_OPEN_PR_CACHE_TTL
                        }
                    }
                    Err(_) => {
                        let streak = streaks.get(key).unwrap_or(0) + 1;
                        streaks.set(key, streak);
                        pr_lookup_failure_ttl(streak)
                    }
                }
            },
        );
        Self {
            inner: Arc::new(Inner {
                git,
                providers,
                cache,
                epochs: Mutex::new(HashMap::new()),
                last_known: Mutex::new(BoundedOrderMap::new(PR_LOOKUP_CACHE_CAPACITY)),
            }),
        }
    }

    pub fn git(&self) -> &GitVcsDriver {
        &self.inner.git
    }

    pub fn providers(&self) -> &Arc<dyn SourceControlProviders> {
        &self.inner.providers
    }

    async fn provider(&self, cwd: &str) -> Result<Arc<dyn SourceControlProvider>, GitManagerServiceError> {
        self.inner.providers.resolve(cwd).await.map_err(provider_error)
    }

    async fn read_config_nullable(&self, cwd: &str, key: &str) -> Option<String> {
        self.inner.git.read_config_value(cwd, key).await.ok().flatten()
    }

    fn epoch(&self, cwd: &str) -> u64 {
        lock(&self.inner.epochs).get(cwd).copied().unwrap_or(0)
    }

    /// `bumpPrLookupEpoch(cwd)` (`cwd` already canonical).
    pub fn bump_epoch(&self, cwd: &str) {
        *lock(&self.inner.epochs).entry(cwd.to_owned()).or_insert(0) += 1;
    }

    /// `prLookupCacheKey(cwd, details)`: NUL-joined.
    fn cache_key(&self, cwd: &str, details: &PrLookupDetails) -> String {
        [
            cwd,
            &details.branch,
            details.upstream_ref.as_deref().unwrap_or(""),
            details.default_branch.as_deref().unwrap_or(""),
            if details.local_branch_exists { "1" } else { "0" },
            details.remote_name.as_deref().unwrap_or(""),
            &self.epoch(cwd).to_string(),
        ]
        .join("\u{0}")
    }

    /// `resolveRemoteRepositoryContext(cwd, remoteName)`.
    async fn resolve_remote_repository_context(&self, cwd: &str, remote_name: Option<&str>) -> RemoteRepositoryContext {
        static HTTP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^https?://").expect("valid regex"));
        let Some(remote_name) = remote_name.filter(|name| !name.is_empty()) else {
            return RemoteRepositoryContext::default();
        };
        let remote_url = self.read_config_nullable(cwd, &format!("remote.{remote_name}.url")).await;
        let mut repository = parse_repository_name_with_owner_from_remote_url(remote_url.as_deref(), None);
        if let Some(url) = remote_url.as_deref() {
            if HTTP.is_match(url) && repository.as_deref().map_or(0, |r| r.split('/').count()) > 2 {
                let detected = detect_source_control_provider_from_remote_url(url);
                let kind: Option<String> = match detected {
                    Some(info) if kind_of(&info) == "unknown" => self.inner.providers.resolve(cwd).await.ok().map(|p| kind_str(&*p).to_owned()),
                    Some(info) => Some(kind_of(&info).to_owned()),
                    None => None,
                };
                repository = parse_repository_name_with_owner_from_remote_url(Some(url), kind.as_deref());
            }
        }
        RemoteRepositoryContext {
            remote_url_key: remote_url.as_deref().map(normalize_git_remote_url),
            owner_login: parse_repository_owner_login(repository.as_deref()),
            repository_name_with_owner: repository,
        }
    }

    /// `resolvePrLookupRepositoryIdentity(cwd, branch, remoteNameOverride?)`.
    async fn resolve_repository_identity(&self, cwd: &str, branch: &str, remote_name_override: Option<String>) -> RepositoryIdentity {
        let remote_name = match remote_name_override {
            Some(name) => Some(name),
            None => self.read_config_nullable(cwd, &format!("branch.{branch}.remote")).await,
        };
        let (head, target) = tokio::join!(
            self.resolve_remote_repository_context(cwd, remote_name.as_deref()),
            self.resolve_remote_repository_context(cwd, Some("origin")),
        );
        RepositoryIdentity {
            head_remote_url_key: head
                .remote_url_key
                .or_else(|| if remote_name.is_none() { target.remote_url_key.clone() } else { None }),
            target_remote_url_key: target.remote_url_key,
        }
    }

    /// `resolveBranchHeadContext(cwd, {branch, upstreamRef, remoteName?})`.
    pub async fn resolve_branch_head_context(&self, cwd: &str, branch: &str, upstream_ref: Option<&str>, remote_name: Option<&str>) -> BranchHeadContext {
        let remote_name = match remote_name {
            Some(name) => Some(name.to_owned()),
            None => self.read_config_nullable(cwd, &format!("branch.{branch}.remote")).await,
        };
        let head_from_upstream = upstream_ref
            .filter(|upstream| !upstream.is_empty())
            .map(|upstream| extract_branch_name_from_remote_ref(upstream, remote_name.as_deref(), &[]))
            .unwrap_or_default();
        let head_branch = if head_from_upstream.is_empty() {
            branch.to_owned()
        } else {
            head_from_upstream.clone()
        };
        let probe_local_selector = head_from_upstream.is_empty() || head_branch == branch;

        let (remote, origin) = tokio::join!(
            self.resolve_remote_repository_context(cwd, remote_name.as_deref()),
            self.resolve_remote_repository_context(cwd, Some("origin")),
        );
        let is_cross_repository = match (&remote.repository_name_with_owner, &origin.repository_name_with_owner) {
            (Some(remote_repository), Some(origin_repository)) => remote_repository.to_lowercase() != origin_repository.to_lowercase(),
            _ => remote_name.as_deref().is_some_and(|name| name != "origin") && remote.repository_name_with_owner.is_some(),
        };
        let owner_selector = remote
            .owner_login
            .as_ref()
            .filter(|_| !head_branch.is_empty())
            .map(|owner| format!("{owner}:{head_branch}"));
        let alias_selector = remote_name
            .as_ref()
            .filter(|name| !name.is_empty() && !head_branch.is_empty())
            .map(|name| format!("{name}:{head_branch}"));
        let probe_remote_owned = is_cross_repository || remote_name.as_deref().is_some_and(|name| name != "origin");
        let alias_if_distinct = if alias_selector != owner_selector { alias_selector.clone() } else { None };

        let mut selectors = Vec::new();
        if is_cross_repository && probe_remote_owned {
            append_unique(&mut selectors, owner_selector.as_deref());
            append_unique(&mut selectors, alias_if_distinct.as_deref());
        }
        if probe_local_selector {
            append_unique(&mut selectors, Some(branch));
        }
        append_unique(&mut selectors, (head_branch != branch).then_some(head_branch.as_str()));
        if !is_cross_repository && probe_remote_owned {
            append_unique(&mut selectors, owner_selector.as_deref());
            append_unique(&mut selectors, alias_if_distinct.as_deref());
        }

        BranchHeadContext {
            local_branch: branch.to_owned(),
            preferred_head_selector: match (&owner_selector, is_cross_repository) {
                (Some(owner), true) => owner.clone(),
                _ => head_branch.clone(),
            },
            head_branch,
            head_selectors: selectors,
            head_remote_url_key: remote
                .remote_url_key
                .clone()
                .or_else(|| if remote_name.is_none() { origin.remote_url_key.clone() } else { None }),
            remote_name,
            target_remote_url_key: origin.remote_url_key,
            head_repository_name_with_owner: remote.repository_name_with_owner,
            head_repository_owner_login: remote.owner_login,
            is_cross_repository,
        }
    }

    /// `findRemoteTrackingRemote(cwd, branch, preferredRemoteName)`.
    async fn find_remote_tracking_remote(&self, cwd: &str, branch: &str, preferred: Option<&str>) -> Option<String> {
        if branch.is_empty() {
            return None;
        }
        let git = &self.inner.git;
        let remotes = git
            .execute(ExecuteGitInput {
                timeout: GitTimeout::Millis(5_000),
                ..ExecuteGitInput::new("GitManager.findRemoteTrackingRemote.remotes", cwd, ["remote"])
            })
            .await
            .ok()?;
        let names: Vec<String> = remotes
            .stdout
            .split('\n')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
        if names.is_empty() {
            return None;
        }
        let mut args = vec!["for-each-ref".to_owned(), "--format=%(refname)".to_owned()];
        args.extend(names.iter().map(|name| format!("refs/remotes/{name}/{branch}")));
        let refs = git
            .execute(ExecuteGitInput {
                timeout: GitTimeout::Millis(5_000),
                ..ExecuteGitInput::new("GitManager.findRemoteTrackingRemote.refs", cwd, args)
            })
            .await
            .ok()?;
        let refs: std::collections::HashSet<&str> = refs.stdout.split('\n').map(str::trim).filter(|r| !r.is_empty()).collect();
        let matching: Vec<&String> = names
            .iter()
            .filter(|name| refs.contains(format!("refs/remotes/{name}/{branch}").as_str()))
            .collect();
        if let Some(preferred) = preferred {
            if matching.iter().any(|name| name.as_str() == preferred) {
                return Some(preferred.to_owned());
            }
        }
        if matching.iter().any(|name| name.as_str() == "origin") {
            return Some("origin".into());
        }
        matching.first().map(|name| (*name).clone())
    }

    /// `resolveLookupHeadContext(cwd, details)`: a branch tracking the default branch (its
    /// base) is looked up under its own name, on the remote that holds a ref of that name.
    pub async fn resolve_lookup_head_context(&self, cwd: &str, details: &PrLookupDetails) -> (BranchHeadContext, bool) {
        let head = self
            .resolve_branch_head_context(cwd, &details.branch, details.upstream_ref.as_deref(), details.remote_name.as_deref())
            .await;
        let upstream_head_is_default = Some(head.head_branch.as_str()) == details.default_branch.as_deref()
            || (details.default_branch.is_none() && (head.head_branch == "main" || head.head_branch == "master"));
        if head.head_branch == details.branch || !upstream_head_is_default || head.is_cross_repository {
            return (head, true);
        }
        let Some(remote_name) = self.find_remote_tracking_remote(cwd, &details.branch, head.remote_name.as_deref()).await else {
            return (head, false);
        };
        let own = self.resolve_branch_head_context(cwd, &details.branch, None, Some(&remote_name)).await;
        (own, true)
    }

    /// `isUnpublishedBranch(cwd, headContext)`.
    async fn is_unpublished_branch(&self, cwd: &str, head: &BranchHeadContext) -> bool {
        if head.head_branch.is_empty() {
            return false;
        }
        let git = &self.inner.git;
        let (remote_key, merge_key) = (format!("branch.{}.remote", head.local_branch), format!("branch.{}.merge", head.local_branch));
        let (remote, merge) = tokio::join!(git.read_config_value(cwd, &remote_key), git.read_config_value(cwd, &merge_key));
        let (Ok(remote), Ok(merge)) = (remote, merge) else {
            return false;
        };
        if remote.is_some() && merge.is_some() {
            return false;
        }
        let matches_ref = |pattern: String| async move {
            git.execute(ExecuteGitInput {
                timeout: GitTimeout::Millis(5_000),
                ..ExecuteGitInput::new(
                    "GitManager.isUnpublishedBranch",
                    cwd,
                    ["for-each-ref".to_owned(), "--count=1".into(), "--format=%(refname)".into(), pattern],
                )
            })
            .await
            .map(|result| !result.stdout.trim().is_empty())
        };
        let (any, this) = tokio::join!(matches_ref("refs/remotes".into()), matches_ref(format!("refs/remotes/*/{}", head.head_branch)));
        match (any, this) {
            (Ok(any), Ok(this)) => any && !this,
            _ => false,
        }
    }

    /// `findOpenPr(cwd, headContext)`.
    pub async fn find_open_pr(&self, cwd: &str, head: &BranchHeadContext) -> Result<Option<PullRequestInfo>, GitManagerServiceError> {
        let provider = self.provider(cwd).await?;
        let kind = kind_str(&*provider);
        for selector in probeable_head_selectors(kind, &head.head_selectors) {
            let pull_requests = provider
                .list_change_requests(ListChangeRequestsInput {
                    cwd: cwd.to_owned(),
                    context: None,
                    source: None,
                    head_selector: selector,
                    state: ChangeRequestStateFilter::Open,
                    limit: Some(if kind == "github" { GITHUB_HEAD_BRANCH_PROBE_LIMIT } else { 1 }),
                })
                .await
                .map_err(provider_error)?;
            if let Some(found) = pull_requests
                .iter()
                .map(PullRequestInfo::from_change_request)
                .find(|pr| matches_branch_head_context(pr, head))
            {
                return Ok(Some(PullRequestInfo {
                    state: zc_contracts::ChangeRequestState::Open,
                    updated_at: None,
                    ..found
                }));
            }
        }
        Ok(None)
    }

    /// `findLatestPrForHeadContext(cwd, headContext)`: the latest open PR, else the latest.
    async fn find_latest_pr_for_head_context(&self, cwd: &str, head: &BranchHeadContext) -> Result<Option<PullRequestInfo>, GitManagerServiceError> {
        let provider = self.provider(cwd).await?;
        let kind = kind_str(&*provider);
        let mut by_number: Vec<PullRequestInfo> = Vec::new();
        for selector in probeable_head_selectors(kind, &head.head_selectors) {
            let pull_requests = provider
                .list_change_requests(ListChangeRequestsInput {
                    cwd: cwd.to_owned(),
                    context: None,
                    source: None,
                    head_selector: selector,
                    state: ChangeRequestStateFilter::All,
                    limit: Some(if kind == "github" { GITHUB_HEAD_BRANCH_PROBE_LIMIT } else { 20 }),
                })
                .await
                .map_err(provider_error)?;
            for pr in pull_requests.iter().map(PullRequestInfo::from_change_request) {
                if !matches_branch_head_context(&pr, head) {
                    continue;
                }
                match by_number.iter_mut().find(|existing| existing.number == pr.number) {
                    Some(existing) => *existing = pr,
                    None => by_number.push(pr),
                }
            }
        }
        // Newest `updatedAt` first, unknown dates last (stable).
        by_number.sort_by_key(|pr| std::cmp::Reverse(pr.updated_at));
        if let Some(open) = by_number.iter().find(|pr| pr.is_open()) {
            return Ok(Some(open.clone()));
        }
        Ok(by_number.into_iter().next())
    }

    /// The lookup behind the cache.
    async fn lookup(&self, cwd: String, details: PrLookupDetails) -> Result<PrLookupOutcome, GitManagerServiceError> {
        let (head_context, lookup) = self.resolve_lookup_head_context(&cwd, &details).await;
        if !lookup {
            return Ok(PrLookupOutcome { latest: None, head_context });
        }
        if details.local_branch_exists && details.upstream_ref.is_none() && self.is_unpublished_branch(&cwd, &head_context).await {
            return Ok(PrLookupOutcome { latest: None, head_context });
        }
        let latest = self.find_latest_pr_for_head_context(&cwd, &head_context).await?;
        Ok(PrLookupOutcome { latest, head_context })
    }

    async fn cached(&self, cwd: &str, key: String, details: &PrLookupDetails) -> Result<PrLookupOutcome, GitManagerServiceError> {
        let this = self.clone();
        let cwd = cwd.to_owned();
        let details = details.clone();
        self.inner.cache.get(key, move || async move { this.lookup(cwd, details).await }).await
    }

    fn remember_last_known(&self, branch_key: &str, entry: LastKnownPr) {
        lock(&self.inner.last_known).set(branch_key, entry);
    }

    /// `resolveLastKnownPr(branchKey, current)`.
    fn resolve_last_known(&self, branch_key: &str, upstream_ref: Option<&str>, head: &BranchHeadContext) -> Option<VcsStatusChangeRequest> {
        let last = lock(&self.inner.last_known).get(branch_key)?;
        if last.head_branch != head.head_branch {
            return None;
        }
        if let (Some(last_key), Some(current_key)) = (&last.head_remote_url_key, &head.head_remote_url_key) {
            return if last_key == current_key { last.pr } else { None };
        }
        if let (Some(_), Some(_), Some(last_remote), Some(current_remote)) = (&last.upstream_ref, upstream_ref, &last.remote_name, &head.remote_name) {
            return if last_remote == current_remote { last.pr } else { None };
        }
        last.pr
    }

    /// `lookupStatusPr(cwd, details, refreshMissingPullRequest)`: never fails (a failed lookup
    /// answers the last known PR of the branch).
    pub async fn lookup_status_pr_inner(
        &self,
        cwd: &str,
        branch: &str,
        upstream_ref: Option<&str>,
        default_branch: Option<&str>,
        is_default_branch: bool,
        refresh_missing_pull_request: bool,
    ) -> Option<VcsStatusChangeRequest> {
        let branch_key = format!("{cwd}\u{0}{branch}");
        let details = PrLookupDetails {
            branch: branch.to_owned(),
            upstream_ref: upstream_ref.map(str::to_owned),
            default_branch: default_branch.map(str::to_owned),
            local_branch_exists: true,
            remote_name: None,
        };
        let key = self.cache_key(cwd, &details);
        if refresh_missing_pull_request {
            if let Some(Ok(outcome)) = self.inner.cache.peek(&key) {
                if outcome.latest.is_none() {
                    self.inner.cache.invalidate(&key);
                }
            }
        }
        match self.cached(cwd, key, &details).await {
            Ok(outcome) => {
                let pr = match &outcome.latest {
                    None => None,
                    // On the default branch only an open PR is the thread's context; merged or
                    // closed matches there are usually reverse-merge history.
                    Some(latest) if is_default_branch && !latest.is_open() => None,
                    Some(latest) => Some(latest.to_status_pr()),
                };
                self.remember_last_known(
                    &branch_key,
                    LastKnownPr {
                        pr: pr.clone(),
                        upstream_ref: details.upstream_ref.clone(),
                        head_branch: outcome.head_context.head_branch.clone(),
                        remote_name: outcome.head_context.remote_name.clone(),
                        head_remote_url_key: outcome.head_context.head_remote_url_key.clone(),
                    },
                );
                pr
            }
            Err(error) => {
                // A provider failure adds its actionable detail (never its upstream cause).
                let provider_field = |key: &str| match &error {
                    GitManagerServiceError::Other(tagged) if tagged.tag == "SourceControlProviderError" => {
                        Some(tagged.fields.get(key).and_then(|value| value.as_str()).unwrap_or("unknown").to_owned())
                    }
                    _ => None,
                };
                tracing::warn!(
                    operation = "lookupStatusPr",
                    branch,
                    error_tag = %error_tag(&error),
                    provider = provider_field("provider"),
                    provider_operation = provider_field("operation"),
                    provider_command = provider_field("command"),
                    error_detail = provider_field("detail"),
                    "PR lookup failed; keeping last known PR state."
                );
                let (head, _) = self.resolve_lookup_head_context(cwd, &details).await;
                self.resolve_last_known(&branch_key, upstream_ref, &head)
            }
        }
    }

    /// `branchPullRequest({cwd, branch}, {refresh})`.
    pub async fn branch_pull_request(&self, cwd: &str, branch: &str, refresh: bool) -> Result<Option<BranchPullRequest>, GitManagerServiceError> {
        let cwd = canonicalize_existing_path(cwd).await;
        let cwd = cwd.as_str();
        let git = &self.inner.git;
        let remotes = git
            .execute(ExecuteGitInput::new("GitManager.branchPullRequest.remotes", cwd, ["remote"]))
            .await?;
        let remote_names: Vec<String> = remotes
            .stdout
            .split('\n')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
        let Some(first_remote) = remote_names.first().cloned() else {
            return Ok(None);
        };
        let branch_ref = git
            .execute(ExecuteGitInput::new(
                "GitManager.branchPullRequest.branchRef",
                cwd,
                [
                    "for-each-ref".to_owned(),
                    "--format=%(refname)%00%(upstream:short)%00%(upstream:remotename)%00%(upstream:remoteref)".into(),
                    format!("refs/heads/{branch}"),
                ],
            ))
            .await?;
        let expected = format!("refs/heads/{branch}");
        let exact = branch_ref.stdout.split('\n').find(|line| line.split('\u{0}').next() == Some(expected.as_str()));
        let fields: Vec<&str> = exact.map(|line| line.split('\u{0}').collect()).unwrap_or_default();
        let field = |index: usize| fields.get(index).copied().unwrap_or("");
        let local_branch_exists = !field(0).is_empty();
        let (saved_upstream, saved_remote, saved_remote_ref) = (field(1), field(2), field(3));
        let mut upstream_ref: Option<String> = None;
        let mut remote_name: Option<String> = None;
        if !saved_upstream.is_empty() {
            if saved_remote.is_empty() || saved_remote_ref.is_empty() {
                return Err(manager_error("branchPullRequest", cwd, format!("Saved upstream for {branch} is incomplete.")));
            }
            remote_name = Some(saved_remote.to_owned());
            let upstream_branch = saved_remote_ref.strip_prefix("refs/heads/").unwrap_or(saved_remote_ref);
            upstream_ref = Some(format!("{saved_remote}/{upstream_branch}"));
        } else if !local_branch_exists {
            let tracking = git
                .execute(ExecuteGitInput::new(
                    "GitManager.branchPullRequest.remoteTrackingRefs",
                    cwd,
                    ["for-each-ref", "--format=%(refname)", "refs/remotes"],
                ))
                .await?;
            let refs: std::collections::HashSet<&str> = tracking.stdout.split('\n').map(str::trim).filter(|r| !r.is_empty()).collect();
            let matching: Vec<&String> = remote_names
                .iter()
                .filter(|name| refs.contains(format!("refs/remotes/{name}/{branch}").as_str()))
                .collect();
            if matching.len() > 1 {
                return Err(manager_error(
                    "branchPullRequest",
                    cwd,
                    format!("Multiple remotes track {branch}. Its pull request is ambiguous."),
                ));
            }
            remote_name = matching.first().map(|name| (*name).clone());
            if let Some(remote) = &remote_name {
                upstream_ref = Some(format!("{remote}/{branch}"));
            }
        }
        let default_remote = if remote_names.iter().any(|name| name == "origin") {
            "origin".to_owned()
        } else {
            first_remote
        };
        let default_branch = git.resolve_default_branch_name(cwd, &default_remote).await.ok().flatten();
        let details = PrLookupDetails {
            branch: branch.to_owned(),
            upstream_ref,
            default_branch: default_branch.clone(),
            local_branch_exists,
            remote_name: if local_branch_exists { None } else { remote_name.clone() },
        };
        let key = self.cache_key(cwd, &details);
        if refresh && matches!(self.inner.cache.peek(&key), Some(Ok(_))) {
            // A completed turn can create a PR or reuse a merged PR's branch: refresh successful
            // answers, keep failed lookups' backoff.
            self.inner.cache.invalidate(&key);
        }
        let mut cached = self.cached(cwd, key.clone(), &details).await?;
        let identity_remote = |head: &BranchHeadContext| head.remote_name.clone().or_else(|| remote_name.clone());
        let can_verify = |head: &BranchHeadContext, identity: &RepositoryIdentity| {
            !((head.head_remote_url_key.is_some() && identity.head_remote_url_key.is_none())
                || (head.target_remote_url_key.is_some() && identity.target_remote_url_key.is_none()))
        };
        let same = |head: &BranchHeadContext, identity: &RepositoryIdentity| {
            head.head_remote_url_key == identity.head_remote_url_key && head.target_remote_url_key == identity.target_remote_url_key
        };
        let identity = self.resolve_repository_identity(cwd, branch, identity_remote(&cached.head_context)).await;
        if !can_verify(&cached.head_context, &identity) {
            return Err(manager_error(
                "branchPullRequest",
                cwd,
                format!("Repository identity for {branch} could not be verified."),
            ));
        }
        if !same(&cached.head_context, &identity) {
            self.inner.cache.invalidate(&key);
            cached = self.cached(cwd, key, &details).await?;
            let refreshed = self.resolve_repository_identity(cwd, branch, identity_remote(&cached.head_context)).await;
            if !can_verify(&cached.head_context, &refreshed) || !same(&cached.head_context, &refreshed) {
                return Err(manager_error(
                    "branchPullRequest",
                    cwd,
                    format!("Repository identity for {branch} changed during pull request lookup."),
                ));
            }
        }
        let Some(latest) = cached.latest else {
            return Ok(None);
        };
        let is_default = Some(branch) == default_branch.as_deref() || (default_branch.is_none() && (branch == "main" || branch == "master"));
        if is_default && !latest.is_open() {
            return Ok(None);
        }
        Ok(Some(BranchPullRequest {
            pull_request: latest.to_status_pr(),
            // Hosting CLIs can pick an upstream repository instead of origin: the PR URL names
            // the repository that owns it.
            repository_key: pull_request_repository_key(&latest.url),
            updated_at: latest.updated_at.map(|at| at.to_iso_string()),
            closed_at: latest.closed_at.clone(),
            merged_at: latest.merged_at.clone(),
        }))
    }
}

fn kind_of(info: &SourceControlProviderInfo) -> &'static str {
    use zc_vcs::contracts::SourceControlProviderKind as K;
    match info.kind {
        K::Github => "github",
        K::Gitlab => "gitlab",
        K::Forgejo => "forgejo",
        K::AzureDevops => "azure-devops",
        K::Bitbucket => "bitbucket",
        K::Unknown => "unknown",
    }
}

fn error_tag(error: &GitManagerServiceError) -> String {
    match error {
        GitManagerServiceError::Manager(_) => "GitManagerError".into(),
        GitManagerServiceError::Command(_) => "GitCommandError".into(),
        GitManagerServiceError::Other(error) => error.tag.clone(),
    }
}

#[async_trait]
impl PullRequestStatusSource for PullRequestLookup {
    async fn lookup_status_pr(
        &self,
        cwd: &str,
        details: &GitRemoteStatusDetails,
        refresh_missing_pull_request: bool,
    ) -> Result<Option<VcsStatusChangeRequest>, GitManagerServiceError> {
        let Some(branch) = details.branch.as_deref() else {
            return Ok(None);
        };
        Ok(self
            .lookup_status_pr_inner(
                cwd,
                branch,
                details.upstream_ref.as_deref(),
                details.default_branch.as_deref(),
                details.is_default_branch,
                refresh_missing_pull_request,
            )
            .await)
    }

    async fn invalidate(&self, cwd: &str) {
        self.bump_epoch(cwd);
    }

    async fn resolve_unknown_provider(
        &self,
        cwd: &str,
        remote_name: &str,
        remote_url: &str,
        provider: &SourceControlProviderInfo,
    ) -> Option<SourceControlProviderInfo> {
        self.inner.providers.resolve_unknown_provider(cwd, remote_name, remote_url, provider).await
    }
}
