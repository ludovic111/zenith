//! `WorkspaceSearchIndex` (`workspace/WorkspaceSearchIndex.ts`): one fff index over a workspace
//! root, and the mapping of its results onto the project contracts.
//!
//! - `list`: the whole index (files and directories, ancestors filled in), `localeCompare`
//!   order, capped at 25,000 entries.
//! - `search`: fuzzy file / directory / mixed search; `imageOnly` filters files to image
//!   extensions before the limit applies (so it fetches a full page).
//! - `searchContents`: grep with fff's file cursors and a 250 ms budget across pages, at most
//!   100 matches per file, and a whole-word post-filter (VS Code's boundary rule, on UTF-16
//!   indices). Ranges come back as UTF-16 string indices, as the client slices `lineContent`.
//! - `refresh`: a full rescan, then waits for the index to be ready again.
//!
//! The finder calls are synchronous (as in the TS, where they block the event loop); here they
//! run on the blocking pool.

use std::sync::Arc;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use zc_contracts::{
    ProjectContentMatch, ProjectContentMatchRange, ProjectEntry, ProjectEntryKind, ProjectListEntriesResult, ProjectSearchContentsResult,
    ProjectSearchEntriesResult,
};

use crate::backend::{Finder, FinderError, FinderFactory, GrepMode, GrepRequest, IndexVariant, MixedSearchPage, PathSearchPage};
use crate::collate::locale_compare;
use crate::errors::SearchIndexError;
use crate::text::{byte_offset_to_utf16_index, is_whole_word_range, js_length, Utf16Line};

pub const WORKSPACE_INDEX_MAX_ENTRIES: usize = 25_000;
pub const WORKSPACE_INDEX_PAGE_SIZE: usize = WORKSPACE_INDEX_MAX_ENTRIES + 2;
pub const WORKSPACE_INDEX_SCAN_TIMEOUT: Duration = Duration::from_secs(15);
/// `WORKSPACE_INDEX_SCAN_TIMEOUT` as the TS spells it in `WorkspaceSearchIndexScanTimedOut`.
pub const WORKSPACE_INDEX_SCAN_TIMEOUT_LABEL: &str = "15 seconds";
pub const WORKSPACE_INDEX_IDLE_TTL: Duration = Duration::from_secs(15 * 60);
pub const CONTENT_SEARCH_TIME_BUDGET_MS: u64 = 250;
pub const CONTENT_SEARCH_MAX_MATCHES_PER_FILE: usize = 100;
/// How often `waitForIndexReady` polls the scan progress.
const INDEX_READY_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The content-search options (`ProjectSearchContentsInput` without `cwd`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentSearch {
    pub query: String,
    pub limit: usize,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub use_regex: bool,
}

/// One live index (`WorkspaceSearchIndex.make(cwd, variant)`).
pub struct WorkspaceSearchIndex {
    cwd: String,
    variant: IndexVariant,
    finder: Arc<dyn Finder>,
}

impl std::fmt::Debug for WorkspaceSearchIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceSearchIndex")
            .field("cwd", &self.cwd)
            .field("variant", &self.variant)
            .finish_non_exhaustive()
    }
}

fn to_posix_path(input: &str) -> String {
    input.replace('\\', "/")
}

fn trim_directory_separator(input: &str) -> &str {
    input.strip_suffix('/').unwrap_or(input)
}

fn entry(path: &str, kind: ProjectEntryKind) -> Option<ProjectEntry> {
    let normalized = to_posix_path(path);
    let normalized = trim_directory_separator(&normalized);
    (!normalized.is_empty()).then(|| ProjectEntry {
        path: normalized.to_owned(),
        kind,
        ignored: None,
    })
}

/// `isWorkspaceImagePreviewPath` (shared `filePreview.ts`).
pub fn is_workspace_image_preview_path(path: &str) -> bool {
    const EXTENSIONS: [&str; 8] = [".avif", ".gif", ".ico", ".jpeg", ".jpg", ".png", ".svg", ".webp"];
    let without_query = path.split(['?', '#']).next().unwrap_or("").to_lowercase();
    EXTENSIONS.iter().any(|extension| without_query.ends_with(extension))
}

