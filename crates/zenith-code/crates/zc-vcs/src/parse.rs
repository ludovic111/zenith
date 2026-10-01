//! Pure parsing helpers of `GitVcsDriverCore.ts` and `GitVcsDriver.ts`, ported one to one
//! (including their quirks: rename arrows in non-`-z` numstat, space-split porcelain paths).

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;

use crate::contracts::{ReviewDiffFileStat, VcsRef};
use crate::remote_refs::parse_remote_ref_with_remote_names;

/// JS `Number.parseInt(value, 10)`: leading whitespace, an optional sign, then digits; `None`
/// for `NaN`.
pub fn js_parse_int(value: &str) -> Option<i64> {
    let trimmed = value.trim_start();
    let (negative, rest) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let parsed: i64 = digits.parse().unwrap_or(i64::MAX);
    Some(if negative { -parsed } else { parsed })
}

/// `parseBranchAb`: `+3 -1` → (3, 1).
pub fn parse_branch_ab(value: &str) -> (u64, u64) {
    static AB: OnceLock<Regex> = OnceLock::new();
    let ab = AB.get_or_init(|| Regex::new(r"^\+(\d+)\s+-(\d+)$").unwrap());
    match ab.captures(value) {
        Some(c) => (c[1].parse().unwrap_or(0), c[2].parse().unwrap_or(0)),
        None => (0, 0),
    }
}

/// One `git diff --numstat` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumstatEntry {
    pub path: String,
    pub insertions: u64,
    pub deletions: u64,
}

fn split_lines_crlf(stdout: &str) -> impl Iterator<Item = &str> {
    stdout.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line))
}

/// `parseNumstatEntries` (non-`-z` numstat).
pub fn parse_numstat_entries(stdout: &str) -> Vec<NumstatEntry> {
    let mut entries = Vec::new();
    for line in split_lines_crlf(stdout) {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.split('\t');
        let added_raw = parts.next().unwrap_or("0");
        let deleted_raw = parts.next().unwrap_or("0");
        let path_parts: Vec<&str> = parts.collect();
        let raw_path = if path_parts.len() > 1 {
            path_parts.last().copied().unwrap_or_default().trim().to_owned()
        } else {
            path_parts.join("\t").trim().to_owned()
        };
        if raw_path.is_empty() {
            continue;
        }
        let normalized = match raw_path.find(" => ") {
            Some(index) => raw_path[index + 4..].trim().to_owned(),
            None => raw_path.clone(),
        };
        entries.push(NumstatEntry {
            path: if normalized.is_empty() { raw_path } else { normalized },
            insertions: js_parse_int(added_raw).map(|v| v.max(0) as u64).unwrap_or(0),
            deletions: js_parse_int(deleted_raw).map(|v| v.max(0) as u64).unwrap_or(0),
        });
    }
    entries
}

/// `parseReviewNumstat` (`--numstat -z`): renames carry two path fields.
pub fn parse_review_numstat(stdout: &str) -> Vec<ReviewDiffFileStat> {
    static FIELD: OnceLock<Regex> = OnceLock::new();
    let field_re = FIELD.get_or_init(|| Regex::new(r"(?s)^(\d+|-)\t(\d+|-)\t(.*)$").unwrap());
    let fields: Vec<&str> = stdout.split('\0').collect();
    let mut files = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let field = fields[index];
        if let Some(c) = field_re.captures(field) {
            let additions = &c[1];
            let deletions = &c[2];
            let rest = c.get(3).map(|m| m.as_str()).unwrap_or_default();
            let (path, previous_path) = if rest.is_empty() {
                index += 1;
                let previous = fields.get(index).copied().unwrap_or_default().to_owned();
                index += 1;
                let path = fields.get(index).copied().unwrap_or_default().to_owned();
                (path, Some(previous))
            } else {
                (rest.to_owned(), None)
            };
            files.push(ReviewDiffFileStat {
                path,
                previous_path,
                additions: if additions == "-" { 0 } else { additions.parse().unwrap_or(0) },
                deletions: if deletions == "-" { 0 } else { deletions.parse().unwrap_or(0) },
            });
        }
        index += 1;
    }
    files
}

