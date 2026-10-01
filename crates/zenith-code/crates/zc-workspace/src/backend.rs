//! The search engine behind [`crate::search_index`]: the [`Finder`] trait (the slice of
//! `@ff-labs/fff-node`'s `FileFinder` the TS uses) and [`FffFinder`], its implementation on the
//! fff Rust library itself.
//!
//! `@ff-labs/fff-node` 0.9.4 is a thin FFI layer over `crates/fff-c` of the fff repository,
//! which is itself a thin C layer over `crates/fff-core` (the `fff-search` crate). We depend on
//! `fff-search` at the same tag (v0.9.4) and repeat, call for call, what `fff-c` does for each
//! `FileFinder` method the TS calls, with the same defaults (`0` → default page sizes, combo
//! boost 100 / 3, 10 MB grep file cap, no frecency or history database). The ranking, the grep
//! paging and time budget, the ignore rules and the background watcher are therefore fff's own.
//!
//! Like the FFI, every call either returns its result, returns an error string (a "returned
//! diagnostic", [`FinderError::Returned`]) or throws ([`FinderError::Threw`]: a Rust panic,
//! caught here the way the JS `try` catches a thrown FFI error). The TS keeps the two apart:
//! only thrown errors become a `cause`.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use fff_search as fff;
use zc_contracts::ProjectEntryKind;
use zc_core::Defect;

/// A failed finder call.
#[derive(Debug, Clone, PartialEq)]
pub enum FinderError {
    /// `{ ok: false, error }`: the library reported a failure.
    Returned(String),
    /// The call threw (panicked); the defect is the thrown error.
    Threw(Defect),
}

/// Which index a finder backs: the path index, or the on-demand content index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IndexVariant {
    Paths,
    Content,
}

impl IndexVariant {
    pub const ALL: [Self; 2] = [Self::Paths, Self::Content];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Paths => "paths",
            Self::Content => "content",
        }
    }
}

/// `getScanProgress()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanProgress {
    pub is_scanning: bool,
    pub is_warmup_complete: bool,
}

/// A page of `fileSearch` / `directorySearch`: relative paths in rank order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PathSearchPage {
    pub items: Vec<String>,
    pub total_matched: usize,
}

/// A page of `mixedSearch`: `(type, relativePath)` in rank order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MixedSearchPage {
    pub items: Vec<(ProjectEntryKind, String)>,
    pub total_matched: usize,
}

/// `GrepOptions.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrepMode {
    Plain,
    Regex,
}

/// The `grep(query, options)` options the TS sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepRequest {
    pub query: String,
    pub mode: GrepMode,
    pub smart_case: bool,
    pub max_matches_per_file: usize,
    pub page_size: usize,
    /// `cursor._offset`; `None` for the first page.
    pub cursor: Option<usize>,
    pub time_budget_ms: u64,
}

/// One grep match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepHit {
    pub relative_path: String,
    pub line_number: u64,
    pub line_content: String,
    /// Byte ranges within `line_content`.
    pub match_ranges: Vec<(u32, u32)>,
}

/// A grep page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GrepPage {
    pub items: Vec<GrepHit>,
    /// `nextCursor._offset`, absent when there are no more files.
    pub next_cursor: Option<usize>,
    pub regex_fallback_error: Option<String>,
}

/// One live index over a workspace root (an fff `FileFinder` instance). Dropping it destroys
/// the instance and stops its watcher.
pub trait Finder: Send + Sync + 'static {
    fn scan_progress(&self) -> Result<ScanProgress, FinderError>;
    fn file_search(&self, query: &str, page_size: usize) -> Result<PathSearchPage, FinderError>;
    fn directory_search(&self, query: &str, page_size: usize) -> Result<PathSearchPage, FinderError>;
    fn mixed_search(&self, query: &str, page_size: usize) -> Result<MixedSearchPage, FinderError>;
    fn grep(&self, request: &GrepRequest) -> Result<GrepPage, FinderError>;
    /// `scanFiles()`: starts a full rescan in the background.
    fn scan_files(&self) -> Result<(), FinderError>;
}

/// `FileFinder.create`.
pub trait FinderFactory: Send + Sync + 'static {
    fn create(&self, cwd: &str, variant: IndexVariant) -> Result<Arc<dyn Finder>, FinderError>;
}

fn panic_defect(payload: Box<dyn std::any::Any + Send>) -> Defect {
    let message = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "native call panicked".to_owned());
    Defect::error("Error", message)
}

