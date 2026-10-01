//! `list` and `listStats`: the inbox across every readable host, its continuation cursors, who
//! is signed in on each host, and the line counts the listing leaves out.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::Infallible;
use std::sync::OnceLock;

use futures::FutureExt;
use regex::Regex;
use zc_contracts::{
    JsNumber, PullRequestDiffStat, PullRequestInvolvement, PullRequestListEntry, PullRequestListFilters, PullRequestListFiltersChecks,
    PullRequestListFiltersDraft, PullRequestListFiltersReview, PullRequestListInput, PullRequestListProjectError, PullRequestListResult, PullRequestListState,
    PullRequestListStatsResult, PullRequestProviderSummary, PullRequestRef, SourceControlProviderKind,
};
use zc_sourcecontrol::rate_limit::RateLimitKey;
use zc_sourcecontrol::util::js_trim;

use super::projects::{ProjectFilter, SupportedProject};
use super::rate_limited::with_rate_limit_backoff;
use super::refs::{CredRef, RefKey};
use super::routing::inherit;
use super::{
    PullRequestService, DEFAULT_REPOSITORY_LIST_LIMIT, LIST_STATS_CACHE_TTL_MS, REF_EPOCH_CAPACITY, REPOSITORY_CONCURRENCY, REPOSITORY_SEARCH_CHUNK,
    SEARCH_VISIBILITY_TTL_MS, VIEWER_CACHE_TTL_MS,
};
use crate::contract::{pull_request_provider_requirement, resolve_pull_request_author_filter};
use crate::error::{Cause, ProviderFailureReason, PullRequestError, PullRequestProviderError};
use crate::provider::{
    ListChangeRequestStatsInput, ListChangeRequestsAcrossInput, ListChangeRequestsInput, ProviderChangeRequest, ProviderHostRef, ProviderListCursor,
};
use crate::util::{first_success_of, for_each_concurrent, locale_compare, lower};

/// How a listing tells two repositories apart: the host is part of it, because the same
/// `owner/repo` on github.com and on an Enterprise install are two repositories.
pub(crate) fn list_cursor_key(host: &str, repository: &str) -> String {
    format!("{host} {}", lower(repository))
}

/// What the providers are told (`ProviderListCursor`), plus the rows already handed over at
/// exactly `updated_before` (the next read asks for that instant inclusively).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListCursor {
    pub updated_before: String,
    pub delivered: i64,
    pub seen_at: Vec<i64>,
}

impl ListCursor {
    fn provider_cursor(&self) -> ProviderListCursor {
        ProviderListCursor {
            updated_before: self.updated_before.clone(),
            delivered: self.delivered,
        }
    }

    /// The rows already sent at the boundary instant come back with an inclusive read.
    fn already_sent(&self, item: &ProviderChangeRequest) -> bool {
        item.updated_at == self.updated_before && self.seen_at.contains(&item.number)
    }
}

/// `parseListCursor`: a continuation written out rather than encoded, so it can be believed or
/// refused on sight: a timestamp of this shape, a count, and the numbers sent at the boundary.
pub(crate) fn parse_list_cursor(raw: &str) -> Option<ListCursor> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        Regex::new(r"^([0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]{1,9})?(?:Z|[+-][0-9]{2}:[0-9]{2}))\|([0-9]{1,9})\|([0-9]{1,9}(?:,[0-9]{1,9})*)?$")
            .expect("valid list cursor pattern")
    });
    let captures = pattern.captures(raw)?;
    Some(ListCursor {
        updated_before: captures.get(1)?.as_str().to_owned(),
        delivered: captures.get(2)?.as_str().parse().ok()?,
        seen_at: captures
            .get(3)
            .map(|seen| seen.as_str().split(',').filter_map(|number| number.parse().ok()).collect())
            .unwrap_or_default(),
    })
}

/// `nextListCursor`: where a repository carries on, from the slice the host just handed over
/// (before the rows already sent were dropped). `None` when the host had nothing at all:
/// repeating the cursor that produced an empty slice would ask the same question forever.
fn next_list_cursor(previous: Option<&ListCursor>, fetched: &[ProviderChangeRequest], cursor_advance: i64) -> Option<String> {
    let oldest = fetched
        .iter()
        .reduce(|left, right| if right.updated_at < left.updated_at { right } else { left })?;
    Some(list_cursor_at(previous, &oldest.updated_at, fetched.iter(), cursor_advance))
}

/// `listCursorAt`: the cursor against a boundary chosen elsewhere (a slice read across several
/// repositories carries every one of them on from the oldest row of the whole slice). The names
/// sent at the boundary carry over while the boundary has not moved.
fn list_cursor_at<'a>(previous: Option<&ListCursor>, boundary: &str, fetched: impl Iterator<Item = &'a ProviderChangeRequest>, delivered_count: i64) -> String {
    let mut seen_at: Vec<i64> = previous
        .filter(|previous| previous.updated_before == boundary)
        .map(|previous| previous.seen_at.clone())
        .unwrap_or_default();
    seen_at.extend(fetched.filter(|item| item.updated_at == boundary).map(|item| item.number));
    let delivered = previous.map_or(0, |previous| previous.delivered) + delivered_count;
    let seen = seen_at.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    format!("{boundary}|{delivered}|{seen}")
}