/// `parsePorcelainPath` (porcelain v2 entry → its path).
pub fn parse_porcelain_path(line: &str) -> Option<String> {
    if line.starts_with("? ") || line.starts_with("! ") {
        let simple = line[2..].trim();
        return (!simple.is_empty()).then(|| simple.to_owned());
    }
    if !(line.starts_with("1 ") || line.starts_with("2 ") || line.starts_with("u ")) {
        return None;
    }
    if let Some(tab) = line.find('\t') {
        let from_tab = &line[tab + 1..];
        let file_path = from_tab.split('\t').next().unwrap_or_default().trim();
        return (!file_path.is_empty()).then(|| file_path.to_owned());
    }
    let last = line.split_whitespace().last().unwrap_or_default();
    (!last.is_empty()).then(|| last.to_owned())
}

/// `filterBranchesForListQuery`.
pub fn filter_refs_for_query(refs: Vec<VcsRef>, query: Option<&str>) -> Vec<VcsRef> {
    match query.filter(|q| !q.is_empty()) {
        None => refs,
        Some(query) => {
            let query = query.to_lowercase();
            refs.into_iter().filter(|r| r.name.to_lowercase().contains(&query)).collect()
        }
    }
}

/// `paginateBranches` (default limit 100).
pub fn paginate_refs(refs: Vec<VcsRef>, cursor: Option<u64>, limit: Option<u64>) -> (Vec<VcsRef>, Option<u64>, u64) {
    let cursor = cursor.unwrap_or(0) as usize;
    let limit = limit.unwrap_or(100) as usize;
    let total = refs.len();
    let page: Vec<VcsRef> = refs.into_iter().skip(cursor).take(limit).collect();
    let next = if cursor + page.len() < total {
        Some((cursor + page.len()) as u64)
    } else {
        None
    };
    (page, next, total as u64)
}

/// `parseWorktreeBranchPaths` (`git worktree list --porcelain -z`): branch → path, skipping
/// prunable worktrees. Insertion order is kept.
pub fn parse_worktree_branch_paths(stdout: &str) -> Vec<(String, String)> {
    let mut map: Vec<(String, String)> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_branch: Option<String> = None;
    let mut prunable = false;
    let mut flush = |path: &mut Option<String>, branch: &mut Option<String>, prunable: &mut bool| {
        if let (Some(p), Some(b)) = (path.take(), branch.take()) {
            if !*prunable {
                if let Some(existing) = map.iter_mut().find(|(name, _)| *name == b) {
                    existing.1 = p;
                } else {
                    map.push((b, p));
                }
            }
        }
        *path = None;
        *branch = None;
        *prunable = false;
    };
    for field in stdout.split('\0') {
        if field.is_empty() {
            flush(&mut current_path, &mut current_branch, &mut prunable);
        } else if let Some(path) = field.strip_prefix("worktree ") {
            current_path = Some(path.to_owned());
        } else if let Some(branch) = field.strip_prefix("branch refs/heads/") {
            current_branch = Some(branch.to_owned());
        } else if field == "prunable" || field.starts_with("prunable ") {
            prunable = true;
        }
    }
    flush(&mut current_path, &mut current_branch, &mut prunable);
    map
}

/// `splitNullSeparatedPaths`: drops an unterminated final path from truncated output.
pub fn split_null_separated_paths(input: &str, truncated: bool) -> Vec<String> {
    let mut parts: Vec<&str> = input.split('\0').collect();
    if truncated && parts.last().is_some_and(|last| !last.is_empty()) {
        parts.pop();
    }
    parts.into_iter().filter(|value| !value.is_empty()).map(str::to_owned).collect()
}

