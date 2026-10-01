//! Port of `workspace/WorkspaceEntries.test.ts`, on real fff indexes and real temp trees.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use zc_contracts::{FilesystemBrowseEntry, FilesystemBrowseResult, ProjectEntry, ProjectEntryKind};
use zc_core::VcsProcess;
use zc_workspace::backend::{GrepPage, GrepRequest, MixedSearchPage, PathSearchPage, ScanProgress};
use zc_workspace::errors::{BrowseError, TaggedError};
use zc_workspace::{
    BrowseRequest, ContentSearch, EntrySearch, FffFactory, Finder, FinderError, FinderFactory, IndexVariant, SearchIndexMap, WorkspaceEntries, WorkspacePaths,
};

fn service_with(factory: Arc<dyn FinderFactory>) -> WorkspaceEntries {
    WorkspaceEntries::new(WorkspacePaths::new(), SearchIndexMap::new(factory), Arc::new(VcsProcess::default()))
}

fn service() -> WorkspaceEntries {
    service_with(Arc::new(FffFactory))
}

fn temp_dir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new().prefix(prefix).tempdir().unwrap()
}

fn path_of(dir: &tempfile::TempDir) -> String {
    dir.path().to_string_lossy().into_owned()
}

fn git(cwd: &str, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(status.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&status.stderr));
}

fn git_dir(prefix: &str) -> tempfile::TempDir {
    let dir = temp_dir(prefix);
    git(&path_of(&dir), &["init", "-q"]);
    dir
}

