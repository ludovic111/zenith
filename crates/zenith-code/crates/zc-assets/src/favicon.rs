//! `ProjectFaviconResolver.ts`: a representative favicon or app icon for a workspace.
//!
//! In order: the project's saved favicon path (used where it exists, so a grouped project's
//! other checkouts still fall back), the `t3.json` `iconPath`, the well-known locations, then
//! an icon `href` declared in a project source file (`<link rel="icon">` or object metadata).
//!
//! Answers are cached (512 entries; 10 minutes for a hit, 1 minute for a miss; failures are not
//! cached), and a cached hit is confirmed with one `stat`, so a deleted icon falls back at once.

use std::io;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use regex::Regex;
use zc_core::cache::TtlCache;
use zc_core::Defect;
use zc_workspace::errors::{platform_error_defect, TaggedError};
use zc_workspace::paths::{join, resolve_relative_path_within_root, WorkspacePaths};

use crate::preview::is_absolute;
use crate::project_file::load_t3_project_file;

const FAVICON_CACHE_CAPACITY: usize = 512;
const FAVICON_POSITIVE_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const FAVICON_NEGATIVE_CACHE_TTL: Duration = Duration::from_secs(60);

/// Well-known favicon paths, checked in order.
pub const FAVICON_CANDIDATES: &[&str] = &[
    "favicon.svg",
    "favicon.ico",
    "favicon.png",
    "public/favicon.svg",
    "public/favicon.ico",
    "public/favicon.png",
    "app/favicon.ico",
    "app/favicon.png",
    "app/icon.svg",
    "app/icon.png",
    "app/icon.ico",
    "src/favicon.ico",
    "src/favicon.svg",
    "src/app/favicon.ico",
    "src/app/icon.svg",
    "src/app/icon.png",
    "assets/icon.svg",
    "assets/icon.png",
    "assets/logo.svg",
    "assets/logo.png",
    ".idea/icon.svg",
];

/// Files that may declare a `<link rel="icon">` or icon metadata.
pub const ICON_SOURCE_FILES: &[&str] = &[
    "index.html",
    "public/index.html",
    "app/routes/__root.tsx",
    "src/routes/__root.tsx",
    "app/root.tsx",
    "src/root.tsx",
    "src/index.html",
];

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

fn link_rel_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r#"(?i)(?-u:\b)rel=["'](?:icon|shortcut icon)["']"#)
}

fn link_href_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r#"(?i)(?-u:\b)href=["']([^"'?]+)"#)
}

fn icon_rel_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r#"(?i)(?-u:\b)rel\s*:\s*["'](?:icon|shortcut icon)["']"#)
}

fn icon_href_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r#"(?i)(?-u:\b)href\s*:\s*["']([^"'?]+)"#)
}

/// `LINK_ICON_HTML_RE`: `<link\b(?=[^>]*\brel=…)(?=[^>]*\bhref=["']([^"'?]+))[^>]*>`, without
/// lookaheads. The greedy `[^>]*` makes the captured `href` the last one that starts inside the
/// tag.
fn extract_link_icon_href(source: &str) -> Option<String> {
    let lower = source.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find("<link") {
        let tag_start = from + found + "<link".len();
        from = from + found + 1;
        let at_boundary = lower.as_bytes().get(tag_start).is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'_'));
        if !at_boundary {
            continue;
        }
        // `[^>]*>`: the tag must close.
        let tag_length = source[tag_start..].find('>')?;
        let tag = &source[tag_start..tag_start + tag_length];
        if !link_rel_regex().is_match(tag) {
            continue;
        }
        let href = link_href_regex()
            .captures_iter(&source[tag_start..])
            .take_while(|captures| captures.get(0).is_some_and(|m| m.start() < tag_length))
            .last()
            .and_then(|captures| captures.get(1).map(|m| m.as_str().to_owned()));
        if href.is_some() {
            return href;
        }
    }
    None
}

