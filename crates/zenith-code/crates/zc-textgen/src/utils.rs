//! `textGeneration/TextGenerationUtils.ts` and the branch helpers of `@t3tools/shared/git`:
//! section limits, the sanitizers every provider applies to generated text, and the
//! `TextGenerationError` constructors.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use zc_ports::TaggedError;

use crate::js::{first_line, len16, slice_head16, trim, trim_end};

/// The four operations, by the names TS uses in `TextGenerationError.operation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    GenerateCommitMessage,
    GeneratePrContent,
    GenerateBranchName,
    GenerateThreadTitle,
}

impl Operation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GenerateCommitMessage => "generateCommitMessage",
            Self::GeneratePrContent => "generatePrContent",
            Self::GenerateBranchName => "generateBranchName",
            Self::GenerateThreadTitle => "generateThreadTitle",
        }
    }
}

impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `new TextGenerationError({operation, detail})`: `{"_tag": "TextGenerationError", operation,
/// detail}` with the TS message `Text generation failed in <operation>: <detail>`. The optional
/// `cause` (a `Defect`) is not carried.
pub fn text_generation_error(operation: &str, detail: impl Into<String>) -> TaggedError {
    let detail = detail.into();
    TaggedError::new("TextGenerationError", format!("Text generation failed in {operation}: {detail}"))
        .with("operation", operation)
        .with("detail", detail)
}

/// The `detail` field of a `TextGenerationError`.
pub fn error_detail(error: &TaggedError) -> &str {
    error.fields.get("detail").and_then(Value::as_str).unwrap_or_default()
}

/// `cliLabel("codex")` → ``Codex CLI (`codex`)``.
fn cli_label(cli_name: &str) -> String {
    let mut chars = cli_name.chars();
    let capitalized = match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    };
    format!("{capitalized} CLI (`{cli_name}`)")
}

/// `normalizeCliError(cliName, operation, error, fallback)` for an I/O failure of the CLI
/// process: a missing executable names the CLI; anything else keeps its details private and
/// reports `fallback`.
pub fn normalize_cli_error(cli_name: &str, operation: &str, error: &std::io::Error, fallback: &str) -> TaggedError {
    let message = error.to_string();
    let lower = message.to_lowercase();
    if error.kind() == std::io::ErrorKind::NotFound
        || message.contains(&format!("Command not found: {cli_name}"))
        || lower.contains(&format!("spawn {cli_name}"))
        || lower.contains("enoent")
    {
        return text_generation_error(operation, format!("{} is required but not available on PATH.", cli_label(cli_name)));
    }
    text_generation_error(operation, fallback)
}

/// `limitSection(value, maxChars)`: cut at `maxChars` UTF-16 units and mark the cut.
pub fn limit_section(value: &str, max_chars: usize) -> String {
    if len16(value) <= max_chars {
        return value.to_owned();
    }
    format!("{}\n\n[truncated]", slice_head16(value, max_chars))
}

static TRAILING_PERIODS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[.]+$").expect("trailing periods"));

/// `sanitizeCommitSubject`: one line, no trailing period, at most 72 characters.
pub fn sanitize_commit_subject(raw: &str) -> String {
    let single_line = trim(first_line(trim(raw)));
    let without_trailing_period = TRAILING_PERIODS.replace(single_line, "");
    let without_trailing_period = trim(&without_trailing_period);
    if without_trailing_period.is_empty() {
        return "Update project files".to_owned();
    }
    if len16(without_trailing_period) <= 72 {
        return without_trailing_period.to_owned();
    }
    trim_end(slice_head16(without_trailing_period, 72)).to_owned()
}

/// `sanitizePrTitle`: one line, with a fallback.
pub fn sanitize_pr_title(raw: &str) -> String {
    let single_line = trim(first_line(trim(raw)));
    if !single_line.is_empty() {
        return single_line.to_owned();
    }
    "Update project changes".to_owned()
}

/// Prompts ask for under 40 characters; this cap only stops a runaway model.
const MAX_THREAD_TITLE_CHARS: usize = 120;

const JS_WS: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";
static EDGE_QUOTES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^['"`]+|['"`]+$"#).expect("edge quotes"));
static WHITESPACE_RUNS: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!("[{JS_WS}]+")).expect("whitespace runs"));

/// `Schema.decodeOption(Schema.fromJsonString(Schema.Struct({title: Schema.String})))`.
fn decode_json_thread_title(raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    value.as_object()?.get("title")?.as_str().map(str::to_owned)
}