fn write(cwd: &str, relative: &str, contents: &str) {
    let path = Path::new(cwd).join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn entry(path: &str, kind: ProjectEntryKind) -> ProjectEntry {
    ProjectEntry {
        path: path.into(),
        kind,
        ignored: None,
    }
}

fn ignored(path: &str, kind: ProjectEntryKind) -> ProjectEntry {
    ProjectEntry {
        path: path.into(),
        kind,
        ignored: Some(true),
    }
}

fn search(query: &str, limit: usize, kind: Option<ProjectEntryKind>) -> EntrySearch {
    EntrySearch {
        query: query.into(),
        limit,
        kind,
        image_only: false,
    }
}

fn contents(query: &str, limit: usize, case_sensitive: bool, whole_word: bool, use_regex: bool) -> ContentSearch {
    ContentSearch {
        query: query.into(),
        limit,
        case_sensitive,
        whole_word,
        use_regex,
    }
}

fn paths(entries: &[ProjectEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.path.clone()).collect()
}

// ------------------------------------------------------------------------------------------
// list
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn lists_immediate_children_including_ignored_and_empty_directories() {
    let dir = git_dir("zc-ws-entries-");
    let cwd = path_of(&dir);
    write(&cwd, "tracked.txt", "");
    git(&cwd, &["add", "tracked.txt"]);
    write(&cwd, ".gitignore", "node_modules/\n.env\ntracked.txt\n");
    write(&cwd, ".env", "secret=value");
    write(&cwd, "node_modules/pkg/index.js", "");
    write(&cwd, "src/index.ts", "");
    std::fs::create_dir(Path::new(&cwd).join("empty")).unwrap();

    let entries = service();
    let root = entries.list(&cwd, Some("")).await.unwrap();
    for expected in [
        ignored(".env", ProjectEntryKind::File),
        ignored("node_modules", ProjectEntryKind::Directory),
        entry("src", ProjectEntryKind::Directory),
        entry("empty", ProjectEntryKind::Directory),
        // Tracked files are never reported as ignored.
        entry("tracked.txt", ProjectEntryKind::File),
    ] {
        assert!(root.entries.contains(&expected), "missing {expected:?} in {:?}", root.entries);
    }
    assert!(!root.entries.iter().any(|entry| entry.path.contains('/')));
    assert!(!root.entries.iter().any(|entry| entry.path == ".git"));
    assert!(!root.truncated);
    let nested = entries.list(&cwd, Some("node_modules/pkg")).await.unwrap();
    assert_eq!(nested.entries, vec![ignored("node_modules/pkg/index.js", ProjectEntryKind::File)]);
    assert!(!nested.truncated);
    let empty = entries.list(&cwd, Some("empty")).await.unwrap();
    assert!(empty.entries.is_empty() && !empty.truncated);
}

#[tokio::test]
async fn rejects_directory_traversal_git_internals_and_symlinks_outside_the_workspace() {
    let dir = temp_dir("zc-ws-entries-");
    let outside = temp_dir("zc-ws-outside-");
    let cwd = path_of(&dir);
    write(&cwd, ".git/HEAD", "");
    std::os::unix::fs::symlink(outside.path(), Path::new(&cwd).join("external")).unwrap();
    let outside_path = path_of(&outside);
    let entries = service();
    for directory_path in ["../", outside_path.as_str(), ".git", "missing", "external", "a/../.."] {
        let error = entries.list(&cwd, Some(directory_path)).await.unwrap_err();
        assert_eq!(error.tag(), "WorkspaceEntriesReadDirectoryError", "{directory_path}");
    }
}

#[tokio::test]
async fn lists_symlinked_directories_that_stay_inside_and_skips_symlink_children() {
    let dir = temp_dir("zc-ws-entries-");
    let cwd = path_of(&dir);
    write(&cwd, "real/file.txt", "");
    std::os::unix::fs::symlink(Path::new(&cwd).join("real"), Path::new(&cwd).join("alias")).unwrap();
    std::os::unix::fs::symlink(Path::new(&cwd).join("real/file.txt"), Path::new(&cwd).join("real/link.txt")).unwrap();
    let entries = service();
    // A symlink to a directory inside the root may be listed through.
    let listed = entries.list(&cwd, Some("alias")).await.unwrap();
    assert_eq!(listed.entries, vec![entry("alias/file.txt", ProjectEntryKind::File)]);
    // Symlinks themselves are neither files nor directories in the listing.
    let root = entries.list(&cwd, Some("")).await.unwrap();
    assert_eq!(paths(&root.entries).iter().filter(|p| *p == "alias").count(), 0);
}

#[tokio::test]
async fn browses_a_workspace_with_more_than_25000_entries_without_truncation() {
    let dir = temp_dir("zc-ws-entries-large-");
    let cwd = path_of(&dir);
    for directory in 0..26 {
        let directory_path = Path::new(&cwd).join(format!("folder-{directory}"));
        std::fs::create_dir(&directory_path).unwrap();
        for index in 0..1000 {
            std::fs::write(directory_path.join(format!("file-{index}.txt")), "").unwrap();
        }
    }
    let entries = service();
    let root = entries.list(&cwd, Some("")).await.unwrap();
    assert_eq!(root.entries.len(), 26);
    assert!(!root.truncated);
    for directory in &root.entries {
        let result = entries.list(&cwd, Some(&directory.path)).await.unwrap();
        assert_eq!(result.entries.len(), 1000);
        assert!(!result.truncated);
        assert!(result
            .entries
            .contains(&entry(&format!("{}/file-999.txt", directory.path), ProjectEntryKind::File)));
    }
}

#[tokio::test]
async fn returns_the_complete_cached_workspace_index() {
    let dir = temp_dir("zc-ws-entries-");
    let cwd = path_of(&dir);
    write(&cwd, "src/components/Composer.tsx", "");
    write(&cwd, "README.md", "");
    write(&cwd, "node_modules/pkg/index.js", "");
    let result = service().list(&cwd, None).await.unwrap();
    for expected in [
        entry("src", ProjectEntryKind::Directory),
        entry("src/components", ProjectEntryKind::Directory),
        entry("src/components/Composer.tsx", ProjectEntryKind::File),
        entry("README.md", ProjectEntryKind::File),
    ] {
        assert!(result.entries.contains(&expected), "missing {expected:?}");
    }
    assert!(!result.entries.iter().any(|entry| entry.path.starts_with("node_modules")));
    assert!(!result.truncated);
    // localeCompare order.
    assert_eq!(paths(&result.entries), ["README.md", "src", "src/components", "src/components/Composer.tsx"]);
}

#[tokio::test]
async fn reports_missing_and_non_directory_roots() {
    let dir = temp_dir("zc-ws-entries-");
    let cwd = path_of(&dir);
    write(&cwd, "file.txt", "");
    let entries = service();
    let missing = entries.list(&format!("{cwd}/missing"), None).await.unwrap_err();
    assert_eq!(missing.tag(), "WorkspaceRootNotExistsError");
    let file = entries.search(&format!("{cwd}/file.txt"), &search("", 10, None)).await.unwrap_err();
    assert_eq!(file.tag(), "WorkspaceRootNotDirectoryError");
}

// ------------------------------------------------------------------------------------------
// search
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn returns_files_and_directories_relative_to_cwd() {
    let dir = temp_dir("zc-ws-entries-");
    let cwd = path_of(&dir);
    write(&cwd, "src/components/Composer.tsx", "");
    write(&cwd, "src/index.ts", "");
    write(&cwd, "README.md", "");
    write(&cwd, ".git/HEAD", "");
    write(&cwd, "node_modules/pkg/index.js", "");
    let result = service().search(&cwd, &search("", 100, None)).await.unwrap();
    let found = paths(&result.entries);
    for expected in ["src", "src/components", "src/components/Composer.tsx", "README.md"] {
        assert!(found.contains(&expected.to_owned()), "missing {expected} in {found:?}");
    }
    assert!(!found.iter().any(|path| path.starts_with(".git")));
    assert!(!found.iter().any(|path| path.starts_with("node_modules")));
    assert!(!result.truncated);
}

#[tokio::test]
async fn filters_and_ranks_entries_by_query() {
    let dir = temp_dir("zc-ws-query-");
    let cwd = path_of(&dir);
    write(&cwd, "src/components/Composer.tsx", "");
    write(&cwd, "src/components/composePrompt.ts", "");
    write(&cwd, "docs/composition.md", "");
    let result = service().search(&cwd, &search("compo", 5, None)).await.unwrap();
    assert!(!result.entries.is_empty());
    assert!(result.entries.iter().any(|entry| entry.path == "src/components"));
    assert!(result.entries.iter().all(|entry| entry.path.to_lowercase().contains("compo")));
}

#[tokio::test]
async fn supports_fuzzy_subsequence_queries_for_composer_path_search() {
    let dir = temp_dir("zc-ws-fuzzy-");
    let cwd = path_of(&dir);
    write(&cwd, "src/components/Composer.tsx", "");
    write(&cwd, "src/components/composePrompt.ts", "");
    write(&cwd, "docs/composition.md", "");
    let result = service().search(&cwd, &search("cmp", 10, None)).await.unwrap();
    let found = paths(&result.entries);
    assert!(found.contains(&"src/components".to_owned()));
    assert!(found.contains(&"src/components/Composer.tsx".to_owned()));
}

#[tokio::test]
async fn prioritizes_exact_basename_matches_ahead_of_broader_path_matches() {
    let dir = temp_dir("zc-ws-exact-");
    let cwd = path_of(&dir);
    write(&cwd, "src/components/Composer.tsx", "");
    write(&cwd, "docs/composer.tsx-notes.md", "");
    let result = service().search(&cwd, &search("Composer.tsx", 5, None)).await.unwrap();
    assert_eq!(result.entries[0].path, "src/components/Composer.tsx");
}

#[tokio::test]
async fn tracks_truncation_without_sorting_every_fuzzy_match() {
    let dir = temp_dir("zc-ws-fuzzy-limit-");
    let cwd = path_of(&dir);
    write(&cwd, "src/components/Composer.tsx", "");
    write(&cwd, "src/components/composePrompt.ts", "");
    write(&cwd, "docs/composition.md", "");
    let result = service().search(&cwd, &search("cmp", 1, None)).await.unwrap();
    assert_eq!(result.entries.len(), 1);
    assert!(result.truncated);
}

#[tokio::test]
async fn applies_the_file_filter_before_limiting_search_results() {
    let dir = temp_dir("zc-ws-file-limit-");
    let cwd = path_of(&dir);
    write(&cwd, "src/index.ts", "");
    write(&cwd, "src/internal.ts", "");
    let result = service().search(&cwd, &search("src", 1, Some(ProjectEntryKind::File))).await.unwrap();
    assert_eq!(result.entries.len(), 1);
    assert_eq!(result.entries[0].kind, ProjectEntryKind::File);
    assert!(["src/index.ts", "src/internal.ts"].contains(&result.entries[0].path.as_str()));
    assert!(result.truncated);
}

#[tokio::test]
async fn answers_an_empty_file_filtered_query_with_a_bounded_file_listing() {
    let dir = temp_dir("zc-ws-empty-query-");
    let cwd = path_of(&dir);
    write(&cwd, "src/index.ts", "");
    write(&cwd, "README.md", "");
    let result = service().search(&cwd, &search("", 10, Some(ProjectEntryKind::File))).await.unwrap();
    let mut found = paths(&result.entries);
    found.sort();
    assert_eq!(found, ["README.md", "src/index.ts"]);
    assert!(result.entries.iter().all(|entry| entry.kind == ProjectEntryKind::File));
}

#[tokio::test]
async fn returns_only_directories_for_the_directory_filter() {
    let dir = temp_dir("zc-ws-directory-filter-");
    let cwd = path_of(&dir);
    write(&cwd, "src/index.ts", "");
    let result = service().search(&cwd, &search("src", 10, Some(ProjectEntryKind::Directory))).await.unwrap();
    assert_eq!(result.entries, vec![entry("src", ProjectEntryKind::Directory)]);
    assert!(!result.truncated);
}

#[tokio::test]
async fn filters_image_only_searches() {
    let dir = temp_dir("zc-ws-images-");
    let cwd = path_of(&dir);
    write(&cwd, "public/logo.svg", "<svg/>");
    write(&cwd, "public/logo.ts", "");
    write(&cwd, "assets/photo.PNG", "");
    let result = service()
        .search(
            &cwd,
            &EntrySearch {
                query: "".into(),
                limit: 10,
                kind: None,
                image_only: true,
            },
        )
        .await
        .unwrap();
    let mut found = paths(&result.entries);
    found.sort();
    assert_eq!(found, ["assets/photo.PNG", "public/logo.svg"]);
}

#[tokio::test]
async fn strips_leading_mention_and_path_characters_from_queries() {
    let dir = temp_dir("zc-ws-mention-");
    let cwd = path_of(&dir);
    write(&cwd, "src/components/Composer.tsx", "");
    let result = service()
        .search(&cwd, &search("  @./Composer.tsx ", 5, Some(ProjectEntryKind::File)))
        .await
        .unwrap();
    assert_eq!(result.entries[0].path, "src/components/Composer.tsx");
}

#[tokio::test]
async fn excludes_gitignored_paths_for_git_repositories() {
    let dir = git_dir("zc-ws-gitignore-");
    let cwd = path_of(&dir);
    write(&cwd, ".gitignore", ".convex/\nconvex/\nignored.txt\n");
    write(&cwd, "src/keep.ts", "export {};");
    write(&cwd, "ignored.txt", "ignore me");
    write(&cwd, ".convex/local-storage/data.json", "{}");
    write(&cwd, "convex/UOoS-l/convex_local_storage/modules/data.json", "{}");
    let result = service().search(&cwd, &search("", 100, None)).await.unwrap();
    let found = paths(&result.entries);
    assert!(found.contains(&"src".to_owned()));
    assert!(found.contains(&"src/keep.ts".to_owned()));
    assert!(!found.contains(&"ignored.txt".to_owned()));
    assert!(!found.iter().any(|path| path.starts_with(".convex/")));
    assert!(!found.iter().any(|path| path.starts_with("convex/")));
}

#[tokio::test]
async fn excludes_tracked_paths_that_match_ignore_rules() {
    let dir = git_dir("zc-ws-tracked-gitignore-");
    let cwd = path_of(&dir);
    write(&cwd, ".convex/local-storage/data.json", "{}");
    write(&cwd, "src/keep.ts", "export {};");
    git(&cwd, &["add", ".convex/local-storage/data.json", "src/keep.ts"]);
    write(&cwd, ".gitignore", ".convex/\n");
    let result = service().search(&cwd, &search("", 100, None)).await.unwrap();
    let found = paths(&result.entries);
    assert!(found.contains(&"src".to_owned()));
    assert!(found.contains(&"src/keep.ts".to_owned()));
    assert!(!found.iter().any(|path| path.starts_with(".convex/")));
}

#[tokio::test]
async fn excludes_convex_in_non_git_workspaces() {
    let dir = temp_dir("zc-ws-non-git-convex-");
    let cwd = path_of(&dir);
    write(&cwd, ".convex/local-storage/data.json", "{}");
    write(&cwd, "src/keep.ts", "export {};");
    let result = service().search(&cwd, &search("", 100, None)).await.unwrap();
    let found = paths(&result.entries);
    assert!(found.contains(&"src".to_owned()));
    assert!(found.contains(&"src/keep.ts".to_owned()));
    assert!(!found.iter().any(|path| path.starts_with(".convex/")));
}

#[tokio::test]
async fn supports_typo_resistant_file_search_through_fff() {
    let dir = temp_dir("zc-ws-fff-typo-");
    let cwd = path_of(&dir);
    write(&cwd, "src/components/Composer.tsx", "");
    let result = service().search(&cwd, &search("compoesr", 10, None)).await.unwrap();
    assert!(paths(&result.entries).contains(&"src/components/Composer.tsx".to_owned()));
}

/// Wraps the fff factory: counts creations, and can make the next `scanFiles` fail.
struct SpyFactory {
    created: AtomicUsize,
    fail_next_scan: Arc<AtomicBool>,
}

struct SpyFinder {
    inner: Arc<dyn Finder>,
    fail_next_scan: Arc<AtomicBool>,
}

impl Finder for SpyFinder {
    fn scan_progress(&self) -> Result<ScanProgress, FinderError> {
        self.inner.scan_progress()
    }
    fn file_search(&self, query: &str, page_size: usize) -> Result<PathSearchPage, FinderError> {
        self.inner.file_search(query, page_size)
    }
    fn directory_search(&self, query: &str, page_size: usize) -> Result<PathSearchPage, FinderError> {
        self.inner.directory_search(query, page_size)
    }
    fn mixed_search(&self, query: &str, page_size: usize) -> Result<MixedSearchPage, FinderError> {
        self.inner.mixed_search(query, page_size)
    }
    fn grep(&self, request: &GrepRequest) -> Result<GrepPage, FinderError> {
        self.inner.grep(request)
    }
    fn scan_files(&self) -> Result<(), FinderError> {
        if self.fail_next_scan.swap(false, Ordering::SeqCst) {
            return Err(FinderError::Returned("scan failed".into()));
        }
        self.inner.scan_files()
    }
}

impl FinderFactory for SpyFactory {
    fn create(&self, cwd: &str, variant: IndexVariant) -> Result<Arc<dyn Finder>, FinderError> {
        self.created.fetch_add(1, Ordering::SeqCst);
        let inner = FffFactory.create(cwd, variant)?;
        Ok(Arc::new(SpyFinder {
            inner,
            fail_next_scan: self.fail_next_scan.clone(),
        }))
    }
}

#[tokio::test]
async fn rebuilds_the_cached_index_after_refresh_fails() {
    let dir = temp_dir("zc-ws-refresh-failure-");
    let cwd = path_of(&dir);
    write(&cwd, "src/index.ts", "export {};\n");
    let factory = Arc::new(SpyFactory {
        created: AtomicUsize::new(0),
        fail_next_scan: Arc::new(AtomicBool::new(false)),
    });
    let entries = service_with(factory.clone());
    entries.list(&cwd, None).await.unwrap();
    assert_eq!(factory.created.load(Ordering::SeqCst), 1);
    // A successful refresh keeps the index.
    entries.refresh(&cwd).await;
    entries.list(&cwd, None).await.unwrap();
    assert_eq!(factory.created.load(Ordering::SeqCst), 1);
    factory.fail_next_scan.store(true, Ordering::SeqCst);
    entries.refresh(&cwd).await;
    entries.list(&cwd, None).await.unwrap();
    assert_eq!(factory.created.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn refresh_only_touches_existing_indexes() {
    let dir = temp_dir("zc-ws-refresh-noop-");
    let cwd = path_of(&dir);
    let factory = Arc::new(SpyFactory {
        created: AtomicUsize::new(0),
        fail_next_scan: Arc::new(AtomicBool::new(false)),
    });
    let entries = service_with(factory.clone());
    entries.refresh(&cwd).await;
    entries.refresh(&format!("{cwd}/missing")).await;
    assert_eq!(factory.created.load(Ordering::SeqCst), 0);
}

// ------------------------------------------------------------------------------------------
// searchContents
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn returns_content_matches_with_file_paths_line_numbers_and_ranges() {
    let dir = temp_dir("zc-ws-content-search-");
    let cwd = path_of(&dir);
    write(
        &cwd,
        "src/shapes.ts",
        "export const square = 4;\nexport const Square = 16;\nexport const squareSize = 8;\n",
    );
    write(&cwd, "src/other.ts", "const circle = true;\n");
    let result = service().search_contents(&cwd, &contents("Square", 100, false, true, false)).await.unwrap();
    let found: Vec<(String, i64)> = result.matches.iter().map(|m| (m.path.clone(), m.line_number)).collect();
    assert_eq!(found, [("src/shapes.ts".to_owned(), 1), ("src/shapes.ts".to_owned(), 2)]);
    assert_eq!(result.matches[0].match_ranges.len(), 1);
    assert_eq!((result.matches[0].match_ranges[0].start, result.matches[0].match_ranges[0].end), (13, 19));
    assert!(!result.truncated);
}

#[tokio::test]
async fn honors_case_sensitivity_and_gitignore_rules() {
    let dir = git_dir("zc-ws-content-ignore-");
    let cwd = path_of(&dir);
    write(&cwd, ".gitignore", "ignored.txt\n");
    write(&cwd, "src/keep.ts", "square\nSquare\n");
    write(&cwd, "ignored.txt", "Square\n");
    let result = service().search_contents(&cwd, &contents("Square", 100, true, false, false)).await.unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!((result.matches[0].path.as_str(), result.matches[0].line_number), ("src/keep.ts", 2));
}

fn ranges(result: &zc_contracts::ProjectContentMatch) -> Vec<(i64, i64)> {
    result.match_ranges.iter().map(|range| (range.start, range.end)).collect()
}

#[tokio::test]
async fn filters_whole_word_matches_by_word_boundaries_without_widening_ranges() {
    let dir = temp_dir("zc-ws-content-whole-word-");
    let cwd = path_of(&dir);
    write(&cwd, "src/words.ts", "note notes denote\nfootnote note\n");
    let result = service().search_contents(&cwd, &contents("note", 100, true, true, false)).await.unwrap();
    assert_eq!(result.matches.len(), 2);
    assert_eq!((result.matches[0].path.as_str(), result.matches[0].line_number), ("src/words.ts", 1));
    assert_eq!(ranges(&result.matches[0]), [(0, 4)]);
    assert_eq!(result.matches[1].line_number, 2);
    assert_eq!(ranges(&result.matches[1]), [(9, 13)]);
}

#[tokio::test]
async fn finds_later_whole_word_matches_in_a_file_after_rejected_raw_matches() {
    let dir = temp_dir("zc-ws-content-late-whole-word-");
    let cwd = path_of(&dir);
    write(&cwd, "src/words.ts", &format!("{}foo\n", "afoo\n".repeat(10)));
    let result = service().search_contents(&cwd, &contents("foo", 1, true, true, false)).await.unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].line_number, 11);
    assert_eq!(ranges(&result.matches[0]), [(0, 3)]);
}

#[tokio::test]
async fn treats_astral_plane_letters_as_whole_word_characters() {
    let dir = temp_dir("zc-ws-content-astral-word-");
    let cwd = path_of(&dir);
    write(&cwd, "src/words.ts", "𐐀foo foo foo𐐀\n");
    let result = service().search_contents(&cwd, &contents("foo", 100, true, true, false)).await.unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(ranges(&result.matches[0]), [(6, 9)]);
}

#[tokio::test]
async fn matches_punctuation_edged_whole_word_queries_including_adjacent_occurrences() {
    let dir = temp_dir("zc-ws-content-punctuation-");
    let cwd = path_of(&dir);
    write(&cwd, "src/words.ts", "-foo- -foo- -foo-\n");
    let result = service().search_contents(&cwd, &contents("-foo-", 100, true, true, false)).await.unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(ranges(&result.matches[0]), [(0, 5), (6, 11), (12, 17)]);
}

#[tokio::test]
async fn matches_punctuation_edged_regex_queries_as_whole_words() {
    let dir = temp_dir("zc-ws-content-regex-punctuation-");
    let cwd = path_of(&dir);
    write(&cwd, "src/words.ts", "foo- foo-\nafoo-b\n");
    let result = service().search_contents(&cwd, &contents("foo-", 100, true, true, true)).await.unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].line_number, 1);
    assert_eq!(ranges(&result.matches[0]), [(0, 4), (5, 9)]);
}