fn map_file_search_result(result: PathSearchPage, limit: usize, image_only: bool) -> ProjectSearchEntriesResult {
    let item_count = result.items.len();
    let mut entries: Vec<ProjectEntry> = result
        .items
        .iter()
        .filter_map(|path| entry(path, ProjectEntryKind::File))
        .filter(|entry| !image_only || is_workspace_image_preview_path(&entry.path))
        .collect();
    let truncated = entries.len() > limit || result.total_matched > item_count;
    entries.truncate(limit);
    ProjectSearchEntriesResult { entries, truncated }
}

fn map_directory_search_result(result: PathSearchPage, limit: usize) -> ProjectSearchEntriesResult {
    let root_directory_count = usize::from(result.items.iter().any(|path| path.is_empty()));
    let mut entries: Vec<ProjectEntry> = result.items.iter().filter_map(|path| entry(path, ProjectEntryKind::Directory)).collect();
    entries.truncate(limit);
    ProjectSearchEntriesResult {
        entries,
        truncated: result.total_matched.saturating_sub(root_directory_count) > limit,
    }
}

fn map_mixed_search_result(result: MixedSearchPage, limit: usize) -> ProjectSearchEntriesResult {
    let mut entries = Vec::new();
    for (kind, path) in &result.items {
        if let Some(entry) = entry(path, *kind) {
            entries.push(entry);
        }
        if entries.len() >= limit {
            break;
        }
    }
    let root_directory_count = usize::from(result.items.iter().any(|(kind, path)| *kind == ProjectEntryKind::Directory && path.is_empty()));
    ProjectSearchEntriesResult {
        entries,
        truncated: result.total_matched.saturating_sub(root_directory_count) > limit,
    }
}

fn parent_path_of(input: &str) -> Option<&str> {
    input.rfind('/').map(|index| &input[..index])
}

/// `withDirectoryAncestors`: adds every missing ancestor directory, keeping first-seen order.
fn with_directory_ancestors(entries: Vec<ProjectEntry>) -> Vec<ProjectEntry> {
    let mut by_path: IndexMap<String, ProjectEntry> = IndexMap::with_capacity(entries.len());
    for entry in &entries {
        by_path.insert(entry.path.clone(), entry.clone());
    }
    for entry in &entries {
        let mut parent = parent_path_of(&entry.path);
        while let Some(path) = parent.filter(|path| !path.is_empty()) {
            if !by_path.contains_key(path) {
                by_path.insert(
                    path.to_owned(),
                    ProjectEntry {
                        path: path.to_owned(),
                        kind: ProjectEntryKind::Directory,
                        ignored: None,
                    },
                );
            }
            parent = parent_path_of(path);
        }
    }
    by_path.into_values().collect()
}

/// `buildContentSearchQuery`: plain case-insensitive search relies on smart case (an
/// all-lowercase needle), regex needs an inline `(?i)`.
fn build_content_search_query(input: &ContentSearch) -> (String, bool) {
    if input.case_sensitive {
        return (input.query.clone(), input.use_regex);
    }
    if input.use_regex {
        (format!("(?i){}", input.query), true)
    } else {
        (input.query.to_lowercase(), false)
    }
}