/// `extractIconHref`: a `<link>` tag first, then icon metadata whose `rel` and `href` share a
/// brace-free run.
pub fn extract_icon_href(source: &str) -> Option<String> {
    if let Some(href) = extract_link_icon_href(source) {
        return Some(href);
    }
    source.split('}').filter(|run| icon_rel_regex().is_match(run)).find_map(|run| {
        icon_href_regex()
            .captures(run)
            .and_then(|captures| captures.get(1))
            .map(|m| m.as_str().to_owned())
    })
}

/// `ProjectFaviconResolutionError.operation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaviconOperation {
    NormalizeWorkspace,
    ResolvePath,
    StatCandidate,
    ReadSource,
}

impl FaviconOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NormalizeWorkspace => "normalize-workspace",
            Self::ResolvePath => "resolve-path",
            Self::StatCandidate => "stat-candidate",
            Self::ReadSource => "read-source",
        }
    }
}

/// `ProjectFaviconResolutionError`.
#[derive(Debug, Clone)]
pub struct ProjectFaviconResolutionError {
    pub operation: FaviconOperation,
    pub workspace_root: String,
    pub relative_path: Option<String>,
    pub absolute_path: Option<String>,
    pub cause: Defect,
}

impl TaggedError for ProjectFaviconResolutionError {
    fn tag(&self) -> &'static str {
        "ProjectFaviconResolutionError"
    }
    fn message(&self) -> String {
        format!(
            "Failed to resolve project favicon during {} for workspace {}.",
            self.operation.as_str(),
            self.workspace_root
        )
    }
    fn cause(&self) -> Option<&Defect> {
        Some(&self.cause)
    }
}

impl std::fmt::Display for ProjectFaviconResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&TaggedError::message(self))
    }
}

impl std::error::Error for ProjectFaviconResolutionError {}

/// A `stat` (`Ok(None)` when missing; `Ok(Some(is_regular_file))` otherwise).
pub type StatFn = Arc<dyn Fn(&str) -> io::Result<Option<bool>> + Send + Sync>;
/// A text read (`Ok(None)` when missing).
pub type ReadFn = Arc<dyn Fn(&str) -> io::Result<Option<String>> + Send + Sync>;

/// The file-system calls the resolver makes (injectable: the TS tests fail one `stat` or one
/// read).
#[derive(Clone)]
pub struct FaviconFs {
    /// `Ok(None)` when missing; `Ok(Some(is_regular_file))` otherwise (symlinks followed).
    pub stat: StatFn,
    /// `Ok(None)` when missing.
    pub read_to_string: ReadFn,
}

fn not_found_as_none<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

impl Default for FaviconFs {
    fn default() -> Self {
        Self {
            stat: Arc::new(|path| not_found_as_none(std::fs::metadata(path)).map(|metadata| metadata.map(|m| m.is_file()))),
            read_to_string: Arc::new(|path| not_found_as_none(std::fs::read(path)).map(|bytes| bytes.map(|b| String::from_utf8_lossy(&b).into_owned()))),
        }
    }
}

/// `ProjectFaviconResolver`.
#[derive(Clone)]
pub struct ProjectFaviconResolver {
    fs: FaviconFs,
    workspace_paths: WorkspacePaths,
    cache: TtlCache<String, Option<String>>,
}

impl Default for ProjectFaviconResolver {
    fn default() -> Self {
        Self::new(FaviconFs::default())
    }
}

impl std::fmt::Debug for ProjectFaviconResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectFaviconResolver").finish_non_exhaustive()
    }
}

/// Where a candidate may live: inside the workspace, or (for a saved absolute path) anywhere.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CandidateScope {
    Workspace,
    Filesystem,
}

impl ProjectFaviconResolver {
    pub fn new(fs: FaviconFs) -> Self {
        Self {
            fs,
            workspace_paths: WorkspacePaths::new(),
            cache: TtlCache::new(FAVICON_CACHE_CAPACITY, FAVICON_POSITIVE_CACHE_TTL),
        }
    }