#[tokio::test]
async fn caps_matches_per_file_so_one_dense_file_cannot_fill_the_page() {
    let dir = temp_dir("zc-ws-content-per-file-cap-");
    let cwd = path_of(&dir);
    write(&cwd, "src/dense.ts", &"needle\n".repeat(300));
    write(&cwd, "src/other.ts", "needle\n");
    let result = service().search_contents(&cwd, &contents("needle", 500, true, false, false)).await.unwrap();
    let dense = result.matches.iter().filter(|m| m.path == "src/dense.ts").count();
    let other = result.matches.iter().filter(|m| m.path == "src/other.ts").count();
    assert_eq!((dense, other), (100, 1));
}

#[tokio::test]
async fn preserves_regex_escapes_during_case_insensitive_searches() {
    let dir = temp_dir("zc-ws-content-regex-");
    let cwd = path_of(&dir);
    write(&cwd, "src/shapes.ts", "Square\nsquare\n");
    let result = service().search_contents(&cwd, &contents("\\SQUARE", 100, false, false, true)).await.unwrap();
    assert_eq!(result.matches.iter().map(|m| m.line_number).collect::<Vec<_>>(), [1, 2]);
}

#[tokio::test]
async fn preserves_invalid_regex_errors_during_case_insensitive_searches() {
    let dir = temp_dir("zc-ws-content-invalid-regex-");
    let cwd = path_of(&dir);
    write(&cwd, "src/shapes.ts", "foobar\n");
    let result = service().search_contents(&cwd, &contents("foo)bar(", 100, false, false, true)).await.unwrap();
    assert!(result.regex_fallback_error.is_some());
    assert!(result.matches.is_empty());
}

