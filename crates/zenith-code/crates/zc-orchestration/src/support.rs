//! Small shared helpers the decider and projector use, ported from `packages/shared` and
//! `packages/contracts`: JS date parsing and comparison, `localeCompare`, project path
//! comparison, and the thread pull request link helpers (`shared/threadPullRequests.ts`).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use regex::Regex;
use zc_contracts::{
    LitLucide, ProjectIconOverride, ProjectIconOverrideLucide, ProjectId, RepositoryIdentity, ThreadLinkedPullRequest, ThreadPullRequestLink,
    ThreadPullRequestLinkSource,
};
use zc_db::pr_keys::{self, ThreadPullRequestKey};

/// `Date.parse` for the ISO strings the server stores (`None` is `NaN`).
pub fn date_parse(value: &str) -> Option<i64> {
    zc_core::time::parse_iso_millis(value)
}

/// `zc_db::collate`'s primary key of an ASCII byte.
fn ascii_primary(byte: u8) -> (u8, u8) {
    /// CLDR root order of the ASCII punctuation and symbols (as in `zc_db::collate`).
    const PUNCTUATION_ORDER: &[u8] = br#"_-,;:!?.'"()[]{}@*/\&#%`^+<=>|~$"#;
    if (byte as char).is_whitespace() {
        (0, byte)
    } else if let Some(index) = PUNCTUATION_ORDER.iter().position(|p| *p == byte) {
        (1, index as u8)
    } else if byte.is_ascii_digit() {
        (2, byte)
    } else if byte.is_ascii_alphabetic() {
        (3, byte.to_ascii_lowercase())
    } else {
        (4, byte)
    }
}

/// `String.prototype.localeCompare` (ICU root collation, see `zc_db::collate`), with an
/// allocation-free path for ASCII strings (ids, timestamps) that orders them the same way.
pub fn locale_compare(left: &str, right: &str) -> Ordering {
    if left == right {
        return Ordering::Equal;
    }
    if !(left.is_ascii() && right.is_ascii()) {
        return zc_db::collate::locale_compare(left, right);
    }
    let primary_order = left.bytes().map(ascii_primary).cmp(right.bytes().map(ascii_primary));
    if primary_order != Ordering::Equal {
        return primary_order;
    }
    let tertiary = |byte: u8| u8::from(byte.is_ascii_uppercase());
    let tertiary_order = left.bytes().map(tertiary).cmp(right.bytes().map(tertiary));
    if tertiary_order != Ordering::Equal {
        return tertiary_order;
    }
    left.cmp(right)
}

fn is_zoned_iso_date_time(value: &str) -> bool {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        Regex::new(
            r"^(?:\d{4}|[+-]\d{6})-(?:0[1-9]|1[0-2])-(?:0[1-9]|[12]\d|3[01])T(?:(?:[01]\d|2[0-3]):[0-5]\d(?::[0-5]\d(?:\.\d+)?)?|24:00(?::00(?:\.0+)?)?)(?:Z|[+-](?:[01]\d|2[0-3]):[0-5]\d)$",
        )
        .expect("static regex")
    });
    value.trim() == value && pattern.is_match(value)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// `parseTimestamp` of `shared/dateTime.ts`: only zoned ISO date-times on real calendar days.
fn parse_timestamp(value: &str) -> Option<i64> {
    if !is_zoned_iso_date_time(value) {
        return None;
    }
    let date_part = &value[..value.find('T')?];
    let (year_text, rest) = date_part.rsplit_once('-').and_then(|(head, day)| {
        let (year, month) = head.rsplit_once('-')?;
        Some((year, (month, day)))
    })?;
    let year: i64 = year_text.parse().ok()?;
    let month: u32 = rest.0.parse().ok()?;
    let day: u32 = rest.1.parse().ok()?;
    if day > days_in_month(year, month) {
        return None;
    }
    date_parse(value)
}

/// `compareDateTimeStrings`: by absolute time; valid timestamps sort after invalid ones, and two
/// invalid ones compare as strings.
pub fn compare_date_time_strings(left: &str, right: &str) -> Ordering {
    match (parse_timestamp(left), parse_timestamp(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => left.cmp(right),
    }
}

/// `isImportedAgentSessionMessageId` (`contracts/agentSessions.ts`).
pub fn is_imported_agent_session_message_id(message_id: &str) -> bool {
    message_id.starts_with("import:")
}

/// `WORKTREE_SETUP_ACTIVITY_KIND` (`contracts/worktreeSetup.ts`).
pub const WORKTREE_SETUP_ACTIVITY_KIND: &str = "worktree-setup";

/// `MAX_SCRIPT_ID_LENGTH` (`contracts/keybindings.ts`).
pub const MAX_SCRIPT_ID_LENGTH: usize = 24;

/// `Schema.is(SCRIPT_RUN_COMMAND_PATTERN)` for `script.<id>.run`: the id is 1–24 characters of
/// lowercase letters, digits and hyphens, starting with a letter or digit.
pub fn is_valid_script_id(id: &str) -> bool {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| Regex::new(r"^[a-z0-9][a-z0-9-]*$").expect("static regex"));
    !id.is_empty() && id.chars().count() <= MAX_SCRIPT_ID_LENGTH && pattern.is_match(id)
}

