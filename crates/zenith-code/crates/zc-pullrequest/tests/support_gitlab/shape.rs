//! The neutral provider types as the TS objects look in JSON (camelCase keys, an absent optional
//! left out, `null` kept), for the golden comparisons against the TS providers.

#![allow(dead_code)]

use serde::Serialize;
use serde_json::{json, Map, Value};
use zc_pullrequest::provider::{
    ProviderChangeRequest, ProviderChangeRequestActivity, ProviderChangeRequestDetail, ProviderChangeRequestPage, ProviderChangeRequestPreview,
    ProviderChangeRequestSummary, ProviderDiffFileContents, ProviderDiffSlice, ProviderFileRevisions,
};
use zc_pullrequest::PullRequestProviderError;

pub fn to_json<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap()
}

/// A builder that leaves `None` out, like an object spread of an absent optional.
#[derive(Default)]
pub struct Obj(Map<String, Value>);

impl Obj {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(mut self, key: &str, value: impl Serialize) -> Self {
        self.0.insert(key.into(), to_json(&value));
        self
    }

    pub fn opt<T: Serialize>(mut self, key: &str, value: Option<T>) -> Self {
        if let Some(value) = value {
            self.0.insert(key.into(), to_json(&value));
        }
        self
    }

    pub fn done(self) -> Value {
        Value::Object(self.0)
    }
}

fn change_request_fields(change_request: &ProviderChangeRequest, with_dates: bool) -> Obj {
    let obj = Obj::new()
        .opt("stack", change_request.stack.as_ref())
        .set("number", change_request.number)
        .set("title", &change_request.title)
        .set("url", &change_request.url)
        .set("author", &change_request.author)
        .set("headBranch", &change_request.head_branch)
        .opt("headRepositoryNameWithOwner", change_request.head_repository_name_with_owner.as_ref())
        .set("baseBranch", &change_request.base_branch)
        .set("state", change_request.state)
        .set("isDraft", change_request.is_draft)
        .set("mergeability", change_request.mergeability)
        .set("additions", change_request.additions)
        .set("deletions", change_request.deletions)
        .set("createdAt", &change_request.created_at);
    let obj = if with_dates {
        obj.opt("closedAt", change_request.closed_at.as_ref())
            .opt("mergedAt", change_request.merged_at.as_ref())
    } else {
        obj
    };
    obj.set("updatedAt", &change_request.updated_at)
        .set("reviewRequestLogins", &change_request.review_request_logins)
        .set("labels", &change_request.labels)
        .opt("reviewDecision", change_request.review_decision.as_ref())
        .opt("checksState", change_request.checks_state.as_ref())
}

pub fn change_request(change_request: &ProviderChangeRequest) -> Value {
    change_request_fields(change_request, true).done()
}

pub fn page(page: &ProviderChangeRequestPage) -> Value {
    Obj::new()
        .set("items", page.items.iter().map(change_request).collect::<Vec<_>>())
        .set("truncated", page.truncated)
        .opt("cursorAdvance", page.cursor_advance)
        .set("continues", page.continues)
        .done()
}

pub fn detail(detail: &ProviderChangeRequestDetail) -> Value {
    change_request_fields(&detail.change_request, false)
        .set("closedAt", &detail.closed_at)
        .set("mergedAt", &detail.merged_at)
        .set("body", &detail.body)
        .set("changedFiles", detail.changed_files)
        .set("reviewers", &detail.reviewers)
        .set("checks", &detail.checks)
        .set("mergeCapabilities", &detail.merge_capabilities)
        .set("viewerPermissions", &detail.viewer_permissions)
        .opt("baseComparison", detail.base_comparison)
        .opt("behindBy", detail.behind_by)
        .opt("autoMergeEnabled", detail.auto_merge_enabled)
        .opt("autoMergeMethod", detail.auto_merge_method)
        .opt("workflowApprovalsRequired", detail.workflow_approvals_required)
        .done()
}