    fn find_existing_file(&self, project_cwd: &str, candidates: &[String], scope: CandidateScope) -> Result<Option<String>, ProjectFaviconResolutionError> {
        for relative_path in candidates {
            let absolute_path = if scope == CandidateScope::Filesystem && is_absolute(relative_path) {
                relative_path.clone()
            } else {
                match resolve_relative_path_within_root(project_cwd, relative_path) {
                    Ok(resolved) => resolved.absolute_path,
                    Err(_) => continue,
                }
            };
            let is_file = (self.fs.stat)(&absolute_path).map_err(|error| ProjectFaviconResolutionError {
                operation: FaviconOperation::StatCandidate,
                workspace_root: project_cwd.to_owned(),
                relative_path: Some(relative_path.clone()),
                absolute_path: Some(absolute_path.clone()),
                cause: platform_error_defect(&error, "stat", "stat", &absolute_path),
            })?;
            if is_file == Some(true) {
                return Ok(Some(absolute_path));
            }
        }
        Ok(None)
    }

    /// `resolvePathUncached` (blocking).
    pub fn resolve_path_uncached(&self, cwd: &str, favicon_path: Option<&str>) -> Result<Option<String>, ProjectFaviconResolutionError> {
        let project_cwd = self
            .workspace_paths
            .normalize_workspace_root(cwd, false)
            .map_err(|error| ProjectFaviconResolutionError {
                operation: FaviconOperation::NormalizeWorkspace,
                workspace_root: cwd.to_owned(),
                relative_path: None,
                absolute_path: None,
                cause: error.to_defect(),
            })?;
        if let Some(favicon_path) = favicon_path {
            if let Some(existing) = self.find_existing_file(&project_cwd, &[favicon_path.to_owned()], CandidateScope::Filesystem)? {
                return Ok(Some(existing));
            }
        }
        if let Some(icon_path) = load_t3_project_file(&project_cwd).and_then(|file| file.icon_path) {
            if let Some(existing) = self.find_existing_file(&project_cwd, &[icon_path], CandidateScope::Workspace)? {
                return Ok(Some(existing));
            }
        }
        for candidate in FAVICON_CANDIDATES {
            if let Some(existing) = self.find_existing_file(&project_cwd, &[(*candidate).to_owned()], CandidateScope::Workspace)? {
                return Ok(Some(existing));
            }
        }
        for source_file in ICON_SOURCE_FILES {
            let source_path = resolve_relative_path_within_root(&project_cwd, source_file).map_err(|error| ProjectFaviconResolutionError {
                operation: FaviconOperation::ResolvePath,
                workspace_root: project_cwd.clone(),
                relative_path: Some((*source_file).to_owned()),
                absolute_path: None,
                cause: error.to_defect(),
            })?;
            let source = (self.fs.read_to_string)(&source_path.absolute_path).map_err(|error| ProjectFaviconResolutionError {
                operation: FaviconOperation::ReadSource,
                workspace_root: project_cwd.clone(),
                relative_path: Some((*source_file).to_owned()),
                absolute_path: Some(source_path.absolute_path.clone()),
                cause: platform_error_defect(&error, "readFileString", "open", &source_path.absolute_path),
            })?;
            let Some(href) = source.as_deref().and_then(extract_icon_href) else {
                continue;
            };
            let clean = href.strip_prefix('/').unwrap_or(&href);
            let candidates = [join("public", clean), clean.to_owned()];
            if let Some(existing) = self.find_existing_file(&project_cwd, &candidates, CandidateScope::Workspace)? {
                return Ok(Some(existing));
            }
        }
        Ok(None)
    }

    fn cached_or_resolve(&self, key: &str, cwd: &str, favicon_path: Option<&str>) -> Result<Option<String>, ProjectFaviconResolutionError> {
        if let Some(cached) = self.cache.get(&key.to_owned()) {
            return Ok(cached);
        }
        let resolved = self.resolve_path_uncached(cwd, favicon_path)?;
        let ttl = if resolved.is_some() {
            FAVICON_POSITIVE_CACHE_TTL
        } else {
            FAVICON_NEGATIVE_CACHE_TTL
        };
        self.cache.insert_with_ttl(key.to_owned(), resolved.clone(), ttl);
        Ok(resolved)
    }