/// `providerDetail`: why a host is not readable, told as the thing to do about it.
fn provider_detail(error: &PullRequestProviderError) -> String {
    if !error.is_unusable() {
        return error.detail.clone();
    }
    let reason = if error.reason == ProviderFailureReason::MissingTool {
        zc_contracts::PullRequestUnavailableReason::CliMissing
    } else {
        zc_contracts::PullRequestUnavailableReason::CliUnauthenticated
    };
    pull_request_provider_requirement(error.provider, reason)
        .map(str::to_owned)
        .unwrap_or_else(|| error.detail.clone())
}

/// One host's answer to "who is signed in", which doubles as "is this host set up".
#[derive(Debug, Clone)]
pub(crate) struct ResolvedViewer {
    pub host: String,
    pub kind: SourceControlProviderKind,
    pub viewer: Option<String>,
    pub error: Option<PullRequestProviderError>,
}

/// The viewer lookup's flight key: nothing about the caller, so a listing and a press for the
/// same host share one lookup.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ViewerKey {
    pub host: String,
    pub kind: SourceControlProviderKind,
    pub roots: Vec<String>,
}

/// The positional filter part of a listing key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FiltersKey {
    draft: Option<PullRequestListFiltersDraft>,
    review: Option<PullRequestListFiltersReview>,
    checks: Option<PullRequestListFiltersChecks>,
    author: Option<String>,
    labels: Option<Vec<Vec<String>>>,
    excluded_labels: Option<Vec<String>>,
}

/// A listing's cache key: the same question keys alike however its record was assembled.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ListKey {
    listings_epoch: u64,
    state: PullRequestListState,
    involvement: Option<PullRequestInvolvement>,
    filters: Option<FiltersKey>,
    project_id: Option<String>,
    project_ids: Option<Vec<String>>,
    host: Option<String>,
    limit: Option<i64>,
    query: Option<String>,
    cursors: Option<Vec<(String, String)>>,
}

impl ListKey {
    fn new(listings_epoch: u64, input: &PullRequestListInput) -> Self {
        Self {
            listings_epoch,
            state: input.state,
            involvement: input.involvement,
            filters: input.filters.as_ref().map(|filters| FiltersKey {
                draft: filters.draft,
                review: filters.review,
                checks: filters.checks,
                author: filters.author.clone(),
                labels: filters.labels.clone(),
                excluded_labels: filters.excluded_labels.clone(),
            }),
            project_id: input.project_id.as_ref().map(|id| id.as_str().to_owned()),
            project_ids: input.project_ids.as_ref().map(|ids| {
                let mut ids: Vec<String> = ids.iter().map(|id| id.as_str().to_owned()).collect();
                ids.sort();
                ids
            }),
            host: input.host.clone(),
            limit: input.limit,
            query: input.query.clone(),
            cursors: input.cursors.as_ref().map(|cursors| {
                let mut entries: Vec<(String, String)> = cursors.iter().map(|(key, value)| (key.clone(), value.clone())).collect();
                entries.sort_by(|left, right| locale_compare(&left.0, &right.0));
                entries
            }),
        }
    }

    /// The listing input back out of its key.
    fn input(&self) -> PullRequestListInput {
        PullRequestListInput {
            state: self.state,
            involvement: self.involvement,
            filters: self.filters.as_ref().map(|filters| PullRequestListFilters {
                draft: filters.draft,
                review: filters.review,
                checks: filters.checks,
                labels: filters.labels.clone(),
                excluded_labels: filters.excluded_labels.clone(),
                author: filters.author.clone(),
            }),
            project_id: self.project_id.clone().map(zc_contracts::ProjectId::new),
            project_ids: self
                .project_ids
                .as_ref()
                .map(|ids| ids.iter().cloned().map(zc_contracts::ProjectId::new).collect()),
            host: self.host.clone(),
            limit: self.limit,
            cursors: self.cursors.as_ref().map(|entries| entries.iter().cloned().collect()),
            query: self.query.clone(),
        }
    }
}

/// A listing's line-count batch key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct StatsBatchKey {
    listings_epoch: u64,
    refs: Vec<(String, String, i64, u64)>,
}

/// One repository's slice of a listing.
struct RepositoryBatch {
    /// Which repository it came from, which is what a cursor for it is filed under.
    key: String,
    entries: Vec<PullRequestListEntry>,
    errors: Vec<PullRequestListProjectError>,
    truncated: bool,
    next_cursor: Option<String>,
}

fn unreadable(project: &SupportedProject) -> PullRequestListProjectError {
    PullRequestListProjectError {
        project_id: project.project.id.clone(),
        project_title: project.project.title.clone(),
        message: format!("{} could not be read.", project.repository),
    }
}