pub fn summary(summary: &ProviderChangeRequestSummary) -> Value {
    Obj::new()
        .set("number", summary.number)
        .set("title", &summary.title)
        .set("url", &summary.url)
        .set("headBranch", &summary.head_branch)
        .set("baseBranch", &summary.base_branch)
        .set("state", summary.state)
        .opt("isDraft", summary.is_draft)
        .opt("closedAt", summary.closed_at.as_ref())
        .opt("mergedAt", summary.merged_at.as_ref())
        .set("updatedAt", &summary.updated_at)
        .opt("author", summary.author.as_ref())
        .opt("additions", summary.additions)
        .opt("deletions", summary.deletions)
        .opt("changedFiles", summary.changed_files)
        .opt("reviewDecision", summary.review_decision.as_ref())
        .opt("checksState", summary.checks_state.as_ref())
        .opt("mergeability", summary.mergeability)
        .done()
}

pub fn preview(preview: &ProviderChangeRequestPreview) -> Value {
    json!({
        "number": preview.number,
        "title": preview.title,
        "url": preview.url,
        "author": preview.author,
        "state": preview.state,
        "isDraft": preview.is_draft,
        "createdAt": preview.created_at,
    })
}

pub fn activity(activity: &ProviderChangeRequestActivity) -> Value {
    Obj::new()
        .opt("author", activity.author.as_ref())
        .opt("reviewers", activity.reviewers.as_ref())
        .set("comments", &activity.comments)
        .set("commentCount", activity.comment_count)
        .set("commentsTruncated", activity.comments_truncated)
        .set("reviewThreads", &activity.review_threads)
        .set("commits", &activity.commits)
        .opt("reactions", activity.reactions.as_ref())
        .done()
}

pub fn diff(slice: &ProviderDiffSlice) -> Value {
    Obj::new()
        .set("patch", &slice.patch)
        .set("truncated", slice.truncated)
        .set("nextCursor", &slice.next_cursor)
        .opt("omittedFileStats", slice.omitted_file_stats.as_ref())
        .done()
}

pub fn file_contents(contents: &ProviderDiffFileContents) -> Value {
    json!({"oldContents": contents.old_contents, "newContents": contents.new_contents})
}

pub fn file_revisions(revisions: &ProviderFileRevisions) -> Value {
    Obj::new()
        .set(
            "revisions",
            revisions.revisions.iter().map(|(path, revision)| json!([path, revision])).collect::<Vec<_>>(),
        )
        .opt("complete", revisions.complete)
        .done()
}

/// `{"ok": …}` or `{"error": <PullRequestProviderError wire>}`.
pub fn outcome<T>(result: Result<T, PullRequestProviderError>, shape: impl FnOnce(&T) -> Value) -> Value {
    match result {
        Ok(value) => json!({"ok": shape(&value)}),
        Err(error) => json!({"error": error.to_wire()}),
    }
}

/// Keeps the first-level cause's name and message; deeper causes are compared by name (the TS
/// side nests Node/Effect internals there, e.g. a schema `Cause` or a `PlatformError`).
pub fn normalize(value: &mut Value, depth: usize) {
    if let Value::Object(map) = value {
        for (key, child) in map.iter_mut() {
            if key != "cause" {
                normalize(child, depth);
            }
        }
        if let Some(cause) = map.get_mut("cause") {
            // A failed `decodeJsonResult` keeps an Effect `Cause` (the whole schema AST); Rust
            // reports it as a `SchemaError`. Either is compared by that name alone.
            let schema_error = cause.get("_id").and_then(Value::as_str) == Some("Cause") || cause.get("name").and_then(Value::as_str) == Some("SchemaError");
            if schema_error {
                *cause = json!({"name": "SchemaError"});
            } else if depth >= 1 {
                let name = if cause.get("_id").and_then(Value::as_str) == Some("Cause") {
                    json!("SchemaError")
                } else {
                    cause.get("name").cloned().unwrap_or(Value::Null)
                };
                *cause = json!({"name": name});
            } else {
                normalize(cause, depth + 1);
            }
        }
    } else if let Value::Array(items) = value {
        for item in items {
            normalize(item, depth);
        }
    }
}