#[tokio::test]
async fn maps_multi_byte_lines_to_string_indexed_ranges() {
    let dir = temp_dir("zc-ws-content-multibyte-");
    let cwd = path_of(&dir);
    write(&cwd, "src/notes.ts", "const label = \"héllo wörld\";\n");
    let result = service().search_contents(&cwd, &contents("wörld", 100, true, false, false)).await.unwrap();
    assert_eq!(result.matches.len(), 1);
    let found = &result.matches[0];
    let units: Vec<u16> = found.line_content.encode_utf16().collect();
    let range = &found.match_ranges[0];
    assert_eq!(String::from_utf16(&units[range.start as usize..range.end as usize]).unwrap(), "wörld");
}

#[tokio::test]
async fn reports_truncation_when_more_matches_exist() {
    let dir = temp_dir("zc-ws-content-truncated-");
    let cwd = path_of(&dir);
    for index in 0..5 {
        write(&cwd, &format!("src/file-{index}.ts"), "needle\nneedle\n");
    }
    let result = service().search_contents(&cwd, &contents("needle", 3, true, false, false)).await.unwrap();
    assert_eq!(result.matches.len(), 3);
    assert!(result.truncated);
}

// ------------------------------------------------------------------------------------------
// browse
// ------------------------------------------------------------------------------------------