fn is_windows_drive_path(value: &str) -> bool {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN
        .get_or_init(|| Regex::new(r"^[a-zA-Z]:([/\\]|$)").expect("static regex"))
        .is_match(value)
}

fn is_root_path(value: &str) -> bool {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    value == "/" || value == "\\" || PATTERN.get_or_init(|| Regex::new(r"^[a-zA-Z]:[/\\]$").expect("static regex")).is_match(value)
}

fn trim_trailing_path_separators(value: &str) -> String {
    if value.is_empty() || is_root_path(value) {
        return value.to_owned();
    }
    let trimmed = if value.starts_with('/') {
        value.trim_end_matches('/')
    } else {
        value.trim_end_matches(['\\', '/'])
    };
    if trimmed.is_empty() {
        return value.to_owned();
    }
    let bytes = trimmed.as_bytes();
    if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return format!("{trimmed}\\");
    }
    trimmed.to_owned()
}

/// `String.prototype.trim`.
pub fn js_trim(value: &str) -> &str {
    pr_keys::js_trim(value)
}

/// `normalizeProjectPathForComparison` (`shared/path.ts`).
pub fn normalize_project_path_for_comparison(value: &str) -> String {
    let normalized = trim_trailing_path_separators(js_trim(value));
    if is_windows_drive_path(&normalized) || normalized.starts_with("\\\\") {
        return normalized.replace('/', "\\").to_lowercase();
    }
    normalized
}

// ---------------------------------------------------------------------------------------------
// Project icons (`ProjectIconOverride` in `contracts/orchestration.ts`)
// ---------------------------------------------------------------------------------------------

/// The monogram text of a project icon as TS decodes it: a `monogram` icon's text, or the
/// `monogramText ?? monogram` an older peer's lucide icon carries.
pub fn project_icon_monogram(icon: &ProjectIconOverride) -> Option<&str> {
    match icon {
        ProjectIconOverride::Monogram(monogram) => Some(&monogram.text),
        ProjectIconOverride::Lucide(lucide) => lucide.monogram_text.as_deref().or(lucide.monogram.as_deref()),
        ProjectIconOverride::Emoji(_) => None,
    }
}

/// The icon as the TS encoder writes it (decode, then encode): a monogram travels as
/// `{kind: "lucide", name: "folder-code", color, monogramText}` so older peers still read a
/// lucide icon; a lucide icon without monogram loses any stray keys.
pub fn canonical_project_icon(icon: &ProjectIconOverride) -> ProjectIconOverride {
    let color = match icon {
        ProjectIconOverride::Emoji(_) => return icon.clone(),
        ProjectIconOverride::Lucide(lucide) => lucide.color,
        ProjectIconOverride::Monogram(monogram) => monogram.color,
    };
    match (icon, project_icon_monogram(icon)) {
        (_, Some(text)) => ProjectIconOverride::Lucide(ProjectIconOverrideLucide {
            kind: LitLucide,
            name: "folder-code".to_owned(),
            color,
            monogram_text: Some(text.to_owned()),
            monogram: None,
        }),
        (ProjectIconOverride::Lucide(lucide), None) => ProjectIconOverride::Lucide(ProjectIconOverrideLucide {
            monogram_text: None,
            monogram: None,
            ..lucide.clone()
        }),
        _ => icon.clone(),
    }
}

// ---------------------------------------------------------------------------------------------
// Thread pull request links (`shared/threadPullRequests.ts`)
// ---------------------------------------------------------------------------------------------

/// What `normalizeThreadPullRequestKey` accepts: a key, optionally with the link URL.
#[derive(Debug, Clone, Copy)]
pub struct KeySource<'a> {
    pub host: &'a str,
    pub repository: &'a str,
    pub number: i64,
    pub url: Option<&'a str>,
}