    /// `resolvePath` (blocking): the icon file, or `None`.
    pub fn resolve_path_blocking(&self, cwd: &str, favicon_path: Option<&str>) -> Result<Option<String>, ProjectFaviconResolutionError> {
        let key = format!("{}\0{cwd}", favicon_path.unwrap_or(""));
        let Some(cached) = self.cached_or_resolve(&key, cwd, favicon_path)? else {
            return Ok(None);
        };
        let is_file = (self.fs.stat)(&cached).map_err(|error| ProjectFaviconResolutionError {
            operation: FaviconOperation::StatCandidate,
            workspace_root: cwd.to_owned(),
            relative_path: None,
            absolute_path: Some(cached.clone()),
            cause: platform_error_defect(&error, "stat", "stat", &cached),
        })?;
        if is_file == Some(true) {
            return Ok(Some(cached));
        }
        self.cache.invalidate(&key);
        self.cached_or_resolve(&key, cwd, favicon_path)
    }

    /// `resolvePath`.
    pub async fn resolve_path(&self, cwd: &str, favicon_path: Option<&str>) -> Result<Option<String>, ProjectFaviconResolutionError> {
        let this = self.clone();
        let cwd = cwd.to_owned();
        let favicon_path = favicon_path.map(str::to_owned);
        tokio::task::spawn_blocking(move || this.resolve_path_blocking(&cwd, favicon_path.as_deref()))
            .await
            .unwrap_or_else(|error| {
                Err(ProjectFaviconResolutionError {
                    operation: FaviconOperation::StatCandidate,
                    workspace_root: String::new(),
                    relative_path: None,
                    absolute_path: None,
                    cause: Defect::error("Error", error.to_string()),
                })
            })
    }
}

#[cfg(test)]
mod tests {
    //! `ProjectFaviconResolver.test.ts`.
    use std::path::Path;

    use super::*;