fn browse(partial_path: &str, cwd: Option<&str>) -> BrowseRequest {
    BrowseRequest {
        partial_path: partial_path.into(),
        cwd: cwd.map(str::to_owned),
    }
}

fn browse_entry(cwd: &str, name: &str) -> FilesystemBrowseEntry {
    FilesystemBrowseEntry {
        name: name.into(),
        full_path: format!("{cwd}/{name}"),
    }
}

#[tokio::test]
async fn returns_matching_directories_and_excludes_files() {
    let dir = temp_dir("zc-ws-browse-prefix-");
    let cwd = path_of(&dir);
    write(&cwd, "alphabet.txt", "ignore me");
    write(&cwd, "alpha/index.ts", "export {};\n");
    write(&cwd, "Alpine/index.ts", "export {};\n");
    let result = service().browse(&browse(&format!("{cwd}/alp"), None)).await.unwrap();
    assert_eq!(
        result,
        FilesystemBrowseResult {
            parent_path: cwd.clone(),
            entries: vec![browse_entry(&cwd, "alpha"), browse_entry(&cwd, "Alpine")]
        }
    );
}

#[tokio::test]
async fn shows_dot_directories_in_directory_mode_and_hidden_prefix_mode() {
    let dir = temp_dir("zc-ws-browse-hidden-");
    let cwd = path_of(&dir);
    write(&cwd, ".config/settings.json", "{}");
    write(&cwd, "config/settings.json", "{}");
    let entries = service();
    let directory = entries.browse(&browse(&format!("{cwd}/"), None)).await.unwrap();
    assert_eq!(directory.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), [".config", "config"]);
    let hidden = entries.browse(&browse(&format!("{cwd}/.c"), None)).await.unwrap();
    assert_eq!(
        hidden,
        FilesystemBrowseResult {
            parent_path: cwd.clone(),
            entries: vec![browse_entry(&cwd, ".config")]
        }
    );
    // Without a dot prefix, dot directories stay hidden.
    let plain = entries.browse(&browse(&format!("{cwd}/c"), None)).await.unwrap();
    assert_eq!(plain.entries, vec![browse_entry(&cwd, "config")]);
}