impl<'a> KeySource<'a> {
    pub fn of_link(link: &'a ThreadPullRequestLink) -> Self {
        Self {
            host: &link.host,
            repository: &link.repository,
            number: link.number,
            url: Some(&link.url),
        }
    }

    pub fn of_key(key: &'a ThreadPullRequestKey) -> Self {
        Self {
            host: &key.host,
            repository: &key.repository,
            number: key.number,
            url: None,
        }
    }
}

/// `normalizeThreadPullRequestKey`.
pub fn normalize_key(source: KeySource<'_>) -> ThreadPullRequestKey {
    pr_keys::normalize_thread_pull_request_key(source.host, source.repository, source.number, None, source.url)
}

/// `threadPullRequestKeyOf`.
pub fn key_of(source: KeySource<'_>) -> String {
    let key = normalize_key(source);
    format!("{}/{}#{}", key.host, key.repository, key.number)
}

/// `threadPullRequestKeysEqual`.
pub fn keys_equal(left: KeySource<'_>, right: KeySource<'_>) -> bool {
    key_of(left) == key_of(right)
}

fn link_key(link: &ThreadPullRequestLink) -> String {
    key_of(KeySource::of_link(link))
}

fn is_open(link: &ThreadPullRequestLink) -> bool {
    match &link.snapshot {
        None => true,
        Some(snapshot) => snapshot.state.as_str() == "open",
    }
}

fn latest_updated_at(link: &ThreadPullRequestLink) -> i64 {
    let value = link
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.updated_at.as_deref())
        .unwrap_or(&link.linked_at);
    date_parse(value).unwrap_or(0)
}

/// `resolveThreadPullRequestChains`: native stacks first, then base→head chains.
fn resolve_chains<'a>(links: &[&'a ThreadPullRequestLink]) -> Vec<Vec<&'a ThreadPullRequestLink>> {
    let visible: Vec<&ThreadPullRequestLink> = links
        .iter()
        .copied()
        .filter(|link| link.source != ThreadPullRequestLinkSource::StackDismissed)
        .collect();
    let mut chains: Vec<Vec<&ThreadPullRequestLink>> = Vec::new();
    let mut placed: HashSet<String> = HashSet::new();

    let mut native_order: Vec<String> = Vec::new();
    let mut native_stacks: HashMap<String, Vec<&ThreadPullRequestLink>> = HashMap::new();
    for link in &visible {
        let Some(stack) = &link.stack else { continue };
        let key = normalize_key(KeySource::of_link(link));
        let stack_key = format!("{}/{}#stack:{}", key.host, key.repository, stack.id);
        if !native_stacks.contains_key(&stack_key) {
            native_order.push(stack_key.clone());
        }
        native_stacks.entry(stack_key).or_default().push(link);
    }
    for stack_key in native_order {
        let mut members = native_stacks.remove(&stack_key).unwrap_or_default();
        let order: HashMap<i64, usize> = members[0]
            .stack
            .as_ref()
            .map(|stack| {
                let mut order = HashMap::new();
                for (index, layer) in stack.layers.iter().enumerate() {
                    order.insert(layer.number, index);
                }
                order
            })
            .unwrap_or_default();
        members.sort_by_key(|member| order.get(&member.number).copied().unwrap_or(0));
        for member in &members {
            placed.insert(link_key(member));
        }
        chains.push(members);
    }

    let remaining: Vec<&ThreadPullRequestLink> = visible.iter().copied().filter(|link| !placed.contains(&link_key(link))).collect();
    let branch_key = |link: &ThreadPullRequestLink, branch: &str| {
        let key = normalize_key(KeySource::of_link(link));
        format!("{}/{}:{}", key.host, key.repository, branch)
    };
    // Reused head names cannot identify a parent unambiguously.
    let mut by_head: HashMap<String, Option<&ThreadPullRequestLink>> = HashMap::new();
    for link in &remaining {
        let Some(snapshot) = &link.snapshot else { continue };
        let key = branch_key(link, &snapshot.head_branch);
        by_head.entry(key).and_modify(|parent| *parent = None).or_insert(Some(link));
    }
    let mut has_child: HashSet<String> = HashSet::new();
    for link in &remaining {
        let Some(snapshot) = &link.snapshot else { continue };
        if let Some(Some(parent)) = by_head.get(&branch_key(link, &snapshot.base_branch)) {
            if !std::ptr::eq(*parent, *link) {
                has_child.insert(link_key(parent));
            }
        }
    }
    for top in &remaining {
        if has_child.contains(&link_key(top)) {
            continue;
        }
        let mut layers: Vec<&ThreadPullRequestLink> = Vec::new();
        let mut cursor: Option<&ThreadPullRequestLink> = Some(top);
        while let Some(current) = cursor {
            if placed.contains(&link_key(current)) {
                break;
            }
            placed.insert(link_key(current));
            layers.insert(0, current);
            cursor = match &current.snapshot {
                None => None,
                Some(snapshot) => by_head.get(&branch_key(current, &snapshot.base_branch)).copied().flatten(),
            };
        }
        if !layers.is_empty() {
            chains.push(layers);
        }
    }
    for link in &remaining {
        if !placed.contains(&link_key(link)) {
            chains.push(vec![link]);
        }
    }
    chains
}