/// `sanitizeThreadTitle`: unwrap a JSON-formatted title, keep one line without edge quotes,
/// collapse whitespace, cap runaway titles.
pub fn sanitize_thread_title(raw: &str) -> String {
    let decoded = decode_json_thread_title(raw);
    let title = decoded.as_deref().unwrap_or(raw);
    let line = trim(first_line(trim(title)));
    let unquoted = EDGE_QUOTES.replace_all(line, "");
    let normalized = WHITESPACE_RUNS.replace_all(trim(&unquoted), " ").into_owned();
    if trim(&normalized).is_empty() {
        return "New thread".to_owned();
    }
    if len16(&normalized) <= MAX_THREAD_TITLE_CHARS {
        return normalized;
    }
    format!("{}...", trim_end(slice_head16(&normalized, MAX_THREAD_TITLE_CHARS - 3)))
}

static EDGE_SEPARATORS_WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(r"^[./{JS_WS}_-]+|[./{JS_WS}_-]+$")).expect("edge separators"));
static NON_BRANCH_CHARS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9/_-]+").expect("branch chars"));
static SLASH_RUNS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/+").expect("slash runs"));
static DASH_RUNS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-+").expect("dash runs"));
static EDGE_SEPARATORS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[./_-]+|[./_-]+$").expect("edge separators"));
static TRAILING_SEPARATORS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[./_-]+$").expect("trailing separators"));

/// `sanitizeBranchFragment`: a lowercase `[a-z0-9/_-]` fragment of at most 64 characters, or
/// `update`.
pub fn sanitize_branch_fragment(raw: &str) -> String {
    let lowered = trim(raw).to_lowercase();
    let unquoted: String = lowered.chars().filter(|c| !matches!(c, '\'' | '"' | '`')).collect();
    let normalized = EDGE_SEPARATORS_WS.replace_all(&unquoted, "");
    let fragment = NON_BRANCH_CHARS.replace_all(&normalized, "-");
    let fragment = SLASH_RUNS.replace_all(&fragment, "/");
    let fragment = DASH_RUNS.replace_all(&fragment, "-");
    let fragment = EDGE_SEPARATORS.replace_all(&fragment, "");
    let fragment = slice_head16(&fragment, 64).to_owned();
    let fragment = TRAILING_SEPARATORS.replace_all(&fragment, "").into_owned();
    if fragment.is_empty() {
        "update".to_owned()
    } else {
        fragment
    }
}

/// `sanitizeFeatureBranchName`: a `feature/…` branch, keeping an existing namespace.
pub fn sanitize_feature_branch_name(raw: &str) -> String {
    let sanitized = sanitize_branch_fragment(raw);
    if sanitized.contains('/') {
        if sanitized.starts_with("feature/") {
            sanitized
        } else {
            format!("feature/{sanitized}")
        }
    } else {
        format!("feature/{sanitized}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_commit_subjects() {
        let subject = sanitize_commit_subject("  Add important change to the system with too much detail and a trailing period.\nsecondary line");
        assert!(len16(&subject) <= 72);
        assert!(!subject.ends_with('.'));
        assert_eq!(sanitize_commit_subject(" ... "), "Update project files");
        assert_eq!(sanitize_commit_subject("Fix it.."), "Fix it");
    }

    #[test]
    fn sanitizes_branches() {
        assert_eq!(sanitize_branch_fragment("  Feat/Session  "), "feat/session");
        assert_eq!(
            sanitize_feature_branch_name("fix/important-system-change"),
            "feature/fix/important-system-change"
        );
        assert_eq!(sanitize_feature_branch_name("feature/x"), "feature/x");
        assert_eq!(sanitize_feature_branch_name("Hello World"), "feature/hello-world");
        assert_eq!(sanitize_branch_fragment("..."), "update");
    }

    #[test]
    fn labels_missing_clis() {
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        let error = normalize_cli_error("codex", "generateBranchName", &missing, "Something went wrong");
        assert_eq!(error_detail(&error), "Codex CLI (`codex`) is required but not available on PATH.");
        let other = std::io::Error::other("request failed with access_token=secret-token");
        let error = normalize_cli_error("codex", "generateCommitMessage", &other, "Failed to generate a commit message");
        assert_eq!(error_detail(&error), "Failed to generate a commit message");
        assert!(!error.message.contains("secret-token"));
    }
}