#[tokio::test]
async fn supports_relative_paths_when_cwd_is_provided() {
    let dir = temp_dir("zc-ws-browse-relative-");
    let cwd = path_of(&dir);
    write(&cwd, "packages/pkg.json", "{}");
    let result = service().browse(&browse("./pack", Some(&cwd))).await.unwrap();
    assert_eq!(
        result,
        FilesystemBrowseResult {
            parent_path: cwd.clone(),
            entries: vec![browse_entry(&cwd, "packages")]
        }
    );
}

#[tokio::test]
async fn rejects_relative_paths_without_cwd() {
    let error = service().browse(&browse("./src", None)).await.unwrap_err();
    assert_eq!(error.tag(), "WorkspaceEntriesCurrentProjectRequiredError");
    assert_eq!(error.message(), "A current project is required to browse relative workspace path './src'.");
}

#[tokio::test]
async fn rejects_windows_paths_off_windows() {
    let entries = service().with_platform(zc_workspace::platform::NodePlatform::Darwin);
    let error = entries.browse(&browse("C:\\Users", Some("/w"))).await.unwrap_err();
    assert!(matches!(error, BrowseError::WindowsPathUnsupported { .. }));
    assert_eq!(
        error.message(),
        "Windows-style workspace path 'C:\\Users' is not supported on 'darwin' from '/w'."
    );
}

