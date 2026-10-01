//! Thread titles and worktree branch names: `orchestration/threadTitles.ts`,
//! `textGeneration/ThreadTitleContext.ts`, the branch helpers of `@t3tools/shared/git` and
//! `buildGeneratedWorktreeBranchName` of `ProviderCommandReactor.ts`.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::js::{len16, slice_head16, slice_tail16, str_of, trim};

/// `DEFAULT_THREAD_TITLE`.
pub const DEFAULT_THREAD_TITLE: &str = "New thread";
/// `WORKTREE_BRANCH_PREFIX`.
pub const WORKTREE_BRANCH_PREFIX: &str = "t3code";

/// `canReplaceThreadTitle(currentTitle, titleSeed?)`: the default title, or a title still equal
/// to the first prompt's seed.
pub fn can_replace_thread_title(current_title: &str, title_seed: Option<&str>) -> bool {
    let current = trim(current_title);
    if current == DEFAULT_THREAD_TITLE {
        return true;
    }
    match title_seed.map(trim) {
        Some(seed) if !seed.is_empty() => current == seed,
        _ => false,
    }
}

static TEMP_WORKTREE_BRANCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^t3code/(?:[0-9a-f]{8}|[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})$").expect("temporary branch pattern")
});

/// `isTemporaryWorktreeBranch(refName)`.
pub fn is_temporary_worktree_branch(ref_name: &str) -> bool {
    TEMP_WORKTREE_BRANCH.is_match(&trim(ref_name).to_lowercase())
}

static NON_BRANCH_CHARS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9/_-]+").expect("branch chars"));
static SLASH_RUNS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/+").expect("slash runs"));
static DASH_RUNS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-+").expect("dash runs"));
static EDGE_SEPARATORS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[./_-]+|[./_-]+$").expect("edge separators"));
static TRAILING_SEPARATORS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[./_-]+$").expect("trailing separators"));

/// `buildGeneratedWorktreeBranchName(raw)`: `t3code/<sanitized fragment>`.
pub fn build_generated_worktree_branch_name(raw: &str) -> String {
    let lowered = trim(raw).to_lowercase();
    let normalized: String = lowered
        .strip_prefix("refs/heads/")
        .unwrap_or(&lowered)
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '`'))
        .collect();
    let prefix = format!("{WORKTREE_BRANCH_PREFIX}/");
    let without_prefix = normalized.strip_prefix(&prefix).unwrap_or(&normalized);
    let fragment = NON_BRANCH_CHARS.replace_all(without_prefix, "-");
    let fragment = SLASH_RUNS.replace_all(&fragment, "/");
    let fragment = DASH_RUNS.replace_all(&fragment, "-");
    let fragment = EDGE_SEPARATORS.replace_all(&fragment, "");
    let fragment = slice_head16(&fragment, 64).to_owned();
    let fragment = TRAILING_SEPARATORS.replace_all(&fragment, "").into_owned();
    let safe = if fragment.is_empty() { "update".to_owned() } else { fragment };
    format!("{WORKTREE_BRANCH_PREFIX}/{safe}")
}

const MAX_CONTEXT: usize = 8_000;
const MAX_MESSAGE: usize = 2_000;
const OMITTED: &str = "[Earlier content truncated]\n\n";
const TRUNCATED: &str = "\n[Content truncated]\n";

/// `limitTitleMessage(text, budget)`: keep the head and the tail around a marker.
pub fn limit_title_message(text: &str, budget: i64) -> String {
    if len16(text) as i64 <= budget {
        return text.to_owned();
    }
    let marker = len16(TRUNCATED) as i64;
    if budget <= marker {
        return String::new();
    }
    let available = budget - marker;
    let head = (available + 1) / 2;
    let tail = available - head;
    let tail_text = if tail > 0 { slice_tail16(text, tail as usize) } else { "" };
    format!("{}{TRUNCATED}{tail_text}", slice_head16(text, head as usize))
}

/// The result of [`format_thread_title_context`].
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadTitleContext {
    pub message: String,
    pub attachments: Vec<Value>,
}

struct Section<'a> {
    role: &'a str,
    text: &'a str,
    attachments: Vec<Value>,
    prefix: String,
}