/// Runs a native call, turning a panic into [`FinderError::Threw`].
fn guarded<T>(call: impl FnOnce() -> Result<T, FinderError>) -> Result<T, FinderError> {
    catch_unwind(AssertUnwindSafe(call)).unwrap_or_else(|payload| Err(FinderError::Threw(panic_defect(payload))))
}

/// What a C string round trip does to a Rust string: `CString::new(s).unwrap_or_default()` in
/// `fff-c`, read back with `readCString(…) ?? ""` in fff-node.
fn through_c_string(value: String) -> String {
    if value.contains('\0') {
        String::new()
    } else {
        value
    }
}

/// [`FinderFactory`] over the fff library.
#[derive(Debug, Clone, Copy, Default)]
pub struct FffFactory;

impl FinderFactory for FffFactory {
    fn create(&self, cwd: &str, variant: IndexVariant) -> Result<Arc<dyn Finder>, FinderError> {
        guarded(|| FffFinder::create(cwd, variant).map(|finder| Arc::new(finder) as Arc<dyn Finder>))
    }
}

/// One fff instance: `fff_create_instance_with` + the per-call glue of `fff-c`.
pub struct FffFinder {
    picker: fff::SharedFilePicker,
    frecency: fff::SharedFrecency,
    query_tracker: fff::SharedQueryTracker,
}

impl FffFinder {
    /// `FileFinder.create({ basePath, disableMmapCache: true, disableContentIndexing:
    /// variant !== "content", aiMode: false, enableFsRootScanning: true,
    /// enableHomeDirScanning: true })`: no frecency/history databases, watcher on, Neovim mode,
    /// automatic cache budget, symlinks not followed.
    pub fn create(cwd: &str, variant: IndexVariant) -> Result<Self, FinderError> {
        if cwd.is_empty() {
            return Err(FinderError::Returned("opts.base_path is null or empty".into()));
        }
        let picker = fff::SharedFilePicker::default();
        let frecency = fff::SharedFrecency::default();
        let query_tracker = fff::SharedQueryTracker::default();
        fff::FilePicker::new_with_shared_state(
            picker.clone(),
            frecency.clone(),
            fff::FilePickerOptions {
                base_path: cwd.to_owned(),
                enable_mmap_cache: false,
                enable_content_indexing: variant == IndexVariant::Content,
                watch: true,
                mode: fff::FFFMode::Neovim,
                cache_budget: fff::ContentCacheBudget::from_overrides(0, 0, 0),
                follow_symlinks: false,
                enable_fs_root_scanning: true,
                enable_home_dir_scanning: true,
            },
        )
        .map_err(|error| FinderError::Returned(format!("Failed to init file picker: {error}")))?;
        Ok(Self {
            picker,
            frecency,
            query_tracker,
        })
    }

    fn with_picker<T>(&self, call: impl FnOnce(&fff::FilePicker) -> Result<T, FinderError>) -> Result<T, FinderError> {
        guarded(|| {
            let guard = self
                .picker
                .read()
                .map_err(|error| FinderError::Returned(format!("Failed to acquire file picker lock: {error}")))?;
            let picker = guard
                .as_ref()
                .ok_or_else(|| FinderError::Returned("File picker not initialized. Call fff_create_instance first.".into()))?;
            call(picker)
        })
    }

    fn fuzzy_options(picker: &fff::FilePicker, page_size: usize, combo: bool) -> fff::FuzzySearchOptions<'_> {
        fff::FuzzySearchOptions {
            max_threads: 0,
            current_file: None,
            project_path: Some(picker.base_path()),
            combo_boost_score_multiplier: if combo { 100 } else { 0 },
            min_combo_count: if combo { 3 } else { 0 },
            pagination: fff::PaginationArgs {
                offset: 0,
                limit: if page_size == 0 { 100 } else { page_size },
            },
        }
    }
}

impl Finder for FffFinder {
    fn scan_progress(&self) -> Result<ScanProgress, FinderError> {
        self.with_picker(|picker| {
            let progress = picker.get_scan_progress();
            Ok(ScanProgress {
                is_scanning: progress.is_scanning,
                is_warmup_complete: progress.is_warmup_complete,
            })
        })
    }