#[tokio::test]
async fn returns_an_empty_listing_when_the_os_denies_directory_access() {
    let dir = temp_dir("zc-ws-browse-eacces-");
    let cwd = path_of(&dir);
    let locked = Path::new(&cwd).join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let locked_path = locked.to_string_lossy().into_owned();
    let result = service().browse(&browse(&format!("{locked_path}/"), None)).await;
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Running as root bypasses the permission check entirely.
    if unsafe { libc::geteuid() } != 0 {
        assert_eq!(
            result.unwrap(),
            FilesystemBrowseResult {
                parent_path: locked_path,
                entries: vec![]
            }
        );
    }
}

#[tokio::test]
async fn reports_other_directory_read_failures() {
    let dir = temp_dir("zc-ws-browse-missing-");
    let cwd = path_of(&dir);
    let error = service().browse(&browse(&format!("{cwd}/missing/"), None)).await.unwrap_err();
    match &error {
        BrowseError::ReadDirectory(read) => assert_eq!(read.parent_path, format!("{cwd}/missing")),
        other => panic!("unexpected {other:?}"),
    }
    let defect = error.to_defect();
    assert_eq!(
        defect.0["cause"]["message"],
        format!("ENOENT: no such file or directory, scandir '{cwd}/missing'")
    );
}

#[tokio::test]
async fn expands_the_home_directory() {
    let home = std::env::var("HOME").unwrap();
    let result = service().browse(&browse("~", None)).await.unwrap();
    assert_eq!(result.parent_path, zc_workspace::paths::resolve_one(&home));
}