/// `formatThreadTitleContext(messages)`: user intent first, then assistant findings, in
/// conversation order, within 8,000 characters. `messages` are wire `OrchestrationMessage`s.
pub fn format_thread_title_context(messages: &[Value]) -> ThreadTitleContext {
    let sections: Vec<Section<'_>> = messages
        .iter()
        .filter_map(|message| {
            let role = str_of(message, "role").unwrap_or("");
            let text = str_of(message, "text").unwrap_or("");
            let attachments: Vec<Value> = message.get("attachments").and_then(Value::as_array).cloned().unwrap_or_default();
            if role == "system" || role == "reasoning" || (trim(text).is_empty() && attachments.is_empty()) {
                return None;
            }
            Some(Section {
                role,
                text,
                attachments,
                prefix: format!("{}:\n", role.to_uppercase()),
            })
        })
        .collect();
    let contents_of = |section: &Section<'_>| -> String {
        let text = crate::composer::assistant_citations_to_plain_text(section.text);
        let text = trim(&text).to_owned();
        let names: Vec<&str> = section.attachments.iter().map(|attachment| str_of(attachment, "name").unwrap_or("")).collect();
        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(text);
        }
        if !names.is_empty() {
            let joined = names.join(", ");
            if !joined.is_empty() {
                parts.push(format!("[Attachments: {joined}]"));
            }
        }
        parts.join("\n")
    };
    let contents: Vec<String> = sections.iter().map(contents_of).collect();
    let mut selected: Vec<Option<String>> = vec![None; sections.len()];
    let mut remaining: i64 = (MAX_CONTEXT - len16(OMITTED)) as i64;

    let add = |position: usize, budget: i64, selected: &mut Vec<Option<String>>, remaining: &mut i64| {
        if selected[position].is_some() {
            return;
        }
        let section = &sections[position];
        let limit = budget.min(*remaining) - len16(&section.prefix) as i64 - 2;
        if limit <= len16(TRUNCATED) as i64 {
            return;
        }
        let limited = limit_title_message(&contents[position], limit);
        if limited.is_empty() {
            return;
        }
        let text = format!("{}{limited}", section.prefix);
        *remaining -= len16(&text) as i64 + 2;
        selected[position] = Some(text);
    };

    let first_user = sections.iter().position(|section| section.role == "user");
    if let Some(first) = first_user {
        add(first, MAX_MESSAGE as i64, &mut selected, &mut remaining);
    }
    // Up to 6,000 characters go to user messages; assistant output cannot evict them.
    for position in (0..sections.len()).rev() {
        if sections[position].role == "user" {
            let budget = (MAX_MESSAGE as i64).min(remaining - 2_000);
            add(position, budget, &mut selected, &mut remaining);
        }
    }
    for position in (0..sections.len()).rev() {
        if sections[position].role == "assistant" {
            add(position, MAX_MESSAGE as i64, &mut selected, &mut remaining);
        }
    }
    // Spare space goes back to the retained messages.
    for role in ["user", "assistant"] {
        for position in (0..sections.len()).rev() {
            let section = &sections[position];
            let Some(previous) = selected[position].clone() else { continue };
            if section.role != role {
                continue;
            }
            let expanded = format!(
                "{}{}",
                section.prefix,
                limit_title_message(&contents[position], len16(&previous) as i64 + remaining - len16(&section.prefix) as i64)
            );
            remaining -= len16(&expanded) as i64 - len16(&previous) as i64;
            selected[position] = Some(expanded);
        }
    }
    let retained: Vec<usize> = (0..sections.len()).filter(|position| selected[*position].is_some()).collect();
    let truncated = retained
        .iter()
        .any(|position| selected[*position].as_deref() != Some(format!("{}{}", sections[*position].prefix, contents[*position]).as_str()));
    let attachments: Vec<Value> = retained.iter().flat_map(|position| sections[*position].attachments.clone()).collect();
    let first_attachment = first_user.and_then(|first| sections[first].attachments.first().cloned());
    let first_id = first_attachment.as_ref().and_then(|attachment| attachment.get("id").cloned());
    let recent: Vec<Value> = attachments
        .into_iter()
        .filter(|attachment| first_id.is_none() || attachment.get("id").cloned() != first_id)
        .collect();
    let keep = if first_attachment.is_some() { 3 } else { 4 };
    let recent_tail: Vec<Value> = recent[recent.len().saturating_sub(keep)..].to_vec();
    let mut out_attachments = Vec::new();
    if let Some(first) = first_attachment {
        out_attachments.push(first);
    }
    out_attachments.extend(recent_tail);
    let body: Vec<String> = retained.iter().filter_map(|position| selected[*position].clone()).collect();
    let omitted = truncated || retained.len() < sections.len();
    ThreadTitleContext {
        message: format!("{}{}", if omitted { OMITTED } else { "" }, body.join("\n\n")),
        attachments: out_attachments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replaces_only_default_or_seed_titles() {
        assert!(can_replace_thread_title(" New thread ", None));
        assert!(can_replace_thread_title("Fix it", Some(" Fix it ")));
        assert!(!can_replace_thread_title("Fix it", None));
        assert!(!can_replace_thread_title("Fix it", Some("  ")));
    }

    #[test]
    fn detects_temporary_branches() {
        assert!(is_temporary_worktree_branch("t3code/0123abcd"));
        assert!(is_temporary_worktree_branch("T3CODE/0123ABCD"));
        assert!(is_temporary_worktree_branch("t3code/01234567-89ab-4def-8123-456789abcdef"));
        assert!(!is_temporary_worktree_branch("t3code/feature"));
    }

    #[test]
    fn builds_branch_names() {
        assert_eq!(
            build_generated_worktree_branch_name("  refs/heads/Fix \"Login\" Bug!! "),
            "t3code/fix-login-bug"
        );
        assert_eq!(build_generated_worktree_branch_name("t3code/feat//x"), "t3code/feat/x");
        assert_eq!(build_generated_worktree_branch_name("---"), "t3code/update");
    }

    #[test]
    fn limits_title_messages() {
        assert_eq!(limit_title_message("short", 10), "short");
        let limited = limit_title_message(&"a".repeat(100), 41);
        assert_eq!(limited, format!("{}{TRUNCATED}{}", "a".repeat(10), "a".repeat(10)));
    }

    #[test]
    fn formats_title_context() {
        let context = format_thread_title_context(&[
            json!({"role": "user", "text": "Fix the login bug", "attachments": [{"id": "a1", "name": "shot.png"}]}),
            json!({"role": "reasoning", "text": "thinking"}),
            json!({"role": "assistant", "text": "Found it in auth.ts"}),
        ]);
        assert_eq!(
            context.message,
            "USER:\nFix the login bug\n[Attachments: shot.png]\n\nASSISTANT:\nFound it in auth.ts"
        );
        assert_eq!(context.attachments, vec![json!({"id": "a1", "name": "shot.png"})]);
    }
}