/// `matchesRowFilters`: the narrowings a row can be judged by from its own fields, applied here
/// rather than trusted to the host (only GitHub narrows a listing itself). Idempotent for the
/// hosts that did narrow. `checks` is the host's alone: no listed row carries its check state.
fn matches_row_filters(item: &ProviderChangeRequest, filters: Option<&PullRequestListFilters>, viewer: &str) -> bool {
    let Some(filters) = filters else {
        return true;
    };
    let labels: HashSet<String> = item.labels.iter().map(|label| lower(js_trim(&label.name))).collect();
    let holds = |label: &str| labels.contains(&lower(js_trim(label)));
    let draft = filters.draft.is_none_or(|draft| item.is_draft == (draft == PullRequestListFiltersDraft::Only));
    // Judged on the provider row: `Some(None)` is a host that summarises its reviews saying there
    // is no decision yet (what "none" asks for), `None` is a host that does not summarise at all,
    // an unjudgeable row left alone.
    let review = match (filters.review, &item.review_decision) {
        (None, _) | (_, None) => true,
        (Some(PullRequestListFiltersReview::None), Some(decision)) => decision.is_none(),
        (Some(review), Some(decision)) => decision.is_some_and(|decision| decision.as_str() == review.as_str()),
    };
    let wanted_labels = filters
        .labels
        .as_ref()
        .is_none_or(|groups| groups.iter().all(|group| group.iter().any(|label| holds(label))));
    let excluded = filters
        .excluded_labels
        .as_ref()
        .is_none_or(|excluded| !excluded.iter().any(|label| holds(label)));
    let author = filters.author.as_deref().is_none_or(|author| {
        item.author
            .as_ref()
            .is_some_and(|actor| lower(&actor.login) == lower(&resolve_pull_request_author_filter(author, Some(viewer))))
    });
    draft && review && wanted_labels && excluded && author
}

fn to_entry(project: &SupportedProject, item: &ProviderChangeRequest, viewer: &str, observed_at: i64) -> PullRequestListEntry {
    let viewer = lower(viewer);
    let author_login = item.author.as_ref().map(|author| lower(&author.login));
    PullRequestListEntry {
        stack: item.stack.clone(),
        provider: project.api.kind(),
        host: project.host.clone(),
        project_id: project.project.id.clone(),
        project_title: project.project.title.clone(),
        repository: project.repository.clone(),
        number: item.number,
        title: item.title.clone(),
        url: item.url.clone(),
        author: item.author.clone(),
        head_branch: item.head_branch.clone(),
        base_branch: item.base_branch.clone(),
        state: item.state,
        is_draft: item.is_draft,
        mergeability: item.mergeability,
        additions: item.additions,
        deletions: item.deletions,
        created_at: item.created_at.clone(),
        updated_at: item.updated_at.clone(),
        observed_at: Some(JsNumber::from(observed_at)),
        viewer_review_requested: author_login.as_deref() != Some(viewer.as_str()) && item.review_request_logins.iter().any(|login| lower(login) == viewer),
        labels: item.labels.clone(),
        review_decision: item.review_decision.flatten(),
        checks_state: item.checks_state.flatten(),
    }
}

/// What one listing call works with once the hosts have been asked who is signed in.
struct ListRun<'a> {
    input: &'a PullRequestListInput,
    involvement: PullRequestInvolvement,
    viewers: &'a BTreeMap<String, String>,
    limit: i64,
    continuation: Option<&'a HashMap<String, ListCursor>>,
}

impl ListRun<'_> {
    fn cursor_of(&self, project: &SupportedProject) -> Option<&ListCursor> {
        self.continuation.and_then(|continuation| continuation.get(&project.cursor_key))
    }

    fn entries(&self, project: &SupportedProject, items: &[&ProviderChangeRequest], viewer: &str, observed_at: i64) -> Vec<PullRequestListEntry> {
        items
            .iter()
            .filter(|item| matches_row_filters(item, self.input.filters.as_ref(), viewer))
            .map(|item| to_entry(project, item, viewer, observed_at))
            .collect()
    }
}

impl PullRequestService {
    /// `decodeCursors`: the cursors the page sent back, read once before any host is asked
    /// anything. A cursor that does not read as one this service issued refuses the whole read.
    fn decode_cursors(cursors: Option<&BTreeMap<String, String>>) -> Result<Option<HashMap<String, ListCursor>>, PullRequestError> {
        let Some(cursors) = cursors else {
            return Ok(None);
        };
        let mut decoded = HashMap::new();
        for (key, raw) in cursors {
            let Some(cursor) = parse_list_cursor(raw) else {
                return Err(PullRequestError::operation("list", "The list could not be carried on from where it left off."));
            };
            decoded.insert(key.clone(), cursor);
        }
        Ok(Some(decoded))
    }