/// `sanitizeRemoteName`.
pub fn sanitize_remote_name(value: &str) -> String {
    static INVALID: OnceLock<Regex> = OnceLock::new();
    static EDGES: OnceLock<Regex> = OnceLock::new();
    let invalid = INVALID.get_or_init(|| Regex::new(r"[^A-Za-z0-9._-]+").unwrap());
    let edges = EDGES.get_or_init(|| Regex::new(r"^-+|-+$").unwrap());
    let replaced = invalid.replace_all(value.trim(), "-");
    let sanitized = edges.replace_all(&replaced, "").into_owned();
    if sanitized.is_empty() {
        "fork".to_owned()
    } else {
        sanitized
    }
}

fn remote_verbose_line() -> &'static Regex {
    static LINE: OnceLock<Regex> = OnceLock::new();
    LINE.get_or_init(|| Regex::new(r"^(\S+)\s+(\S+)\s+\((fetch|push)\)$").unwrap())
}

/// `parseRemoteFetchUrls` (`git remote -v`): name → fetch URL, in first-seen order.
pub fn parse_remote_fetch_urls(stdout: &str) -> Vec<(String, String)> {
    let mut remotes: Vec<(String, String)> = Vec::new();
    for line in stdout.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(c) = remote_verbose_line().captures(trimmed) else {
            continue;
        };
        if &c[3] != "fetch" {
            continue;
        }
        let (name, url) = (c[1].to_owned(), c[2].to_owned());
        if let Some(existing) = remotes.iter_mut().find(|(n, _)| *n == name) {
            existing.1 = url;
        } else {
            remotes.push((name, url));
        }
    }
    remotes
}

/// One remote of `parseGitRemoteVerboseOutput`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerboseRemote {
    pub url: Option<String>,
    pub push_url: Option<String>,
}

/// `parseGitRemoteVerboseOutput` (`GitVcsDriver.ts`): name → fetch and push URLs.
pub fn parse_git_remote_verbose_output(output: &str) -> Vec<(String, VerboseRemote)> {
    let mut remotes: Vec<(String, VerboseRemote)> = Vec::new();
    for line in output.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(c) = remote_verbose_line().captures(trimmed) else {
            continue;
        };
        let name = c[1].to_owned();
        let url = c[2].to_owned();
        let index = match remotes.iter().position(|(n, _)| *n == name) {
            Some(index) => index,
            None => {
                remotes.push((name, VerboseRemote::default()));
                remotes.len() - 1
            }
        };
        if &c[3] == "fetch" {
            remotes[index].1.url = Some(url);
        } else {
            remotes[index].1.push_url = Some(url);
        }
    }
    remotes
}

/// An upstream ref split into remote and branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamRef {
    pub upstream_ref: String,
    pub remote_name: String,
    pub branch_name: String,
}

/// `parseUpstreamRefWithRemoteNames`.
pub fn parse_upstream_ref_with_remote_names(upstream_ref: &str, remote_names: &[String]) -> Option<UpstreamRef> {
    parse_remote_ref_with_remote_names(upstream_ref, remote_names).map(|parsed| UpstreamRef {
        upstream_ref: upstream_ref.to_owned(),
        remote_name: parsed.remote_name,
        branch_name: parsed.branch_name,
    })
}

/// `parseUpstreamRefByFirstSeparator`.
pub fn parse_upstream_ref_by_first_separator(upstream_ref: &str) -> Option<UpstreamRef> {
    let index = upstream_ref.find('/')?;
    if index == 0 || index == upstream_ref.len() - 1 {
        return None;
    }
    let remote_name = upstream_ref[..index].trim();
    let branch_name = upstream_ref[index + 1..].trim();
    if remote_name.is_empty() || branch_name.is_empty() {
        return None;
    }
    Some(UpstreamRef {
        upstream_ref: upstream_ref.to_owned(),
        remote_name: remote_name.to_owned(),
        branch_name: branch_name.to_owned(),
    })
}