    fn file_search(&self, query: &str, page_size: usize) -> Result<PathSearchPage, FinderError> {
        self.with_picker(|picker| {
            let tracker_guard = self
                .query_tracker
                .read()
                .map_err(|_| FinderError::Returned("Failed to acquire query tracker lock".into()))?;
            let parsed = fff::QueryParser::default().parse(query);
            let result = picker.fuzzy_search(&parsed, tracker_guard.as_ref(), Self::fuzzy_options(picker, page_size, true));
            Ok(PathSearchPage {
                items: result.items.iter().map(|item| through_c_string(item.relative_path(picker))).collect(),
                total_matched: result.total_matched,
            })
        })
    }

    fn directory_search(&self, query: &str, page_size: usize) -> Result<PathSearchPage, FinderError> {
        self.with_picker(|picker| {
            let parsed = fff::QueryParser::new(fff::DirSearchConfig).parse(query);
            let result = picker.fuzzy_search_directories(&parsed, Self::fuzzy_options(picker, page_size, false));
            Ok(PathSearchPage {
                items: result.items.iter().map(|item| through_c_string(item.relative_path(picker))).collect(),
                total_matched: result.total_matched,
            })
        })
    }

    fn mixed_search(&self, query: &str, page_size: usize) -> Result<MixedSearchPage, FinderError> {
        self.with_picker(|picker| {
            let tracker_guard = self
                .query_tracker
                .read()
                .map_err(|_| FinderError::Returned("Failed to acquire query tracker lock".into()))?;
            let parsed = fff::QueryParser::new(fff::MixedSearchConfig).parse(query);
            let result = picker.fuzzy_search_mixed(&parsed, tracker_guard.as_ref(), Self::fuzzy_options(picker, page_size, true));
            Ok(MixedSearchPage {
                items: result
                    .items
                    .iter()
                    .map(|item| match item {
                        fff::MixedItemRef::File(file) => (ProjectEntryKind::File, through_c_string(file.relative_path(picker))),
                        fff::MixedItemRef::Dir(dir) => (ProjectEntryKind::Directory, through_c_string(dir.relative_path(picker))),
                    })
                    .collect(),
                total_matched: result.total_matched,
            })
        })
    }

    fn grep(&self, request: &GrepRequest) -> Result<GrepPage, FinderError> {
        self.with_picker(|picker| {
            let parsed = if picker.mode().is_ai() {
                fff::QueryParser::new(fff::AiGrepConfig).parse(&request.query)
            } else {
                fff::grep::parse_grep_query(&request.query)
            };
            let options = fff::GrepSearchOptions {
                max_file_size: 10 * 1024 * 1024,
                max_matches_per_file: request.max_matches_per_file,
                smart_case: request.smart_case,
                file_offset: request.cursor.unwrap_or(0),
                page_limit: if request.page_size == 0 { 50 } else { request.page_size },
                mode: match request.mode {
                    GrepMode::Plain => fff::GrepMode::PlainText,
                    GrepMode::Regex => fff::GrepMode::Regex,
                },
                time_budget_ms: request.time_budget_ms,
                before_context: 0,
                after_context: 0,
                classify_definitions: false,
                trim_whitespace: false,
                abort_signal: None,
            };
            let result = picker.grep(&parsed, &options);
            let items = result
                .matches
                .iter()
                .map(|hit| {
                    let file = result.files[hit.file_index];
                    GrepHit {
                        relative_path: through_c_string(file.relative_path(picker)),
                        line_number: hit.line_number,
                        line_content: through_c_string(hit.line_content.clone()),
                        match_ranges: hit.match_byte_offsets.iter().copied().collect(),
                    }
                })
                .collect();
            // The FFI narrows the offset to u32 and fff-node treats 0 as "no more files".
            let next = result.next_file_offset as u32 as usize;
            Ok(GrepPage {
                items,
                next_cursor: (next > 0).then_some(next),
                regex_fallback_error: result.regex_fallback_error.clone().map(through_c_string).filter(|error| !error.is_empty()),
            })
        })
    }

    fn scan_files(&self) -> Result<(), FinderError> {
        guarded(|| {
            self.picker
                .trigger_full_rescan_async(&self.frecency)
                .map_err(|error| FinderError::Returned(format!("Failed to trigger rescan: {error}")))
        })
    }
}

impl Drop for FffFinder {
    /// `fff_destroy`: drop the picker (cancels in-flight scans and stops the watcher), then the
    /// frecency and query-tracker slots.
    fn drop(&mut self) {
        if let Ok(mut guard) = self.picker.write() {
            drop(guard.take());
        }
        if let Ok(mut guard) = self.frecency.write() {
            *guard = None;
        }
        if let Ok(mut guard) = self.query_tracker.write() {
            *guard = None;
        }
    }
}