    /// The `viewerFlights` lookup: the host's own provider, let through a pause, tried across
    /// the host's checkouts until one answers. Only a success is believed for a while.
    async fn viewer_lookup(self, key: ViewerKey) -> Result<ResolvedViewer, Infallible> {
        let registered = self
            .inner
            .registry
            .get(key.kind)
            .unwrap_or_else(|| panic!("Missing pull request provider: {}", key.kind.as_str()));
        let api = with_rate_limit_backoff(registered, &key.host, &self.inner.rate_limits, true);
        let mut attempts = Vec::with_capacity(key.roots.len());
        for cwd in &key.roots {
            attempts.push(api.get_viewer(ProviderHostRef {
                cwd: cwd.clone(),
                host: Some(key.host.clone()),
            }));
        }
        let resolved = match first_success_of(attempts).await {
            Some(Ok(viewer)) => {
                let resolved = ResolvedViewer {
                    host: key.host.clone(),
                    kind: key.kind,
                    viewer: Some(viewer),
                    error: None,
                };
                let at = self.now();
                self.inner
                    .viewers_by_host
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(key.host.clone(), (at, resolved.clone()));
                resolved
            }
            Some(Err(error)) => ResolvedViewer {
                host: key.host.clone(),
                kind: key.kind,
                viewer: None,
                error: Some(error),
            },
            None => ResolvedViewer {
                host: key.host.clone(),
                kind: key.kind,
                viewer: None,
                error: None,
            },
        };
        Ok(resolved)
    }

    async fn resolve_viewer(
        &self,
        host: &str,
        projects: &[SupportedProject],
        viewer_roots: &HashMap<String, Vec<String>>,
        allow_paused: bool,
    ) -> ResolvedViewer {
        let now = self.now();
        let held = self
            .inner
            .viewers_by_host
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(host)
            .cloned();
        if let Some((at, result)) = held {
            if now - at <= VIEWER_CACHE_TTL_MS {
                return result;
            }
        }
        let for_host: Vec<&SupportedProject> = projects.iter().filter(|project| project.host == host).collect();
        let kind = for_host[0].api.kind();
        // Every checkout on the host, not just the ones that survived de-duplication: one
        // unreadable worktree would otherwise report the whole host as signed out.
        let mut roots: Vec<String> = viewer_roots
            .get(host)
            .cloned()
            .unwrap_or_else(|| for_host.iter().map(|project| project.project.workspace_root.clone()).collect());
        roots.sort();
        roots.dedup();
        let key = ViewerKey {
            host: host.to_owned(),
            kind,
            roots,
        };
        if !allow_paused {
            // The pause holds back the callers nobody is waiting on without splitting the flight
            // they share with a press: a failed lookup is held nowhere, so letting a background
            // read through would spawn this host's CLI on every refresh and re-extend the pause.
            if let Err(paused) = self.inner.rate_limits.check(&RateLimitKey::new(kind, host), false) {
                return ResolvedViewer {
                    host: host.to_owned(),
                    kind,
                    viewer: None,
                    error: Some(
                        PullRequestProviderError::new(kind, "getViewer", ProviderFailureReason::RateLimited, paused.detail())
                            .with_retry_at(Some(paused.retry_at))
                            .with_cause(Cause::new(paused)),
                    ),
                };
            }
        }
        let this = self.clone();
        let lookup_key = key.clone();
        match self.inner.viewer_flights.get(key, move || inherit(this.viewer_lookup(lookup_key))).await {
            Ok(resolved) => resolved,
            Err(never) => match never {},
        }
    }

    /// `resolveViewers`: one viewer lookup per host (two GitHub hosts are two accounts).
    pub(crate) async fn resolve_viewers(
        &self,
        projects: &[SupportedProject],
        viewer_roots: &HashMap<String, Vec<String>>,
        allow_paused: bool,
    ) -> Vec<ResolvedViewer> {
        let mut hosts: Vec<String> = Vec::new();
        for project in projects {
            if !hosts.contains(&project.host) {
                hosts.push(project.host.clone());
            }
        }
        for_each_concurrent(hosts, REPOSITORY_CONCURRENCY, |host| async move {
            self.resolve_viewer(&host, projects, viewer_roots, allow_paused).await
        })
        .await
    }

    /// `viewerOf`: who the host says the reader is (the routed credential's viewer when there
    /// is one); `None` rather than a failure when the host cannot say.
    pub(crate) async fn viewer_of(&self, project: &SupportedProject) -> Option<String> {
        if let Some(credential) = super::routing::current_routing_credential() {
            return Some(credential.viewer);
        }
        let resolved = self.resolve_viewers(std::slice::from_ref(project), &HashMap::new(), false).await;
        resolved.into_iter().next().and_then(|resolved| resolved.viewer)
    }

    /// `requiredViewerOf`: who the host says the reader is, for the paths whose rows are keyed
    /// by it. A failed lookup is refused rather than answered as the unnamed reader; the lookup
    /// is let through a host's backoff, since the reader is waiting on every one of these paths.
    pub(crate) async fn required_viewer_of(&self, project: &SupportedProject, operation: &str) -> Result<Option<String>, PullRequestError> {
        let resolved = self.resolve_viewers(std::slice::from_ref(project), &HashMap::new(), true).await;
        match resolved.into_iter().next() {
            Some(ResolvedViewer { error: Some(error), .. }) => Err(PullRequestError::from_provider(operation, error)),
            Some(resolved) => Ok(resolved.viewer),
            None => Ok(None),
        }
    }