/// `resolveThreadCurrentPullRequestLink`: the one link a one-slot surface shows.
fn resolve_current_link<'a>(links: &[&'a ThreadPullRequestLink]) -> Option<&'a ThreadPullRequestLink> {
    let visible: Vec<&ThreadPullRequestLink> = links
        .iter()
        .copied()
        .filter(|link| link.source != ThreadPullRequestLinkSource::StackDismissed)
        .collect();
    if visible.is_empty() {
        return None;
    }
    let open: Vec<&ThreadPullRequestLink> = visible.iter().copied().filter(|link| is_open(link)).collect();
    if open.len() == 1 {
        return Some(open[0]);
    }
    let chains = resolve_chains(&visible);
    if open.len() > 1 {
        let mut open_chains: Vec<Vec<&ThreadPullRequestLink>> = chains
            .iter()
            .map(|chain| chain.iter().rev().copied().filter(|link| is_open(link)).collect::<Vec<_>>())
            .filter(|layers: &Vec<&ThreadPullRequestLink>| !layers.is_empty())
            .collect();
        let max_linked = |layers: &Vec<&ThreadPullRequestLink>| -> Option<i64> {
            let mut max: Option<i64> = None;
            for link in layers {
                // `Math.max` over `Date.parse`: one NaN makes the whole max NaN.
                let parsed = date_parse(&link.linked_at)?;
                max = Some(max.map_or(parsed, |current: i64| current.max(parsed)));
            }
            max
        };
        open_chains.sort_by(|left, right| match (max_linked(right), max_linked(left)) {
            (Some(right), Some(left)) => right.cmp(&left),
            _ => Ordering::Equal,
        });
        return open_chains.into_iter().flatten().next();
    }
    if chains.len() == 1 {
        return chains[0].last().copied();
    }
    let mut terminal = visible.clone();
    terminal.sort_by_key(|link| std::cmp::Reverse(latest_updated_at(link)));
    terminal.first().copied()
}

/// `pullRequestHostOf` (`contracts/pullRequest.ts`), `None` for a missing provider kind.
pub fn pull_request_host_of(identity: &RepositoryIdentity, kind: Option<&str>) -> Option<String> {
    if kind == Some("forgejo") {
        if let Ok(remote) = url::Url::parse(&identity.locator.remote_url) {
            if remote.scheme() == "http" || remote.scheme() == "https" {
                return Some(url_host(&remote).to_lowercase());
            }
        }
    }
    let host = js_trim(identity.canonical_key.split('/').next().unwrap_or(""));
    if host.is_empty() {
        kind.map(str::to_owned)
    } else {
        Some(host.to_lowercase())
    }
}