/// `parseTrackingBranchByUpstreamRef` (`for-each-ref --format=%(refname:short)\t%(upstream:short)`).
pub fn parse_tracking_branch_by_upstream_ref(stdout: &str, upstream_ref: &str) -> Option<String> {
    for line in stdout.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split('\t');
        let branch = parts.next().unwrap_or_default().trim();
        let candidate = parts.next().unwrap_or_default().trim();
        if branch.is_empty() || candidate.is_empty() {
            continue;
        }
        if candidate == upstream_ref {
            return Some(branch.to_owned());
        }
    }
    None
}

/// `deriveLocalBranchNameFromRemoteRef` (core version): `None` without a usable separator.
pub fn derive_local_branch_from_remote_ref(branch_name: &str) -> Option<String> {
    let index = branch_name.find('/')?;
    if index == 0 || index == branch_name.len() - 1 {
        return None;
    }
    let local = branch_name[index + 1..].trim();
    (!local.is_empty()).then(|| local.to_owned())
}

/// `parseDefaultBranchFromRemoteHeadRef`.
pub fn parse_default_branch_from_remote_head_ref(value: &str, remote_name: &str) -> Option<String> {
    let trimmed = value.trim();
    let prefix = format!("refs/remotes/{remote_name}/");
    let rest = trimmed.strip_prefix(&prefix)?.trim();
    (!rest.is_empty()).then(|| rest.to_owned())
}

/// `isNonRepositoryGitStderr`.
pub fn is_non_repository_git_stderr(stderr: &str) -> bool {
    stderr.to_lowercase().contains("not a git repository")
}

/// `isUnbornHeadStderr`.
pub fn is_unborn_head_stderr(stderr: &str) -> bool {
    let normalized = stderr.to_lowercase();
    normalized.contains("bad revision 'head'") || (normalized.contains("unknown revision") && normalized.contains("path not in the working tree"))
}

/// `isMissingWorktreeStderr`.
pub fn is_missing_worktree_stderr(stderr: &str) -> bool {
    let normalized = stderr.to_lowercase();
    normalized.contains("is not a working tree") || normalized.contains("cannot remove working tree")
}

/// `fetchFailureDetail`: a fixed diagnosis for recognized fetch failures (stderr itself can
/// carry credentials and never leaves the process).
pub fn fetch_failure_detail(stderr: &str) -> Option<&'static str> {
    static AUTH: OnceLock<Regex> = OnceLock::new();
    static NETWORK: OnceLock<Regex> = OnceLock::new();
    static MISSING: OnceLock<Regex> = OnceLock::new();
    static LOCK: OnceLock<Regex> = OnceLock::new();
    let auth = AUTH.get_or_init(|| {
        Regex::new(r"(?i)^(?:fatal: (?:Authentication failed|could not read (?:Username|Password))\b|\S+: Permission denied \(publickey)").unwrap()
    });
    let network = NETWORK.get_or_init(|| {
        Regex::new(r"(?i)^(?:(?:fatal: |ssh: )?Could not resolve host(?:name)?\b|fatal: unable to access .+: (?:Could not resolve host|Failed to connect)\b|ssh: connect to host \S+ port \d+: (?:Connection timed out|Connection refused|Network is unreachable)\b)").unwrap()
    });
    let missing = MISSING.get_or_init(|| {
        Regex::new(r"(?i)^(?:remote: Repository not found\.?$|fatal: repository .+ not found$|fatal: .+ does not appear to be a git repository$)").unwrap()
    });
    let lock = LOCK.get_or_init(|| Regex::new(r#"(?i)^(?:(?:error|fatal): cannot lock ref\b|fatal: Unable to create ['"].+\.lock['"]:)"#).unwrap());
    let lines: Vec<&str> = stderr.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line).trim()).collect();
    if lines.iter().any(|line| auth.is_match(line)) {
        return Some("Git could not authenticate with the remote. Check Git credentials or SSH access on the server, then retry.");
    }
    if lines.iter().any(|line| network.is_match(line)) {
        return Some("Git could not reach the remote. Check the server's network connection and remote host, then retry.");
    }
    if lines.iter().any(|line| missing.is_match(line)) {
        return Some("Git could not access the remote repository. Check the remote URL and repository permissions on the server.");
    }
    if lines.iter().any(|line| lock.is_match(line)) {
        return Some("Git could not update a local reference. Another Git operation or a stale lock may be blocking the fetch; check the repository on the server, then retry.");
    }
    None
}

