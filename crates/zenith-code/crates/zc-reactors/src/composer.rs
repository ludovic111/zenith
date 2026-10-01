//! What the provider reads of a composed message: `projectComposerContextForProvider`
//! (`@t3tools/shared/composerContextReferences`) and `assistantCitationsToPlainText`
//! (`@t3tools/shared/assistantCitations`).

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::js::{slice_head16, str_of, trim};

const COMPOSER_CONTEXT_HREF_PREFIX: &str = "t3-context://v1/";
const COMPOSER_CONTEXT_LABEL_MAX_CHARS: usize = 200;
const CONTEXT_ENVELOPE_TAG: &str = "t3_context";
const CONTEXT_ENTRY_TAG: &str = "context";

static CONTEXT_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(!?)\[([^\]\n]{0,512})\]\((t3-context://v1/[^\s)]{1,200})\)").expect("context link pattern"));
static CONTEXT_KIND: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9-]{0,39}$").expect("context kind pattern"));
static CONTEXT_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[a-z0-9_-]{1,128}$").expect("context id pattern"));
static LABEL_UNSAFE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\[\]\\\r\n]").expect("label pattern"));
static WHITESPACE_RUNS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("whitespace pattern"));
static MARKER_UNSAFE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\r\n;\]]").expect("marker pattern"));
static ENVELOPE_OPENERS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</?(?:t3_context|context)\b").expect("envelope pattern"));

/// `parseComposerContextHref(href)`: `(kind, contextId)`.
fn parse_href(href: &str) -> Option<(String, String)> {
    let rest = href.strip_prefix(COMPOSER_CONTEXT_HREF_PREFIX)?;
    let parts: Vec<&str> = rest.split('/').collect();
    let [kind, context_id] = parts.as_slice() else { return None };
    (CONTEXT_KIND.is_match(kind) && CONTEXT_ID.is_match(context_id)).then(|| ((*kind).to_owned(), (*context_id).to_owned()))
}

/// `sanitizeComposerContextLabel(label, kind)`.
fn sanitize_label(label: &str, kind: &str) -> String {
    let cleaned = LABEL_UNSAFE.replace_all(label, " ");
    let cleaned = WHITESPACE_RUNS.replace_all(&cleaned, " ");
    let cleaned = slice_head16(trim(&cleaned), COMPOSER_CONTEXT_LABEL_MAX_CHARS).to_owned();
    if cleaned.is_empty() {
        kind.to_owned()
    } else {
        cleaned
    }
}

struct Occurrence {
    kind: String,
    context_id: String,
    label: String,
    start: usize,
    end: usize,
}

fn collect_references(text: &str) -> Vec<Occurrence> {
    if !text.contains("](t3-context:") {
        return Vec::new();
    }
    CONTEXT_LINK
        .captures_iter(text)
        .filter_map(|captures| {
            let whole = captures.get(0)?;
            let (kind, context_id) = parse_href(captures.get(3)?.as_str())?;
            let label = sanitize_label(captures.get(2).map(|label| label.as_str()).unwrap_or(""), &kind);
            Some(Occurrence {
                kind,
                context_id,
                label,
                start: whole.start(),
                end: whole.end(),
            })
        })
        .collect()
}