/// `URL.host`: the hostname plus a non-default port.
fn url_host(url: &url::Url) -> String {
    let host = url.host_str().unwrap_or("");
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

/// `sourceControlRepositorySelector` (`shared/sourceControl.ts`).
pub fn source_control_repository_selector(identity: &RepositoryIdentity) -> Option<String> {
    if identity.provider.as_deref() == Some("azure-devops") {
        let display = identity.display_name.clone().unwrap_or_default();
        let segments: Vec<&str> = display.split('/').filter(|part| *part != "_git").collect();
        if let Some(name) = identity.name.as_deref().filter(|name| !name.is_empty()) {
            return Some(name.to_owned());
        }
        return segments.last().filter(|segment| !segment.is_empty()).map(|segment| segment.to_string());
    }
    if let Some(display) = identity.display_name.as_deref().filter(|display| !display.is_empty()) {
        return Some(display.to_owned());
    }
    match (identity.owner.as_deref(), identity.name.as_deref()) {
        (Some(owner), Some(name)) if !owner.is_empty() && !name.is_empty() => Some(format!("{owner}/{name}")),
        _ => None,
    }
}

/// `legacyLinkedPullRequestOf`: the single link a legacy `linkedPullRequest` consumer sees —
/// only links of the thread's own repository qualify.
pub fn legacy_linked_pull_request_of(
    links: &[ThreadPullRequestLink],
    project_id: &ProjectId,
    identity: Option<&RepositoryIdentity>,
) -> Option<ThreadLinkedPullRequest> {
    let identity = identity?;
    let host = pull_request_host_of(identity, identity.provider.as_deref())?;
    let repository = source_control_repository_selector(identity)?;
    let azure_key = (identity.provider.as_deref() == Some("azure-devops")).then(|| pr_keys::canonical_repository_key(&identity.canonical_key.to_lowercase()));
    let candidates: Vec<&ThreadPullRequestLink> = links
        .iter()
        .filter(|link| {
            if let Some(azure_key) = &azure_key {
                let key = pr_keys::legacy_thread_pull_request_key(&link.repository, link.number, &link.url, Some(&link.host));
                return pr_keys::canonical_repository_key(&format!("{}/{}", key.host, key.repository)) == *azure_key;
            }
            let parsed = pr_keys::parse_change_request_url(&link.url);
            if let Some(parsed) = parsed.as_ref().filter(|parsed| parsed.authority.is_some()) {
                if let Ok(remote) = url::Url::parse(&identity.locator.remote_url) {
                    if remote.scheme() == "http" || remote.scheme() == "https" {
                        return parsed.authority.as_deref() == Some(url_host(&remote).as_str()) && parsed.repository == repository.to_lowercase();
                    }
                }
                return parsed.host == host && parsed.repository == repository.to_lowercase();
            }
            link.host.to_lowercase() == host.to_lowercase() && link.repository.to_lowercase() == repository.to_lowercase()
        })
        .collect();
    let link = resolve_current_link(&candidates)?;
    Some(ThreadLinkedPullRequest {
        project_id: project_id.clone(),
        repository: if azure_key.is_none() { link.repository.clone() } else { repository },
        number: link.number,
        url: link.url.clone(),
    })
}

/// `legacyThreadPullRequestKey`.
pub fn legacy_thread_pull_request_key(linked: &ThreadLinkedPullRequest, fallback_host: Option<&str>) -> ThreadPullRequestKey {
    pr_keys::legacy_thread_pull_request_key(&linked.repository, linked.number, &linked.url, fallback_host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_date_times_like_the_shared_helper() {
        assert_eq!(
            compare_date_time_strings("2026-01-01T00:00:00.000Z", "2026-01-01T00:00:01.000Z"),
            Ordering::Less
        );
        assert_eq!(
            compare_date_time_strings("2026-02-30T00:00:00.000Z", "2026-01-01T00:00:00.000Z"),
            Ordering::Less
        );
        assert_eq!(compare_date_time_strings("b", "a"), Ordering::Greater);
        assert_eq!(
            compare_date_time_strings("2026-01-01T01:00:00+01:00", "2026-01-01T00:00:00.000Z"),
            Ordering::Equal
        );
    }

    #[test]
    fn the_ascii_locale_compare_agrees_with_the_general_one() {
        let samples = [
            "",
            "a",
            "A",
            "b",
            "B",
            "z",
            "Z",
            "0",
            "9",
            "10",
            "plan-2",
            "plan_2",
            "plan-10",
            "plan 1",
            "plan.1",
            "a-b",
            "a_b",
            "2026-01-01T00:00:00.000Z",
            "2026-01-01T00:00:00.001Z",
            "2026-01-01T00:00:00+00:00",
            "x$",
            "x~",
            "x|",
            "aB",
            "Ab",
            "settle:cmd:req",
            "async-answer:q",
            "\u{e9}vent",
            "x\"",
            "x\\",
            "x\u{1}",
        ];
        for left in samples {
            for right in samples {
                assert_eq!(
                    locale_compare(left, right),
                    zc_db::collate::locale_compare(left, right),
                    "{left:?} vs {right:?}"
                );
            }
        }
    }

    #[test]
    fn normalizes_project_paths() {
        assert_eq!(normalize_project_path_for_comparison(" /a/b/ "), "/a/b");
        assert_eq!(normalize_project_path_for_comparison("/"), "/");
        assert_eq!(normalize_project_path_for_comparison("C:/Repo/"), "c:\\repo");
        assert_eq!(normalize_project_path_for_comparison("C:/"), "c:\\");
    }

    #[test]
    fn validates_script_ids() {
        assert!(is_valid_script_id("lint"));
        assert!(is_valid_script_id("a-1"));
        assert!(!is_valid_script_id("-a"));
        assert!(!is_valid_script_id("Upper"));
        assert!(!is_valid_script_id(&"a".repeat(25)));
    }
}
