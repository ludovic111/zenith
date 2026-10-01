//! `gitLabMergeRequests.ts`: decoding `glab mr view|list --output json`.

use serde_json::{Map, Value};
use zc_contracts::ChangeRequestState;

use crate::records::{decode_list, decode_one, NormalizedChangeRequest};
use crate::util::{
    js_trim, optional_bool, optional_date, optional_field, optional_string, positive_int, trim_optional_string, trimmed_non_empty, SchemaMismatch,
};

struct ProjectReference {
    path_with_namespace: Option<String>,
    path_with_namespace_camel: Option<String>,
    namespace: Option<(Option<String>, Option<String>, Option<String>)>,
}

fn decode_project(object: &Map<String, Value>, key: &str) -> Result<Option<ProjectReference>, SchemaMismatch> {
    let project = match optional_field(object, key, true)? {
        None => return Ok(None),
        Some(Value::Object(project)) => project,
        Some(_) => return Err(SchemaMismatch),
    };
    let namespace = match optional_field(project, "namespace", true)? {
        None => None,
        Some(Value::Object(namespace)) => Some((
            optional_string(namespace, "path", false)?,
            optional_string(namespace, "full_path", false)?,
            optional_string(namespace, "fullPath", false)?,
        )),
        Some(_) => return Err(SchemaMismatch),
    };
    Ok(Some(ProjectReference {
        path_with_namespace: optional_string(project, "path_with_namespace", false)?,
        path_with_namespace_camel: optional_string(project, "pathWithNamespace", false)?,
        namespace,
    }))
}

fn project_path_with_namespace(project: Option<&ProjectReference>) -> Option<String> {
    let project = project?;
    trim_optional_string(project.path_with_namespace.as_deref())
        .or_else(|| trim_optional_string(project.path_with_namespace_camel.as_deref()))
        .or_else(|| {
            let (path, full_path, full_path_camel) = project.namespace.as_ref()?;
            trim_optional_string(full_path.as_deref())
                .or_else(|| trim_optional_string(full_path_camel.as_deref()))
                .or_else(|| trim_optional_string(path.as_deref()))
        })
}

fn number_or_null(object: &Map<String, Value>, key: &str) -> Result<Option<f64>, SchemaMismatch> {
    match optional_field(object, key, true)? {
        None => Ok(None),
        Some(Value::Number(number)) => Ok(number.as_f64()),
        Some(_) => Err(SchemaMismatch),
    }
}

fn normalize_state(state: Option<&str>) -> ChangeRequestState {
    match state.map(|s| js_trim(s).to_lowercase()).as_deref() {
        Some("merged") => ChangeRequestState::Merged,
        Some("closed") => ChangeRequestState::Closed,
        _ => ChangeRequestState::Open,
    }
}

/// `GitLabMergeRequestSchema` + `normalizeGitLabMergeRequestRecord`.
pub fn decode_gitlab_merge_request(value: &Value) -> Option<NormalizedChangeRequest> {
    let object = value.as_object()?;
    let number = positive_int(object.get("iid")?)?;
    let title = trimmed_non_empty(object.get("title")?)?;
    let url = trimmed_non_empty(object.get("web_url")?)?;
    let head_ref_name = trimmed_non_empty(object.get("source_branch")?)?;
    let base_ref_name = trimmed_non_empty(object.get("target_branch")?)?;
    let state = optional_string(object, "state", true).ok()?;
    let draft = optional_bool(object, "draft").ok()?;
    let work_in_progress = optional_bool(object, "work_in_progress").ok()?;
    let closed_at = optional_string(object, "closed_at", true).ok()?;
    let merged_at = optional_string(object, "merged_at", true).ok()?;
    let updated_at = optional_date(object, "updated_at").ok()?;
    let source_project_id = number_or_null(object, "source_project_id").ok()?;
    let target_project_id = number_or_null(object, "target_project_id").ok()?;
    let source_project = decode_project(object, "source_project").ok()?;
    let target_project = decode_project(object, "target_project").ok()?;

    let source_path = project_path_with_namespace(source_project.as_ref());
    let target_path = project_path_with_namespace(target_project.as_ref());
    let is_cross_repository = match (source_project_id, target_project_id) {
        (Some(source), Some(target)) => Some(source != target),
        _ => match (&source_path, &target_path) {
            (Some(source), Some(target)) => Some(source.to_lowercase() != target.to_lowercase()),
            _ => None,
        },
    };
    let owner_login = source_path.as_deref().and_then(|path| trim_optional_string(path.split('/').next()));

    Some(NormalizedChangeRequest {
        number,
        title,
        url,
        base_ref_name,
        head_ref_name,
        state: normalize_state(state.as_deref()),
        is_draft: (draft == Some(true) || work_in_progress == Some(true)).then_some(true),
        closed_at: Some(closed_at),
        merged_at: Some(merged_at),
        updated_at,
        is_cross_repository,
        head_repository_name_with_owner: source_path.map(Some),
        head_repository_owner_login: owner_login.map(Some),
    })
}

/// `decodeGitLabMergeRequestListJson`.
pub fn decode_gitlab_merge_request_list_json(raw: &str) -> Result<Vec<NormalizedChangeRequest>, String> {
    decode_list(raw, decode_gitlab_merge_request)
}

/// `decodeGitLabMergeRequestJson`.
pub fn decode_gitlab_merge_request_json(raw: &str) -> Result<NormalizedChangeRequest, String> {
    decode_one(raw, decode_gitlab_merge_request)
}