impl WorkspaceSearchIndex {
    /// `make(cwd, variant)`: creates the fff instance and waits (15 s at most) until its first
    /// scan, and for the content variant its content index, is complete.
    pub async fn make(factory: Arc<dyn FinderFactory>, cwd: &str, variant: IndexVariant) -> Result<Self, SearchIndexError> {
        let cwd = cwd.to_owned();
        let created = {
            let cwd = cwd.clone();
            tokio::task::spawn_blocking(move || factory.create(&cwd, variant))
                .await
                .unwrap_or_else(|join| Err(FinderError::Threw(zc_core::Defect::error("Error", join.to_string()))))
        };
        let finder = created.map_err(|error| match error {
            FinderError::Threw(cause) => SearchIndexError::CreateFailed {
                cwd: cwd.clone(),
                reason: "FileFinder.create threw unexpectedly.".into(),
                cause: Some(cause),
            },
            FinderError::Returned(reason) => SearchIndexError::CreateFailed {
                cwd: cwd.clone(),
                reason,
                cause: None,
            },
        })?;
        let index = Self { cwd, variant, finder };
        let cwd = index.cwd.clone();
        index
            .wait_for_index_ready(|reason, cause| SearchIndexError::CreateFailed {
                cwd: cwd.clone(),
                reason,
                cause,
            })
            .await?;
        Ok(index)
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    pub fn variant(&self) -> IndexVariant {
        self.variant
    }

    /// `finder.waitForIndexReady(15_000)`: polls the scan progress every 50 ms until the scan is
    /// over and the warmup (content index) is complete.
    async fn wait_for_index_ready(&self, on_failure: impl Fn(String, Option<zc_core::Defect>) -> SearchIndexError) -> Result<(), SearchIndexError> {
        let deadline = tokio::time::Instant::now() + WORKSPACE_INDEX_SCAN_TIMEOUT;
        loop {
            let progress = self.finder.scan_progress().map_err(|error| match error {
                FinderError::Returned(reason) => on_failure(reason, None),
                FinderError::Threw(cause) => on_failure("FileFinder.waitForIndexReady rejected unexpectedly.".into(), Some(cause)),
            })?;
            if !progress.is_scanning && progress.is_warmup_complete {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(SearchIndexError::ScanTimedOut {
                    cwd: self.cwd.clone(),
                    timeout: WORKSPACE_INDEX_SCAN_TIMEOUT_LABEL.into(),
                });
            }
            tokio::time::sleep(INDEX_READY_POLL_INTERVAL).await;
        }
    }

    /// `runSearch`: one finder call on the blocking pool, its failures as `SearchFailed`.
    async fn run_search<T: Send + 'static>(
        &self,
        query_length: usize,
        page_size: usize,
        operation: &'static str,
        call: impl FnOnce(&dyn Finder) -> Result<T, FinderError> + Send + 'static,
    ) -> Result<T, SearchIndexError> {
        let finder = self.finder.clone();
        let result = tokio::task::spawn_blocking(move || call(finder.as_ref()))
            .await
            .unwrap_or_else(|join| Err(FinderError::Threw(zc_core::Defect::error("Error", join.to_string()))));
        result.map_err(|error| match error {
            FinderError::Threw(cause) => SearchIndexError::SearchFailed {
                cwd: self.cwd.clone(),
                query_length,
                page_size,
                reason: format!("FileFinder.{operation} threw unexpectedly."),
                cause: Some(cause),
            },
            FinderError::Returned(reason) => SearchIndexError::SearchFailed {
                cwd: self.cwd.clone(),
                query_length,
                page_size,
                reason,
                cause: None,
            },
        })
    }

    /// `refresh()`: `scanFiles()` then `waitForIndexReady`.
    pub async fn refresh(&self) -> Result<(), SearchIndexError> {
        let finder = self.finder.clone();
        let scanned = tokio::task::spawn_blocking(move || finder.scan_files())
            .await
            .unwrap_or_else(|join| Err(FinderError::Threw(zc_core::Defect::error("Error", join.to_string()))));
        let cwd = self.cwd.clone();
        scanned.map_err(|error| match error {
            FinderError::Threw(cause) => SearchIndexError::RefreshFailed {
                cwd: cwd.clone(),
                reason: "FileFinder.scanFiles threw unexpectedly.".into(),
                cause: Some(cause),
            },
            FinderError::Returned(reason) => SearchIndexError::RefreshFailed {
                cwd: cwd.clone(),
                reason,
                cause: None,
            },
        })?;
        self.wait_for_index_ready(|reason, cause| SearchIndexError::RefreshFailed {
            cwd: cwd.clone(),
            reason,
            cause,
        })
        .await
    }

    /// `list()`: every indexed entry plus ancestors, sorted by `localeCompare`.
    pub async fn list(&self) -> Result<ProjectListEntriesResult, SearchIndexError> {
        let result = self
            .run_search(0, WORKSPACE_INDEX_PAGE_SIZE, "mixedSearch", |finder| {
                finder.mixed_search("", WORKSPACE_INDEX_PAGE_SIZE)
            })
            .await?;
        let mapped = map_mixed_search_result(result, WORKSPACE_INDEX_MAX_ENTRIES);
        let mut sorted = with_directory_ancestors(mapped.entries);
        sorted.sort_by(|left, right| locale_compare(&left.path, &right.path));
        let total = sorted.len();
        sorted.truncate(WORKSPACE_INDEX_MAX_ENTRIES);
        Ok(ProjectListEntriesResult {
            truncated: mapped.truncated || sorted.len() < total,
            entries: sorted,
        })
    }