    /// One repository asked on its own: every host without a search across repositories, and
    /// the fallback of a batched read. One unreachable repository must not blank the page.
    async fn read_repository(&self, run: &ListRun<'_>, project: &SupportedProject) -> RepositoryBatch {
        let viewer = run.viewers.get(&project.host).cloned().unwrap_or_default();
        let cursor = run.cursor_of(project);
        let observed_at = self.now();
        let page = project
            .api
            .list_change_requests(ListChangeRequestsInput {
                cwd: project.project.workspace_root.clone(),
                repository: project.repository.clone(),
                host: project.host.clone(),
                state: run.input.state,
                involvement: run.involvement,
                viewer: viewer.clone(),
                limit: run.limit,
                query: run.input.query.clone(),
                cursor: cursor.map(ListCursor::provider_cursor),
                filters: run.input.filters.clone(),
            })
            .await;
        match page {
            Ok(page) => {
                // The boundary instant was asked for inclusively, so the rows already sent at it
                // come back with the slice; dropping them here keeps their neighbours.
                let items: Vec<&ProviderChangeRequest> = page
                    .items
                    .iter()
                    .filter(|item| cursor.is_none_or(|cursor| !cursor.already_sent(item)))
                    .collect();
                let next_cursor = if page.continues && page.truncated {
                    next_list_cursor(cursor, &page.items, page.cursor_advance.unwrap_or(items.len() as i64))
                } else {
                    None
                };
                RepositoryBatch {
                    key: project.cursor_key.clone(),
                    entries: run.entries(project, &items, &viewer, observed_at),
                    errors: Vec::new(),
                    truncated: page.truncated,
                    next_cursor,
                }
            }
            Err(_) => RepositoryBatch {
                key: project.cursor_key.clone(),
                entries: Vec::new(),
                errors: vec![unreadable(project)],
                truncated: false,
                next_cursor: None,
            },
        }
    }

    async fn read_separately(&self, run: &ListRun<'_>, chunk: &[SupportedProject]) -> Vec<RepositoryBatch> {
        for_each_concurrent(chunk.iter().cloned(), REPOSITORY_CONCURRENCY, |project: SupportedProject| async move {
            self.read_repository(run, &project).await
        })
        .await
    }