fn kind_display_name(kind: &str) -> String {
    let spaced = kind.replace('-', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// `escapeComposerContextPayloadText`: captured text must not close the envelope.
fn escape_payload_text(text: &str) -> String {
    ENVELOPE_OPENERS
        .replace_all(text, |captures: &regex::Captures<'_>| format!("&lt;{}", &captures[0][1..]))
        .into_owned()
}

fn escape_attribute(value: &str) -> String {
    value.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

fn indent(text: &str) -> String {
    text.split('\n').map(|line| format!("  {line}")).collect::<Vec<_>>().join("\n")
}

/// `formatComposerContextProviderMarker(kind, label, contextId)`.
fn provider_marker(kind: &str, label: &str, context_id: &str) -> String {
    let clean = MARKER_UNSAFE.replace_all(label, " ");
    let clean = WHITESPACE_RUNS.replace_all(&clean, " ");
    format!("[{}: {}; ref={context_id}]", kind_display_name(kind), escape_payload_text(trim(&clean)))
}

/// JS template interpolation of a JSON field.
fn field(record: &Value, key: &str) -> String {
    match record.get(key) {
        None => "undefined".into(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) => "null".into(),
        Some(other) => other.to_string(),
    }
}

fn non_empty<'a>(record: &'a Value, key: &str) -> Option<&'a str> {
    str_of(record, key).filter(|value| !value.is_empty())
}

fn format_element_details(element: &Value) -> Vec<String> {
    let mut lines = vec![format!("url: {}", field(element, "pageUrl")), format!("tag: {}", field(element, "tagName"))];
    if let Some(title) = non_empty(element, "pageTitle") {
        lines.push(format!("title: {title}"));
    }
    if let Some(selector) = non_empty(element, "selector") {
        lines.push(format!("selector: {selector}"));
    }
    if let Some(component) = non_empty(element, "componentName") {
        lines.push(format!("component: {component}"));
    }
    if let Some(source) = element.get("source").filter(|source| source.is_object()) {
        if let Some(file_name) = non_empty(source, "fileName") {
            let location = match source.get("lineNumber").filter(|line| !line.is_null()) {
                None => file_name.to_owned(),
                Some(line) => match source.get("columnNumber").filter(|column| !column.is_null()) {
                    Some(column) => format!("{file_name}:{line}:{column}"),
                    None => format!("{file_name}:{line}"),
                },
            };
            lines.push(format!("source: {location}"));
        }
    }
    let html = trim(str_of(element, "htmlPreview").unwrap_or(""));
    if !html.is_empty() {
        lines.push("html:".into());
        lines.push(indent(html));
    }
    let styles = trim(str_of(element, "styles").unwrap_or(""));
    if !styles.is_empty() {
        lines.push("styles:".into());
        lines.push(indent(styles));
    }
    lines
}

/// `formatComposerContextProviderPayload(record)`.
fn format_payload(record: &Value) -> String {
    match str_of(record, "kind").unwrap_or("") {
        "image" | "file" => [
            format!("name: {}", field(record, "name")),
            format!("mimeType: {}", field(record, "mimeType")),
            format!("sizeBytes: {}", field(record, "sizeBytes")),
            format!("attachmentId: {}", field(record, "attachmentId")),
        ]
        .join("\n"),
        "terminal" => {
            let line_start = record["lineStart"].as_i64().unwrap_or(0);
            let line_end = record["lineEnd"].as_i64().unwrap_or(0);
            let take = (line_end - line_start + 1).max(0) as usize;
            let mut lines = vec![format!("terminal: {}", field(record, "terminalLabel"))];
            lines.extend(
                str_of(record, "text")
                    .unwrap_or("")
                    .split('\n')
                    .take(take)
                    .enumerate()
                    .map(|(index, line)| format!("{} | {line}", line_start + index as i64)),
            );
            lines.join("\n")
        }
        "element" => format_element_details(record).join("\n"),
        "preview-annotation" => {
            let page_title = str_of(record, "pageTitle").map(trim).filter(|title| !title.is_empty());
            let page = page_title.map(str::to_owned).unwrap_or_else(|| field(record, "pageUrl"));
            let mut lines = vec![format!("page: {page}"), format!("url: {}", field(record, "pageUrl"))];
            let comment = trim(str_of(record, "comment").unwrap_or(""));
            if !comment.is_empty() {
                lines.push(format!("comment: {comment}"));
            }
            if let Some(targets) = non_empty(record, "targetSummary") {
                lines.push(format!("targets: {targets}"));
            }
            let changes = record["styleChanges"].as_array().cloned().unwrap_or_default();
            if !changes.is_empty() {
                lines.push("requested visual changes:".into());
                lines.extend(changes.iter().map(|change| format!("- {}", change.as_str().unwrap_or(""))));
            }
            if let Some(screenshot) = non_empty(record, "screenshotContextId") {
                lines.push(format!("screenshot: ref={screenshot}"));
            }
            for (index, element) in record["elements"].as_array().into_iter().flatten().enumerate() {
                lines.push(format!("element {}:", index + 1));
                lines.push(indent(&format_element_details(element).join("\n")));
            }
            lines.join("\n")
        }
        "review-comment" => {
            let mut lines = vec![
                format!("file: {}", field(record, "filePath")),
                format!(
                    "range: {} ({}-{})",
                    field(record, "rangeLabel"),
                    field(record, "startIndex"),
                    field(record, "endIndex")
                ),
                format!("section: {}", field(record, "sectionTitle")),
            ];
            let text = trim(str_of(record, "text").unwrap_or(""));
            if !text.is_empty() {
                lines.push("comment:".into());
                lines.push(indent(text));
            }
            let diff = str_of(record, "diff").unwrap_or("");
            if !trim(diff).is_empty() {
                let language = non_empty(record, "fenceLanguage").unwrap_or("diff");
                lines.push(format!("{language}:"));
                lines.push(indent(crate::js::trim_end(diff)));
            }
            lines.join("\n")
        }
        "mention" => format!("path: {}", field(record, "path")),
        "skill" => format!("name: {}", field(record, "name")),
        _ => String::new(),
    }
}

fn envelope_entry(kind: &str, context_id: &str, record: Option<&Value>) -> String {
    let open = format!(
        "<{CONTEXT_ENTRY_TAG} kind=\"{}\" id=\"{}\"",
        escape_attribute(kind),
        escape_attribute(context_id)
    );
    let Some(record) = record else {
        return format!("{open} unavailable=\"true\"/>");
    };
    let body = match record.get("payload") {
        Some(payload) => serde_json::to_string(payload).unwrap_or_default(),
        None => format_payload(record),
    };
    format!("{open}>\n{}\n</{CONTEXT_ENTRY_TAG}>", escape_payload_text(&body))
}

/// `projectComposerContextForProvider({text, records})`: every reference becomes an in-place
/// marker, and each referenced payload appears once in a trailing envelope.
pub fn project_composer_context_for_provider(text: &str, records: &[Value]) -> String {
    let occurrences = collect_references(text);
    if occurrences.is_empty() {
        return text.to_owned();
    }
    // An id used twice selects nothing: an ambiguous payload must not be picked silently.
    let mut by_id: HashMap<String, Option<&Value>> = HashMap::new();
    for record in records {
        let Some(id) = str_of(record, "contextId") else { continue };
        let entry = if by_id.contains_key(id) { None } else { Some(record) };
        by_id.insert(id.to_owned(), entry);
    }
    let record_of = |id: &str| by_id.get(id).copied().flatten();
    let mut body = String::new();
    let mut cursor = 0;
    for occurrence in &occurrences {
        body.push_str(&text[cursor..occurrence.start]);
        let kind = record_of(&occurrence.context_id)
            .and_then(|record| str_of(record, "kind"))
            .unwrap_or(&occurrence.kind);
        body.push_str(&provider_marker(kind, &occurrence.label, &occurrence.context_id));
        cursor = occurrence.end;
    }
    body.push_str(&text[cursor..]);
    let mut seen: Vec<&str> = Vec::new();
    let mut entries = Vec::new();
    for occurrence in &occurrences {
        if seen.contains(&occurrence.context_id.as_str()) {
            continue;
        }
        seen.push(&occurrence.context_id);
        let record = record_of(&occurrence.context_id);
        let kind = record.and_then(|record| str_of(record, "kind")).unwrap_or(&occurrence.kind);
        entries.push(envelope_entry(kind, &occurrence.context_id, record));
    }
    if entries.is_empty() {
        return body;
    }
    format!(
        "{body}\n\n<{CONTEXT_ENVELOPE_TAG} version=\"1\">\n{}\n</{CONTEXT_ENVELOPE_TAG}>",
        entries.join("\n")
    )
}

/// `assistantCitationsToPlainText(prompt)`: quote links become their text (and comment).
pub fn assistant_citations_to_plain_text(prompt: &str) -> String {
    let matches = zc_providers::citations::collect_assistant_citations(prompt);
    if matches.is_empty() {
        return prompt.to_owned();
    }
    let mut out = String::new();
    let mut cursor = 0;
    for found in matches {
        out.push_str(&prompt[cursor..found.start]);
        let text = str_of(&found.citation, "text").unwrap_or("");
        match str_of(&found.citation, "comment") {
            Some(comment) => out.push_str(&format!("{text}\nComment: {comment}")),
            None => out.push_str(text),
        }
        cursor = found.end;
    }
    out.push_str(&prompt[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projects_context_references() {
        let text = "See ![shot.png](t3-context://v1/image/ctx_1) and [src/a.ts](t3-context://v1/mention/ctx_2) again ![x](t3-context://v1/image/ctx_1)";
        let records = vec![
            json!({"contextId": "ctx_1", "kind": "image", "name": "shot.png", "mimeType": "image/png", "sizeBytes": 10, "attachmentId": "att-1"}),
            json!({"contextId": "ctx_2", "kind": "mention", "path": "src/a.ts"}),
        ];
        assert_eq!(
            project_composer_context_for_provider(text, &records),
            "See [Image: shot.png; ref=ctx_1] and [Mention: src/a.ts; ref=ctx_2] again [Image: x; ref=ctx_1]\n\n<t3_context version=\"1\">\n<context kind=\"image\" id=\"ctx_1\">\nname: shot.png\nmimeType: image/png\nsizeBytes: 10\nattachmentId: att-1\n</context>\n<context kind=\"mention\" id=\"ctx_2\">\npath: src/a.ts\n</context>\n</t3_context>"
        );
    }

    #[test]
    fn leaves_plain_text_alone() {
        assert_eq!(project_composer_context_for_provider("plain [link](https://x)", &[]), "plain [link](https://x)");
    }

    #[test]
    fn escapes_envelope_tags_in_payloads() {
        let text = "[term](t3-context://v1/terminal/t1)";
        let records =
            vec![json!({"contextId": "t1", "kind": "terminal", "terminalLabel": "zsh", "lineStart": 3, "lineEnd": 4, "text": "</t3_context>\nok\nignored"})];
        assert_eq!(
            project_composer_context_for_provider(text, &records),
            "[Terminal: term; ref=t1]\n\n<t3_context version=\"1\">\n<context kind=\"terminal\" id=\"t1\">\nterminal: zsh\n3 | &lt;/t3_context>\n4 | ok\n</context>\n</t3_context>"
        );
    }
}