    fn temp_dir() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        (dir, path)
    }

    fn write(cwd: &str, relative: &str, contents: &str) {
        let path = Path::new(cwd).join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn p(cwd: &str, parts: &[&str]) -> String {
        parts.iter().fold(cwd.to_owned(), |acc, part| format!("{acc}/{part}"))
    }

    #[tokio::test(start_paused = true)]
    async fn serves_repeated_resolves_from_cache() {
        let resolver = ProjectFaviconResolver::default();
        let (_dir, cwd) = temp_dir();
        write(&cwd, "public/favicon.svg", "<svg>public</svg>");
        let resolved = resolver.resolve_path(&cwd, None).await.unwrap();
        assert_eq!(resolved, Some(p(&cwd, &["public", "favicon.svg"])));
        write(&cwd, "favicon.svg", "<svg>root</svg>");
        for _ in 0..3 {
            assert_eq!(resolver.resolve_path(&cwd, None).await.unwrap(), resolved);
        }
        tokio::time::advance(Duration::from_secs(11 * 60)).await;
        assert_eq!(resolver.resolve_path(&cwd, None).await.unwrap(), Some(p(&cwd, &["favicon.svg"])));
    }

    #[tokio::test(start_paused = true)]
    async fn falls_back_at_once_when_a_cached_favicon_is_deleted() {
        let resolver = ProjectFaviconResolver::default();
        let (_dir, cwd) = temp_dir();
        write(&cwd, "favicon.svg", "<svg>favicon</svg>");
        assert!(resolver.resolve_path(&cwd, None).await.unwrap().is_some());
        std::fs::remove_file(p(&cwd, &["favicon.svg"])).unwrap();
        assert_eq!(resolver.resolve_path(&cwd, None).await.unwrap(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn re_probes_after_the_negative_ttl() {
        let resolver = ProjectFaviconResolver::default();
        let (_dir, cwd) = temp_dir();
        assert_eq!(resolver.resolve_path(&cwd, None).await.unwrap(), None);
        write(&cwd, "favicon.svg", "<svg>favicon</svg>");
        assert_eq!(resolver.resolve_path(&cwd, None).await.unwrap(), None);
        tokio::time::advance(Duration::from_secs(2 * 60)).await;
        assert!(resolver.resolve_path(&cwd, None).await.unwrap().is_some());
    }

    #[test]
    fn prefers_well_known_files_then_t3_json_and_saved_overrides() {
        let resolver = ProjectFaviconResolver::default();
        let (_dir, cwd) = temp_dir();
        write(&cwd, "favicon.svg", "<svg>favicon</svg>");
        assert_eq!(resolver.resolve_path_uncached(&cwd, None).unwrap(), Some(p(&cwd, &["favicon.svg"])));

        write(&cwd, "t3.json", r#"{ "iconPath": "brand/mark.svg" }"#);
        write(&cwd, "brand/mark.svg", "<svg>mark</svg>");
        assert_eq!(resolver.resolve_path_uncached(&cwd, None).unwrap(), Some(p(&cwd, &["brand", "mark.svg"])));

        write(&cwd, "brand/custom.svg", "<svg>custom</svg>");
        assert_eq!(
            resolver.resolve_path_uncached(&cwd, Some("brand/custom.svg")).unwrap(),
            Some(p(&cwd, &["brand", "custom.svg"]))
        );
        // A saved path missing from this checkout falls back.
        assert_eq!(
            resolver.resolve_path_uncached(&cwd, Some("brand/missing.svg")).unwrap(),
            Some(p(&cwd, &["brand", "mark.svg"]))
        );
    }

    #[test]
    fn uses_a_saved_favicon_outside_the_workspace() {
        let resolver = ProjectFaviconResolver::default();
        let (_dir, cwd) = temp_dir();
        let (_pictures_dir, pictures) = temp_dir();
        write(&pictures, "custom.png", "image");
        let external = p(&pictures, &["custom.png"]);
        assert_eq!(resolver.resolve_path_uncached(&cwd, Some(&external)).unwrap(), Some(external));
    }

    #[test]
    fn falls_back_when_t3_json_is_missing_its_icon_or_invalid() {
        let resolver = ProjectFaviconResolver::default();
        let (_dir, cwd) = temp_dir();
        write(&cwd, "t3.json", r#"{ "iconPath": "brand/missing.svg" }"#);
        write(&cwd, "favicon.svg", "<svg>favicon</svg>");
        assert_eq!(resolver.resolve_path_uncached(&cwd, None).unwrap(), Some(p(&cwd, &["favicon.svg"])));
        write(&cwd, "t3.json", "{ not json");
        assert_eq!(resolver.resolve_path_uncached(&cwd, None).unwrap(), Some(p(&cwd, &["favicon.svg"])));
    }

    #[test]
    fn does_not_resolve_a_t3_json_icon_outside_the_root() {
        let resolver = ProjectFaviconResolver::default();
        let (_dir, parent) = temp_dir();
        let cwd = p(&parent, &["app"]);
        write(&parent, "secret.svg", "<svg>secret</svg>");
        write(&cwd, "t3.json", r#"{ "iconPath": "../secret.svg" }"#);
        assert_eq!(resolver.resolve_path_uncached(&cwd, None).unwrap(), None);
    }

    #[test]
    fn resolves_icon_hrefs_from_project_sources() {
        let resolver = ProjectFaviconResolver::default();
        let logo = |cwd: &str| Some(p(cwd, &["public", "brand", "logo.svg"]));
        let cases: &[(&str, &str)] = &[
            ("index.html", r#"<link rel="icon" href="/brand/logo.svg">"#),
            (
                "src/routes/__root.tsx",
                "export const Route = createRootRoute({\n  head: () => ({\n    links: [\n      { rel: \"stylesheet\", href: \"/app.css\" },\n      { rel: \"icon\", href: \"/brand/logo.svg\" },\n    ],\n  }),\n});",
            ),
            ("src/root.tsx", r#"const links = [{ href: "/brand/logo.svg", rel: "shortcut icon" }];"#),
            ("src/root.tsx", r#"const links = [{ attributes: {}, rel: "icon", href: "/brand/logo.svg" }];"#),
            ("src/root.tsx", r#"const links = [{ rel: "icon" }, { rel: "icon", href: "/brand/logo.svg" }];"#),
        ];
        for (file, source) in cases {
            let (_dir, cwd) = temp_dir();
            write(&cwd, file, source);
            write(&cwd, "public/brand/logo.svg", "<svg>brand</svg>");
            assert_eq!(resolver.resolve_path_uncached(&cwd, None).unwrap(), logo(&cwd), "{file}: {source}");
        }
    }

    #[test]
    fn scans_large_sources_quickly() {
        let resolver = ProjectFaviconResolver::default();
        let (_dir, cwd) = temp_dir();
        let filler = format!("<p>{}</p>\n", "pokopia companion guide ".repeat(24));
        write(
            &cwd,
            "index.html",
            &format!(
                "<!doctype html><html><head><title>guide</title></head><body>\n{}</body></html>",
                filler.repeat(1200)
            ),
        );
        let started = std::time::Instant::now();
        assert_eq!(resolver.resolve_path_uncached(&cwd, None).unwrap(), None);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn returns_none_without_an_icon_and_skips_outside_hrefs() {
        let resolver = ProjectFaviconResolver::default();
        let (_dir, cwd) = temp_dir();
        assert_eq!(resolver.resolve_path_uncached(&cwd, None).unwrap(), None);
        write(&cwd, "index.html", r#"<link rel="icon" href="../../secret.svg">"#);
        assert_eq!(resolver.resolve_path_uncached(&cwd, None).unwrap(), None);
        write(&cwd, "public/index.html", r#"<link rel="icon" href="/brand/logo.svg">"#);
        write(&cwd, "public/brand/logo.svg", "<svg>brand</svg>");
        assert_eq!(
            resolver.resolve_path_uncached(&cwd, None).unwrap(),
            Some(p(&cwd, &["public", "brand", "logo.svg"]))
        );
    }

    #[test]
    fn preserves_workspace_normalization_context() {
        let resolver = ProjectFaviconResolver::default();
        let (_dir, cwd) = temp_dir();
        let missing = p(&cwd, &["missing"]);
        let error = resolver.resolve_path_uncached(&missing, None).unwrap_err();
        assert_eq!(error.operation, FaviconOperation::NormalizeWorkspace);
        assert_eq!(error.workspace_root, missing);
        assert_eq!(error.cause.0["name"], "WorkspaceRootNotExistsError");
    }

    #[test]
    fn preserves_candidate_stat_and_source_read_failures() {
        let (_dir, cwd) = temp_dir();
        let favicon = p(&cwd, &["favicon.svg"]);
        let failing_stat = favicon.clone();
        let default = FaviconFs::default();
        let resolver = ProjectFaviconResolver::new(FaviconFs {
            stat: Arc::new(move |path| {
                if path == failing_stat {
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))
                } else {
                    (FaviconFs::default().stat)(path)
                }
            }),
            read_to_string: default.read_to_string.clone(),
        });
        let error = resolver.resolve_path_uncached(&cwd, None).unwrap_err();
        assert_eq!(error.operation, FaviconOperation::StatCandidate);
        assert_eq!(error.relative_path.as_deref(), Some("favicon.svg"));
        assert_eq!(error.absolute_path.as_deref(), Some(favicon.as_str()));

        write(&cwd, "index.html", r#"<link rel="icon" href="/favicon.svg">"#);
        let source = p(&cwd, &["index.html"]);
        let failing_read = source.clone();
        let resolver = ProjectFaviconResolver::new(FaviconFs {
            stat: default.stat.clone(),
            read_to_string: Arc::new(move |path| {
                if path == failing_read {
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))
                } else {
                    (FaviconFs::default().read_to_string)(path)
                }
            }),
        });
        let error = resolver.resolve_path_uncached(&cwd, None).unwrap_err();
        assert_eq!(error.operation, FaviconOperation::ReadSource);
        assert_eq!(error.relative_path.as_deref(), Some("index.html"));
        assert_eq!(error.absolute_path.as_deref(), Some(source.as_str()));
    }

    #[test]
    fn extracts_the_last_href_of_an_icon_link() {
        assert_eq!(extract_icon_href(r#"<LINK data-href="x" href='y.png' rel="icon">"#).as_deref(), Some("y.png"));
        assert_eq!(
            extract_icon_href(r#"<link rel="stylesheet" href="a.css"><link rel="icon" href="b.ico?v=1">"#).as_deref(),
            Some("b.ico")
        );
        assert_eq!(extract_icon_href(r#"<linkx rel="icon" href="a.svg">"#), None);
    }
}