    /// One host's repositories in one read, split back up by repository (each keeps its own
    /// cursor and its own errors). A read that fails is read the long way instead.
    async fn read_together(&self, run: &ListRun<'_>, chunk: &[SupportedProject]) -> Vec<RepositoryBatch> {
        let first = &chunk[0];
        if !first.api.optional_methods().list_change_requests_across {
            return self.read_separately(run, chunk).await;
        }
        let viewer = run.viewers.get(&first.host).cloned().unwrap_or_default();
        let cursor = run.cursor_of(first);
        let observed_at = self.now();
        let page = first
            .api
            .list_change_requests_across(ListChangeRequestsAcrossInput {
                cwd: first.project.workspace_root.clone(),
                host: first.host.clone(),
                repositories: chunk.iter().map(|project| project.repository.clone()).collect(),
                state: run.input.state,
                involvement: run.involvement,
                viewer: viewer.clone(),
                limit: run.limit,
                query: run.input.query.clone(),
                cursor: cursor.map(ListCursor::provider_cursor),
                filters: run.input.filters.clone(),
            })
            .await;
        let Ok(page) = page else {
            return self.read_separately(run, chunk).await;
        };
        let now = self.now();
        let mut rows: HashMap<String, Vec<ProviderChangeRequest>> = HashMap::new();
        {
            let mut visible = self.inner.search_visible_at.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            visible.retain(|_, at| now - *at <= SEARCH_VISIBILITY_TTL_MS);
            for item in &page.items {
                rows.entry(lower(js_trim(&item.repository))).or_default().push(item.change_request.clone());
                visible.insert(search_visibility_key(&first.host, &item.repository), now);
            }
        }
        // The oldest row of the whole slice: how far every repository in it has been read,
        // including the ones that contributed nothing.
        let boundary = page
            .items
            .iter()
            .map(|item| item.change_request.updated_at.as_str())
            .reduce(|oldest, at| if at < oldest { at } else { oldest })
            .map(str::to_owned);
        let empty = Vec::new();
        let rows = &rows;
        let empty = &empty;
        for_each_concurrent(chunk.iter().cloned(), REPOSITORY_CONCURRENCY, |project: SupportedProject| {
            let fetched = rows.get(&lower(js_trim(&project.repository))).unwrap_or(empty);
            let boundary = boundary.clone();
            let viewer = viewer.clone();
            let truncated = page.truncated;
            async move {
                // GitHub does not index every repository for search (a renamed one answers for its
                // old name with silence), so a repository the search said nothing about is read on
                // its own once before it is believed, on its first slice only.
                let last_visible = self
                    .inner
                    .search_visible_at
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&search_visibility_key(&project.host, &project.repository))
                    .copied();
                let known_visible = !truncated && last_visible.is_some_and(|at| now - at <= SEARCH_VISIBILITY_TTL_MS);
                let cursor_here = run.cursor_of(&project);
                if fetched.is_empty() && cursor_here.is_none() && !known_visible {
                    return self.read_repository(run, &project).await;
                }
                let items: Vec<&ProviderChangeRequest> = fetched
                    .iter()
                    .filter(|item| cursor_here.is_none_or(|cursor| !cursor.already_sent(item)))
                    .collect();
                let next_cursor = match (&boundary, truncated) {
                    (Some(boundary), true) => Some(list_cursor_at(cursor_here, boundary, fetched.iter(), items.len() as i64)),
                    _ => None,
                };
                RepositoryBatch {
                    key: project.cursor_key.clone(),
                    entries: run.entries(&project, &items, &viewer, observed_at),
                    errors: Vec::new(),
                    truncated,
                    next_cursor,
                }
            }
        })
        .await
    }

    /// `listUncached`.
    pub(crate) async fn list_uncached(&self, input: PullRequestListInput) -> Result<PullRequestListResult, PullRequestError> {
        let involvement = input.involvement.unwrap_or(PullRequestInvolvement::All);
        // Refused whole rather than per repository: a cursor is only ever a value this service
        // issued, so one that does not read as one means the page is sending something made up.
        let continuation = Self::decode_cursors(input.cursors.as_ref())?;
        let workspace = self
            .list_workspace_projects(ProjectFilter {
                project_id: input.project_id.clone(),
                project_ids: input.project_ids.clone(),
                host: input.host.clone(),
            })
            .await?;
        let projects = workspace.supported;
        let mut project_counts: HashMap<String, i64> = HashMap::new();
        for project in &projects {
            *project_counts.entry(project.host.clone()).or_default() += 1;
        }

        let viewer_results = self.resolve_viewers(&projects, &workspace.viewer_roots, false).await;
        let mut viewers: BTreeMap<String, String> = BTreeMap::new();
        for result in &viewer_results {
            if let Some(viewer) = &result.viewer {
                viewers.insert(result.host.clone(), viewer.clone());
            }
        }

        // One summary per host, which is what the viewer lookup answers for.
        let mut providers: Vec<PullRequestProviderSummary> = viewer_results
            .iter()
            .map(|result| PullRequestProviderSummary {
                host: result.host.clone(),
                kind: result.kind,
                searches_on_host: projects
                    .iter()
                    .find(|project| project.host == result.host)
                    .is_some_and(|project| project.api.capabilities().search),
                project_count: project_counts.get(&result.host).copied().unwrap_or(1),
                configured: result.viewer.is_some(),
                detail: result.error.as_ref().map(provider_detail),
            })
            .collect();
        providers.extend(workspace.unimplemented.iter().map(|(host, (kind, project_count))| PullRequestProviderSummary {
            host: host.clone(),
            kind: *kind,
            searches_on_host: false,
            project_count: *project_count,
            configured: false,
            detail: Some("This host cannot be browsed here yet.".to_owned()),
        }));

        // A continued listing reads only the repositories it was asked to carry on with; the host
        // summaries above stay over the whole workspace.
        let selected: Vec<SupportedProject> = match &continuation {
            None => projects.clone(),
            Some(continuation) => projects
                .iter()
                .filter(|project| continuation.contains_key(&project.cursor_key))
                .cloned()
                .collect(),
        };
        let readable: Vec<SupportedProject> = selected.iter().filter(|project| viewers.contains_key(&project.host)).cloned().collect();
        let unreadable_errors: Vec<PullRequestListProjectError> =
            selected.iter().filter(|project| !viewers.contains_key(&project.host)).map(unreadable).collect();
        if readable.is_empty() {
            // No host this request covers can be read. An unusable host is the reported cause
            // when there is one, because it names the fix; only the hosts this request was going
            // to read count.
            let errors: Vec<&PullRequestProviderError> = viewer_results
                .iter()
                .filter(|result| selected.iter().any(|project| project.host == result.host))
                .filter_map(|result| result.error.as_ref())
                .collect();
            let blocking = errors.iter().find(|error| error.is_unusable()).or_else(|| errors.first());
            if let Some(blocking) = blocking {
                return Err(PullRequestError::from_provider("list", (*blocking).clone()));
            }
            return Ok(PullRequestListResult {
                viewers,
                providers,
                entries: Vec::new(),
                errors: Vec::new(),
                truncated: false,
                next_cursors: BTreeMap::new(),
            });
        }

        let run = ListRun {
            input: &input,
            involvement,
            viewers: &viewers,
            limit: input.limit.unwrap_or(DEFAULT_REPOSITORY_LIST_LIMIT),
            continuation: continuation.as_ref(),
        };
        // A host with a search across repositories is asked once for all of them; everyone else
        // once each. Repositories at different points of one listing are different questions, so
        // they are grouped by the boundary they carry on from.
        let mut together: Vec<(String, Vec<SupportedProject>)> = Vec::new();
        let mut separate: Vec<SupportedProject> = Vec::new();
        for project in &readable {
            if !project.api.optional_methods().list_change_requests_across {
                separate.push(project.clone());
                continue;
            }
            let key = format!(
                "{}\n{}",
                project.host,
                run.cursor_of(project).map(|cursor| cursor.updated_before.as_str()).unwrap_or("")
            );
            match together.iter_mut().find(|(held, _)| *held == key) {
                Some((_, group)) => group.push(project.clone()),
                None => together.push((key, vec![project.clone()])),
            }
        }
        enum Read {
            Separate(SupportedProject),
            Together(Vec<SupportedProject>),
        }
        let mut reads: Vec<Read> = separate.into_iter().map(Read::Separate).collect();
        for (_, group) in &together {
            reads.extend(group.chunks(REPOSITORY_SEARCH_CHUNK).map(|chunk| Read::Together(chunk.to_vec())));
        }
        let run = &run;
        let batches: Vec<RepositoryBatch> = for_each_concurrent(reads, REPOSITORY_CONCURRENCY, |read: Read| async move {
            match read {
                Read::Separate(project) => vec![self.read_repository(run, &project).await],
                Read::Together(chunk) => self.read_together(run, &chunk).boxed().await,
            }
        })
        .await
        .into_iter()
        .flatten()
        .collect();

        let mut next_cursors = BTreeMap::new();
        for batch in &batches {
            if let Some(cursor) = &batch.next_cursor {
                next_cursors.insert(batch.key.clone(), cursor.clone());
            }
        }
        let truncated = batches.iter().any(|batch| batch.truncated);
        let mut errors = unreadable_errors;
        let mut entries = Vec::new();
        for batch in batches {
            errors.extend(batch.errors);
            entries.extend(batch.entries);
        }
        entries.sort_by(|left, right| locale_compare(&right.updated_at, &left.updated_at));
        Ok(PullRequestListResult {
            viewers,
            providers,
            entries,
            errors,
            truncated,
            next_cursors,
        })
    }

    /// `list`: keyed positionally (cursor entries sorted), so concurrent identical reads share one
    /// host request and a further slice is its own cached answer.
    pub(crate) async fn list_cached(&self, input: PullRequestListInput) -> Result<PullRequestListResult, PullRequestError> {
        let listings_epoch = self.with_epochs(|epochs| epochs.listings);
        let key = ListKey::new(listings_epoch, &input);
        let this = self.clone();
        let lookup_key = key.clone();
        self.inner
            .list_cache
            .get(key, move || inherit(async move { this.list_uncached(lookup_key.input()).await }))
            .await
    }

    /// `listStatsUncached`: the line counts of rows already on the page, one read per host whose
    /// listing deferred them. A ref this workspace cannot serve is dropped rather than refused.
    async fn list_stats_uncached(&self, refs: Vec<PullRequestRef>) -> Result<PullRequestListStatsResult, PullRequestError> {
        if refs.is_empty() {
            return Ok(PullRequestListStatsResult { stats: Vec::new() });
        }
        let supported = self.list_workspace_projects(ProjectFilter::default()).await?.supported;
        let by_project: HashMap<&str, &SupportedProject> = supported.iter().map(|project| (project.project.id.as_str(), project)).collect();
        let mut wanted: crate::util::OrderedMap<String, (SupportedProject, i64)> = crate::util::OrderedMap::new();
        for reference in &refs {
            let Some(project) = by_project.get(reference.project_id.as_str()) else {
                continue;
            };
            // The repository travels through the client, so it is checked against the project's
            // own remote rather than handed to a provider verbatim.
            if !project.api.optional_methods().list_change_request_stats || lower(&project.repository) != lower(js_trim(&reference.repository)) {
                continue;
            }
            wanted.insert(
                format!("{} {}", project.project.id.as_str(), reference.number),
                ((*project).clone(), reference.number),
            );
        }
        let mut by_host: Vec<(String, Vec<(SupportedProject, i64)>)> = Vec::new();
        for (_, (project, number)) in wanted.iter() {
            match by_host.iter_mut().find(|(host, _)| *host == project.host) {
                Some((_, entries)) => entries.push((project.clone(), *number)),
                None => by_host.push((project.host.clone(), vec![(project.clone(), *number)])),
            }
        }
        let stats = for_each_concurrent(by_host, REPOSITORY_CONCURRENCY, |(_, entries)| async move {
            let first = &entries[0].0;
            let projects_by_repository: HashMap<String, &SupportedProject> = entries
                .iter()
                .map(|(project, number)| (format!("{} {number}", lower(&project.repository)), project))
                .collect();
            let read = first
                .api
                .list_change_request_stats(ListChangeRequestStatsInput {
                    cwd: first.project.workspace_root.clone(),
                    host: first.host.clone(),
                    change_requests: entries.iter().map(|(project, number)| (project.repository.clone(), *number)).collect(),
                })
                .await;
            // A row without its counts is a row the page already draws without them.
            match read {
                Ok(read) => read
                    .into_iter()
                    .filter_map(|stat| {
                        let project = projects_by_repository.get(&format!("{} {}", lower(&stat.repository), stat.number))?;
                        Some(PullRequestDiffStat {
                            project_id: project.project.id.clone(),
                            repository: project.repository.clone(),
                            number: stat.number,
                            additions: stat.additions,
                            deletions: stat.deletions,
                        })
                    })
                    .collect::<Vec<_>>(),
                Err(_) => Vec::new(),
            }
        })
        .await;
        Ok(PullRequestListStatsResult {
            stats: stats.into_iter().flatten().collect(),
        })
    }

    /// The key a reference's counts are held under: they belong to a change request rather than
    /// to a filtered page, and are stranded by explicit refreshes, mutations and turns.
    pub(crate) fn stats_cache_key(&self, key: RefKey) -> (u64, RefKey) {
        (self.with_epochs(|epochs| epochs.listings), key)
    }

    pub(crate) fn record_stats(&self, key: (u64, RefKey), value: PullRequestDiffStat, at: i64) {
        self.with_epochs(|epochs| {
            epochs.recent_stats.insert_last(key, (at, value));
            if epochs.recent_stats.len() > REF_EPOCH_CAPACITY {
                epochs.recent_stats.pop_first();
            }
        });
    }

    /// `listStats` over canonical references: rows already counted are answered from what is
    /// held; exact batches share in-flight reads.
    pub(crate) async fn list_stats_cached(&self, refs: Vec<PullRequestRef>) -> Result<PullRequestListStatsResult, PullRequestError> {
        if refs.is_empty() {
            return Ok(PullRequestListStatsResult { stats: Vec::new() });
        }
        let now = self.now();
        let mut held = Vec::new();
        let mut missing: crate::util::OrderedMap<(u64, RefKey), PullRequestRef> = crate::util::OrderedMap::new();
        for reference in refs {
            let key = self.stats_cache_key(self.ref_key(&CredRef::plain(reference.clone())));
            let cached = self.with_epochs(|epochs| epochs.recent_stats.get(&key).cloned());
            match cached {
                Some((at, value)) if now - at < LIST_STATS_CACHE_TTL_MS => held.push(value),
                _ => missing.insert(key, reference),
            }
        }
        if missing.is_empty() {
            return Ok(PullRequestListStatsResult { stats: held });
        }
        let batch_key = self.with_epochs(|epochs| {
            let mut refs: Vec<(String, String, i64, u64)> = missing
                .iter()
                .map(|(_, reference)| {
                    (
                        reference.project_id.as_str().to_owned(),
                        reference.repository.clone(),
                        reference.number,
                        epochs.ref_epoch(reference),
                    )
                })
                .collect();
            refs.sort_by(|left, right| locale_compare(&format!("{} {} {}", left.0, left.1, left.2), &format!("{} {} {}", right.0, right.1, right.2)));
            StatsBatchKey {
                listings_epoch: epochs.listings,
                refs,
            }
        });
        let this = self.clone();
        let lookup_key = batch_key.clone();
        let (result, at) = self
            .inner
            .list_stats_cache
            .get(batch_key, move || {
                inherit(async move {
                    let refs = lookup_key
                        .refs
                        .iter()
                        .map(|(project_id, repository, number, _)| PullRequestRef {
                            project_id: zc_contracts::ProjectId::new(project_id.clone()),
                            host: None,
                            expected_account_id: None,
                            allow_stale: None,
                            repository: repository.clone(),
                            number: *number,
                        })
                        .collect();
                    let result = this.list_stats_uncached(refs).await?;
                    Ok((result, this.now()))
                })
            })
            .await?;
        for (key, reference) in missing.iter() {
            let stat = result.stats.iter().find(|stat| {
                stat.project_id == reference.project_id && lower(&stat.repository) == lower(&reference.repository) && stat.number == reference.number
            });
            if let Some(stat) = stat {
                self.record_stats(key.clone(), stat.clone(), at);
            }
        }
        held.extend(result.stats);
        Ok(PullRequestListStatsResult { stats: held })
    }
}

fn search_visibility_key(host: &str, repository: &str) -> String {
    format!("{host}\n{}", lower(js_trim(repository)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_back_only_the_cursors_it_writes() {
        assert_eq!(
            parse_list_cursor("2026-07-02T00:00:00Z|99|7"),
            Some(ListCursor {
                updated_before: "2026-07-02T00:00:00Z".into(),
                delivered: 99,
                seen_at: vec![7],
            })
        );
        assert_eq!(parse_list_cursor("2026-07-02T00:00:00.123+02:00|0|").map(|cursor| cursor.seen_at), Some(vec![]));
        assert_eq!(parse_list_cursor("yesterday"), None);
        assert_eq!(parse_list_cursor("2026-07-02T00:00:00Z|1|٣"), None);
    }
}