/// `git worktree add` checkout progress.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CheckoutProgressLine {
    pub percent: f64,
    pub completed: u64,
    pub total: u64,
}

/// `parseGitCheckoutProgressLine`: `Updating files:  78% (2104/2700)`.
pub fn parse_git_checkout_progress_line(line: &str) -> Option<CheckoutProgressLine> {
    static PROGRESS: OnceLock<Regex> = OnceLock::new();
    let progress = PROGRESS.get_or_init(|| Regex::new(r"Updating files:\s+(\d+)%\s+\((\d+)/(\d+)\)").unwrap());
    let c = progress.captures(line)?;
    let percent: f64 = c[1].parse().ok()?;
    let completed: u64 = c[2].parse().ok()?;
    let total: u64 = c[3].parse().ok()?;
    Some(CheckoutProgressLine {
        percent: percent.clamp(0.0, 100.0),
        completed,
        total,
    })
}

/// `chunkPathsForGitCheckIgnore`: NUL-terminated batches of at most 256 KiB.
pub fn chunk_paths_for_git_check_ignore(relative_paths: &[String]) -> Vec<Vec<String>> {
    const MAX: usize = 256 * 1024;
    let mut chunks = Vec::new();
    let mut chunk: Vec<String> = Vec::new();
    let mut chunk_bytes = 0;
    for path in relative_paths {
        let bytes = path.len() + 1;
        if !chunk.is_empty() && chunk_bytes + bytes > MAX {
            chunks.push(std::mem::take(&mut chunk));
            chunk_bytes = 0;
        }
        chunk.push(path.clone());
        chunk_bytes += bytes;
        if chunk_bytes >= MAX {
            chunks.push(std::mem::take(&mut chunk));
            chunk_bytes = 0;
        }
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

/// Merges staged and unstaged unborn-HEAD numstat output, as `readStatusDetailsLocal` does.
pub fn merge_unborn_numstat(unstaged: &str, staged: &str) -> String {
    let mut order: Vec<String> = Vec::new();
    let mut totals: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for entry in parse_numstat_entries(staged).into_iter().chain(parse_numstat_entries(unstaged)) {
        let slot = totals.entry(entry.path.clone()).or_insert_with(|| {
            order.push(entry.path.clone());
            (0, 0)
        });
        slot.0 += entry.insertions;
        slot.1 += entry.deletions;
    }
    order
        .iter()
        .map(|path| {
            let (i, d) = totals[path];
            format!("{i}\t{d}\t{path}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numstat_like_js() {
        let entries = parse_numstat_entries("3\t1\tsrc/a.ts\r\n-\t-\timage.png\n2\t0\told => new\n\n");
        assert_eq!(
            entries,
            vec![
                NumstatEntry {
                    path: "src/a.ts".into(),
                    insertions: 3,
                    deletions: 1
                },
                NumstatEntry {
                    path: "image.png".into(),
                    insertions: 0,
                    deletions: 0
                },
                NumstatEntry {
                    path: "new".into(),
                    insertions: 2,
                    deletions: 0
                },
            ]
        );
    }

    #[test]
    fn parses_review_numstat_with_renames() {
        let stdout = "1\t2\ta.txt\0-\t-\tbin.png\0\
                      0\t0\t\0old name.md\0new\tname.md\0";
        let files = parse_review_numstat(stdout);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, "a.txt");
        assert_eq!(files[1].additions, 0);
        assert_eq!(files[2].previous_path.as_deref(), Some("old name.md"));
        assert_eq!(files[2].path, "new\tname.md");
    }

    #[test]
    fn parses_porcelain_paths() {
        assert_eq!(parse_porcelain_path("? new file.txt").as_deref(), Some("new file.txt"));
        assert_eq!(
            parse_porcelain_path("1 .M N... 100644 100644 100644 abc abc src/a.ts").as_deref(),
            Some("src/a.ts")
        );
        assert_eq!(
            // TS reads the field after the tab: the *original* path of a rename.
            parse_porcelain_path("2 R. N... 100644 100644 100644 a b R100 new.ts\told.ts").as_deref(),
            Some("old.ts")
        );
        assert_eq!(parse_porcelain_path("# branch.head main"), None);
    }

    #[test]
    fn splits_null_separated_paths() {
        assert_eq!(split_null_separated_paths("a\0b\0partial", true), vec!["a", "b"]);
        assert_eq!(split_null_separated_paths("a\0b\0last", false), vec!["a", "b", "last"]);
        assert_eq!(split_null_separated_paths("a\0b\0", true), vec!["a", "b"]);
    }

    #[test]
    fn parses_worktree_lists() {
        let stdout = "worktree /r\0HEAD abc\0branch refs/heads/main\0\0\
                      worktree /r/wt\0HEAD def\0branch refs/heads/feature\0prunable gitdir file points to non-existent location\0\0\
                      worktree /r/wt2\0HEAD 123\0detached\0\0";
        assert_eq!(parse_worktree_branch_paths(stdout), vec![("main".to_owned(), "/r".to_owned())]);
    }

    #[test]
    fn diagnoses_fetch_failures_without_echoing_stderr() {
        assert!(fetch_failure_detail("fatal: Authentication failed for 'https://x'")
            .unwrap()
            .contains("authenticate"));
        assert!(fetch_failure_detail("ssh: Could not resolve hostname x: nodename").unwrap().contains("reach"));
        assert!(
            fetch_failure_detail("fatal: '/tmp/x' does not appear to be a git repository\nfatal: Could not read from remote repository.")
                .unwrap()
                .contains("access the remote repository")
        );
        assert!(fetch_failure_detail("error: cannot lock ref 'refs/remotes/origin/main'")
            .unwrap()
            .contains("local reference"));
        assert_eq!(fetch_failure_detail("something else"), None);
    }

    #[test]
    fn parses_checkout_progress() {
        let p = parse_git_checkout_progress_line("Updating files:  78% (2104/2700)").unwrap();
        assert_eq!((p.percent, p.completed, p.total), (78.0, 2104, 2700));
        assert!(parse_git_checkout_progress_line("Preparing worktree").is_none());
    }

    #[test]
    fn chunks_check_ignore_input() {
        let long = "x".repeat(200 * 1024);
        let paths = vec![long.clone(), long.clone(), "a".into()];
        let chunks = chunk_paths_for_git_check_ignore(&paths);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1].len(), 2);
    }

    #[test]
    fn misc_helpers() {
        assert_eq!(parse_branch_ab("+3 -1"), (3, 1));
        assert_eq!(parse_branch_ab("garbage"), (0, 0));
        assert_eq!(sanitize_remote_name("  my fork!! "), "my-fork");
        assert_eq!(sanitize_remote_name("!!!"), "fork");
        assert_eq!(js_parse_int(" 12abc"), Some(12));
        assert_eq!(js_parse_int("-"), None);
        assert_eq!(
            parse_default_branch_from_remote_head_ref("refs/remotes/origin/main\n", "origin").as_deref(),
            Some("main")
        );
        assert_eq!(derive_local_branch_from_remote_ref("origin/feature/x").as_deref(), Some("feature/x"));
        assert_eq!(derive_local_branch_from_remote_ref("main"), None);
        assert_eq!(
            parse_tracking_branch_by_upstream_ref("main\torigin/main\nfeat\torigin/feat\n", "origin/feat").as_deref(),
            Some("feat")
        );
        assert_eq!(merge_unborn_numstat("1\t0\ta\n2\t1\tb", "3\t0\ta"), "4\t0\ta\n2\t1\tb");
    }
}