    /// `search(query, limit, kind, imageOnly)`.
    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        kind: Option<ProjectEntryKind>,
        image_only: bool,
    ) -> Result<ProjectSearchEntriesResult, SearchIndexError> {
        let page_size = if image_only { WORKSPACE_INDEX_PAGE_SIZE } else { (limit + 1).max(1) };
        let query_length = js_length(query);
        let owned = query.to_owned();
        if kind == Some(ProjectEntryKind::File) || image_only {
            let result = self
                .run_search(query_length, page_size, "fileSearch", move |finder| finder.file_search(&owned, page_size))
                .await?;
            return Ok(map_file_search_result(result, limit, image_only));
        }
        if kind == Some(ProjectEntryKind::Directory) {
            let result = self
                .run_search(query_length, page_size, "directorySearch", move |finder| {
                    finder.directory_search(&owned, page_size)
                })
                .await?;
            return Ok(map_directory_search_result(result, limit));
        }
        let result = self
            .run_search(query_length, page_size, "mixedSearch", move |finder| finder.mixed_search(&owned, page_size))
            .await?;
        Ok(map_mixed_search_result(result, limit))
    }

    /// `searchContents(input)`.
    pub async fn search_contents(&self, input: &ContentSearch) -> Result<ProjectSearchContentsResult, SearchIndexError> {
        let (search_query, regex_mode) = build_content_search_query(input);
        let deadline = Instant::now() + Duration::from_millis(CONTENT_SEARCH_TIME_BUDGET_MS);
        // Grep cursors advance by file, so whole-word post-filtering needs enough raw candidates
        // from the current file before moving to the next one.
        let raw_page_size = if input.whole_word {
            input.limit.max(CONTENT_SEARCH_MAX_MATCHES_PER_FILE)
        } else {
            input.limit
        };
        let query_length = js_length(&input.query);
        let mut matches: Vec<ProjectContentMatch> = Vec::new();
        let mut next_cursor: Option<usize> = None;
        let mut regex_fallback_error: Option<String> = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let remaining_ms = (remaining.as_secs_f64() * 1000.0).ceil().max(1.0) as u64;
            let request = GrepRequest {
                query: search_query.clone(),
                mode: if regex_mode { GrepMode::Regex } else { GrepMode::Plain },
                smart_case: !input.case_sensitive && !regex_mode,
                // A single dense file must not consume the whole result page.
                max_matches_per_file: CONTENT_SEARCH_MAX_MATCHES_PER_FILE.min(raw_page_size),
                page_size: raw_page_size,
                cursor: next_cursor,
                time_budget_ms: remaining_ms,
            };
            let page = self.run_search(query_length, input.limit, "grep", move |finder| finder.grep(&request)).await?;
            for hit in page.items {
                let line = Utf16Line::new(&hit.line_content);
                let ranges: Vec<ProjectContentMatchRange> = hit
                    .match_ranges
                    .iter()
                    .map(|&(start, end)| ProjectContentMatchRange {
                        start: byte_offset_to_utf16_index(&hit.line_content, start as usize) as i64,
                        end: byte_offset_to_utf16_index(&hit.line_content, end as usize) as i64,
                    })
                    .filter(|range| !input.whole_word || is_whole_word_range(&line, range.start as usize, range.end as usize))
                    .collect();
                if ranges.is_empty() {
                    continue;
                }
                matches.push(ProjectContentMatch {
                    path: to_posix_path(&hit.relative_path),
                    line_number: hit.line_number as i64,
                    line_content: hit.line_content,
                    match_ranges: ranges,
                });
            }
            next_cursor = page.next_cursor;
            if regex_fallback_error.is_none() {
                regex_fallback_error = page.regex_fallback_error;
            }
            if !(matches.len() < input.limit && next_cursor.is_some() && Instant::now() < deadline) {
                break;
            }
        }
        let truncated = matches.len() > input.limit || next_cursor.is_some();
        matches.truncate(input.limit);
        Ok(ProjectSearchContentsResult {
            matches,
            truncated,
            regex_fallback_error,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::backend::{GrepHit, GrepPage, ScanProgress};
    use crate::errors::TaggedError;
    use std::sync::Mutex;
    use zc_core::Defect;

    type Handler<T> = Box<dyn Fn(&str, usize) -> Result<T, FinderError> + Send + Sync>;
    type GrepHandler = Box<dyn Fn(&GrepRequest) -> Result<GrepPage, FinderError> + Send + Sync>;

    /// A scripted finder (the `vi.fn` mocks of `WorkspaceSearchIndex.test.ts`).
    #[derive(Default)]
    pub(crate) struct FakeFinder {
        pub ready: Option<Box<dyn Fn() -> Result<ScanProgress, FinderError> + Send + Sync>>,
        pub file_search: Option<Handler<PathSearchPage>>,
        pub mixed_search: Option<Handler<MixedSearchPage>>,
        pub grep: Option<GrepHandler>,
        pub scan_files: Option<Box<dyn Fn() -> Result<(), FinderError> + Send + Sync>>,
        pub calls: Mutex<Vec<String>>,
        pub grep_requests: Mutex<Vec<GrepRequest>>,
    }

    impl Finder for FakeFinder {
        fn scan_progress(&self) -> Result<ScanProgress, FinderError> {
            match &self.ready {
                Some(ready) => ready(),
                None => Ok(ScanProgress {
                    is_scanning: false,
                    is_warmup_complete: true,
                }),
            }
        }
        fn file_search(&self, query: &str, page_size: usize) -> Result<PathSearchPage, FinderError> {
            self.calls.lock().unwrap().push(format!("fileSearch({query:?}, {page_size})"));
            (self.file_search.as_ref().expect("fileSearch not mocked"))(query, page_size)
        }
        fn directory_search(&self, _query: &str, _page_size: usize) -> Result<PathSearchPage, FinderError> {
            unimplemented!("directorySearch not mocked")
        }
        fn mixed_search(&self, query: &str, page_size: usize) -> Result<MixedSearchPage, FinderError> {
            self.calls.lock().unwrap().push(format!("mixedSearch({query:?}, {page_size})"));
            (self.mixed_search.as_ref().expect("mixedSearch not mocked"))(query, page_size)
        }
        fn grep(&self, request: &GrepRequest) -> Result<GrepPage, FinderError> {
            self.grep_requests.lock().unwrap().push(request.clone());
            (self.grep.as_ref().expect("grep not mocked"))(request)
        }
        fn scan_files(&self) -> Result<(), FinderError> {
            (self.scan_files.as_ref().expect("scanFiles not mocked"))()
        }
    }

    pub(crate) struct FakeFactory(pub Mutex<Option<Result<Arc<dyn Finder>, FinderError>>>);

    impl FakeFactory {
        pub fn new(result: Result<Arc<dyn Finder>, FinderError>) -> Arc<Self> {
            Arc::new(Self(Mutex::new(Some(result))))
        }
    }

    impl FinderFactory for FakeFactory {
        fn create(&self, _cwd: &str, _variant: IndexVariant) -> Result<Arc<dyn Finder>, FinderError> {
            self.0.lock().unwrap().take().expect("create called once")
        }
    }

    async fn make_with(finder: FakeFinder, variant: IndexVariant) -> Result<WorkspaceSearchIndex, SearchIndexError> {
        WorkspaceSearchIndex::make(FakeFactory::new(Ok(Arc::new(finder))), "/workspace/project", variant).await
    }

    #[tokio::test]
    async fn filters_image_searches_before_applying_the_result_limit() {
        let mut items: Vec<String> = (0..200).map(|index| format!("src/file-{index}.ts")).collect();
        items.push("public/icon.svg".into());
        let finder = FakeFinder {
            file_search: Some(Box::new(move |_, _| {
                Ok(PathSearchPage {
                    items: items.clone(),
                    total_matched: items.len(),
                })
            })),
            ..Default::default()
        };
        let finder = Arc::new(finder);
        let index = WorkspaceSearchIndex::make(
            FakeFactory::new(Ok(finder.clone() as Arc<dyn Finder>)),
            "/workspace/project",
            IndexVariant::Paths,
        )
        .await
        .unwrap();
        let without_kind = index.search("", 200, None, true).await.unwrap();
        let with_directory_kind = index.search("", 200, Some(ProjectEntryKind::Directory), true).await.unwrap();
        let expected = vec![ProjectEntry {
            path: "public/icon.svg".into(),
            kind: ProjectEntryKind::File,
            ignored: None,
        }];
        assert_eq!(without_kind.entries, expected);
        assert_eq!(with_directory_kind.entries, expected);
        assert_eq!(
            *finder.calls.lock().unwrap(),
            vec!["fileSearch(\"\", 25002)".to_owned(), "fileSearch(\"\", 25002)".to_owned()]
        );
    }

    #[tokio::test]
    async fn preserves_unexpected_creation_failures() {
        let cause = Defect::error("Error", "native initialization failed");
        let error = WorkspaceSearchIndex::make(
            FakeFactory::new(Err(FinderError::Threw(cause.clone()))),
            "/workspace/project",
            IndexVariant::Paths,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            SearchIndexError::CreateFailed {
                cwd: "/workspace/project".into(),
                reason: "FileFinder.create threw unexpectedly.".into(),
                cause: Some(cause),
            }
        );
    }

    #[tokio::test]
    async fn keeps_returned_creation_diagnostics_out_of_the_cause_chain() {
        let error = WorkspaceSearchIndex::make(
            FakeFactory::new(Err(FinderError::Returned("native index rejected the directory".into()))),
            "/workspace/project",
            IndexVariant::Paths,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            SearchIndexError::CreateFailed {
                cwd: "/workspace/project".into(),
                reason: "native index rejected the directory".into(),
                cause: None,
            }
        );
        assert!(error.cause().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn waits_for_the_full_content_index_warmup_before_returning() {
        let polls = Arc::new(Mutex::new(0usize));
        let counter = polls.clone();
        let finder = FakeFinder {
            ready: Some(Box::new(move || {
                let mut polls = counter.lock().unwrap();
                *polls += 1;
                Ok(ScanProgress {
                    is_scanning: false,
                    is_warmup_complete: *polls >= 4,
                })
            })),
            ..Default::default()
        };
        let started = tokio::time::Instant::now();
        make_with(finder, IndexVariant::Content).await.unwrap();
        assert_eq!(*polls.lock().unwrap(), 4);
        assert_eq!(started.elapsed(), INDEX_READY_POLL_INTERVAL * 3);
    }

    #[tokio::test(start_paused = true)]
    async fn preserves_a_full_index_warmup_timeout_as_a_structured_error() {
        let finder = FakeFinder {
            ready: Some(Box::new(|| {
                Ok(ScanProgress {
                    is_scanning: true,
                    is_warmup_complete: false,
                })
            })),
            ..Default::default()
        };
        let error = make_with(finder, IndexVariant::Content).await.unwrap_err();
        assert_eq!(
            error,
            SearchIndexError::ScanTimedOut {
                cwd: "/workspace/project".into(),
                timeout: "15 seconds".into()
            }
        );
    }

    #[tokio::test]
    async fn preserves_search_and_refresh_failures_with_operation_context() {
        let finder = FakeFinder {
            mixed_search: Some(Box::new(|_, _| Err(FinderError::Threw(Defect::error("Error", "native search failed"))))),
            grep: Some(Box::new(|_| Err(FinderError::Threw(Defect::error("Error", "native grep failed"))))),
            scan_files: Some(Box::new(|| Err(FinderError::Threw(Defect::error("Error", "native scan failed"))))),
            ..Default::default()
        };
        let index = make_with(finder, IndexVariant::Paths).await.unwrap();
        let query = "authorization: Bearer secret-token";
        let search_error = index.search(query, 3, None, false).await.unwrap_err();
        let content_error = index
            .search_contents(&ContentSearch {
                query: query.into(),
                limit: 3,
                case_sensitive: false,
                whole_word: false,
                use_regex: false,
            })
            .await
            .unwrap_err();
        let refresh_error = index.refresh().await.unwrap_err();
        assert_eq!(
            search_error,
            SearchIndexError::SearchFailed {
                cwd: "/workspace/project".into(),
                query_length: query.len(),
                page_size: 4,
                reason: "FileFinder.mixedSearch threw unexpectedly.".into(),
                cause: Some(Defect::error("Error", "native search failed")),
            }
        );
        assert!(!search_error.message().contains("Bearer") && !search_error.message().contains("secret-token"));
        assert_eq!(
            content_error,
            SearchIndexError::SearchFailed {
                cwd: "/workspace/project".into(),
                query_length: query.len(),
                page_size: 3,
                reason: "FileFinder.grep threw unexpectedly.".into(),
                cause: Some(Defect::error("Error", "native grep failed")),
            }
        );
        assert!(!format!("{:?}", content_error.to_defect()).contains("secret-token"));
        assert_eq!(
            refresh_error,
            SearchIndexError::RefreshFailed {
                cwd: "/workspace/project".into(),
                reason: "FileFinder.scanFiles threw unexpectedly.".into(),
                cause: Some(Defect::error("Error", "native scan failed")),
            }
        );
    }

    #[tokio::test]
    async fn keeps_returned_search_diagnostics_out_of_the_cause_chain() {
        let finder = FakeFinder {
            mixed_search: Some(Box::new(|_, _| Err(FinderError::Returned("native query rejected".into())))),
            scan_files: Some(Box::new(|| Err(FinderError::Returned("native refresh rejected".into())))),
            ..Default::default()
        };
        let index = make_with(finder, IndexVariant::Paths).await.unwrap();
        let query = "authorization: Bearer secret-token";
        let search_error = index.search(query, 3, None, false).await.unwrap_err();
        let refresh_error = index.refresh().await.unwrap_err();
        assert_eq!(
            search_error,
            SearchIndexError::SearchFailed {
                cwd: "/workspace/project".into(),
                query_length: query.len(),
                page_size: 4,
                reason: "native query rejected".into(),
                cause: None,
            }
        );
        assert_eq!(
            refresh_error,
            SearchIndexError::RefreshFailed {
                cwd: "/workspace/project".into(),
                reason: "native refresh rejected".into(),
                cause: None,
            }
        );
    }

    #[tokio::test]
    async fn continues_whole_word_searches_after_a_filtered_grep_page() {
        let finder = FakeFinder {
            grep: Some(Box::new(|request| {
                let (line, cursor) = if request.cursor.is_some() {
                    ("needle", None)
                } else {
                    ("needleSuffix", Some(1))
                };
                Ok(GrepPage {
                    items: vec![GrepHit {
                        relative_path: "src/words.ts".into(),
                        line_number: 1,
                        line_content: line.into(),
                        match_ranges: vec![(0, 6)],
                    }],
                    next_cursor: cursor,
                    regex_fallback_error: None,
                })
            })),
            ..Default::default()
        };
        let finder = Arc::new(finder);
        let index = WorkspaceSearchIndex::make(
            FakeFactory::new(Ok(finder.clone() as Arc<dyn Finder>)),
            "/workspace/project",
            IndexVariant::Content,
        )
        .await
        .unwrap();
        let result = index
            .search_contents(&ContentSearch {
                query: "needle".into(),
                limit: 1,
                case_sensitive: true,
                whole_word: true,
                use_regex: false,
            })
            .await
            .unwrap();
        assert_eq!(
            result,
            ProjectSearchContentsResult {
                matches: vec![ProjectContentMatch {
                    path: "src/words.ts".into(),
                    line_number: 1,
                    line_content: "needle".into(),
                    match_ranges: vec![ProjectContentMatchRange { start: 0, end: 6 }],
                }],
                truncated: false,
                regex_fallback_error: None,
            }
        );
        let requests = finder.grep_requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].cursor, Some(1));
        // Whole word asks for at least 100 raw candidates, capped at 100 per file.
        assert_eq!(requests[0].page_size, 100);
        assert_eq!(requests[0].max_matches_per_file, 100);
        assert!(!requests[0].smart_case);
    }

    #[test]
    fn content_queries_follow_smart_case_and_inline_flags() {
        let base = ContentSearch {
            query: "\\SQUARE".into(),
            limit: 1,
            case_sensitive: false,
            whole_word: false,
            use_regex: true,
        };
        assert_eq!(build_content_search_query(&base), ("(?i)\\SQUARE".into(), true));
        let plain = ContentSearch {
            use_regex: false,
            query: "Square".into(),
            ..base.clone()
        };
        assert_eq!(build_content_search_query(&plain), ("square".into(), false));
        let sensitive = ContentSearch { case_sensitive: true, ..plain };
        assert_eq!(build_content_search_query(&sensitive), ("Square".into(), false));
    }

    #[test]
    fn mapping_rules() {
        let directories = PathSearchPage {
            items: vec!["".into(), "src/".into(), "docs/".into()],
            total_matched: 3,
        };
        let mapped = map_directory_search_result(directories, 2);
        assert_eq!(mapped.entries.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["src", "docs"]);
        assert!(!mapped.truncated, "the root directory does not count towards truncation");
        let ancestors = with_directory_ancestors(vec![ProjectEntry {
            path: "a/b/c.ts".into(),
            kind: ProjectEntryKind::File,
            ignored: None,
        }]);
        assert_eq!(ancestors.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["a/b/c.ts", "a/b", "a"]);
        assert!(is_workspace_image_preview_path("public/Icon.SVG?raw"));
        assert!(!is_workspace_image_preview_path("public/icon.svg.ts"));
    }
}
