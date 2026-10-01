//! `azureDevOpsPullRequests.ts`: decoding `az repos pr show|list` JSON and building browser URLs.

use serde_json::{Map, Value};
use zc_contracts::ChangeRequestState;

use crate::records::{decode_list, decode_one, NormalizedChangeRequest};
use crate::util::{
    encode_uri_component, js_trim, optional_bool, optional_date, optional_field, optional_string, parse_url, positive_int, trim_optional_string,
    trimmed_non_empty, url_origin, SchemaMismatch,
};

fn normalize_ref_name(ref_name: &str) -> String {
    let trimmed = js_trim(ref_name);
    trimmed.strip_prefix("refs/heads/").unwrap_or(trimmed).to_owned()
}

fn normalize_state(status: &str) -> ChangeRequestState {
    match js_trim(status).to_lowercase().as_str() {
        "completed" => ChangeRequestState::Merged,
        "abandoned" => ChangeRequestState::Closed,
        _ => ChangeRequestState::Open,
    }
}

/// `azureDevOpsOrganizationBaseFromRestApiUrl`.
fn organization_base_from_rest_api_url(value: Option<&str>) -> Option<String> {
    let raw = trim_optional_string(value)?;
    let url = parse_url(&raw)?;
    let hostname = url.host_str().unwrap_or_default().to_lowercase();
    let segments: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    if !segments.iter().any(|s| s.eq_ignore_ascii_case("_apis")) {
        return None;
    }
    if hostname == "dev.azure.com" {
        return segments.first().map(|organization| format!("{}/{organization}", url_origin(&url)));
    }
    if hostname.ends_with(".visualstudio.com") {
        return Some(url_origin(&url));
    }
    None
}

/// Fields of [`azure_devops_pull_request_web_url`].
#[derive(Debug, Clone, Default)]
pub struct AzurePullRequestUrlInput<'a> {
    pub pull_request_id: i64,
    pub web_link: Option<&'a str>,
    pub repository_web_url: Option<&'a str>,
    pub rest_api_url: Option<&'a str>,
    pub project_name: Option<&'a str>,
    pub repository_name: Option<&'a str>,
}

/// `azureDevOpsPullRequestWebUrl`: the web link, else the repository web URL, else a URL built
/// from the REST URL's organization, else the REST URL itself.
pub fn azure_devops_pull_request_web_url(input: &AzurePullRequestUrlInput<'_>) -> String {
    if let Some(link) = trim_optional_string(input.web_link) {
        return link;
    }
    if let Some(repository) = trim_optional_string(input.repository_web_url) {
        return format!("{}/pullrequest/{}", repository.trim_end_matches('/'), input.pull_request_id);
    }
    let organization = organization_base_from_rest_api_url(input.rest_api_url);
    let project = trim_optional_string(input.project_name);
    let repository = trim_optional_string(input.repository_name);
    if let (Some(organization), Some(project), Some(repository)) = (organization, project, repository) {
        return format!(
            "{organization}/{}/_git/{}/pullrequest/{}",
            encode_uri_component(&project),
            encode_uri_component(&repository),
            input.pull_request_id
        );
    }
    trim_optional_string(input.rest_api_url).unwrap_or_default()
}

fn object<'a>(map: &'a Map<String, Value>, key: &str) -> Result<Option<&'a Map<String, Value>>, SchemaMismatch> {
    match optional_field(map, key, false)? {
        None => Ok(None),
        Some(Value::Object(inner)) => Ok(Some(inner)),
        Some(_) => Err(SchemaMismatch),
    }
}

/// `AzureDevOpsPullRequestSchema` + `normalizeAzureDevOpsPullRequestRecord`.
pub fn decode_azure_devops_pull_request(value: &Value) -> Option<NormalizedChangeRequest> {
    let map = value.as_object()?;
    let number = positive_int(map.get("pullRequestId")?)?;
    let title = trimmed_non_empty(map.get("title")?)?;
    let rest_url = optional_string(map, "url", false).ok()?;
    let repository = object(map, "repository").ok()?;
    let (repository_name, repository_web_url, project_name) = match repository {
        None => (None, None, None),
        Some(repository) => {
            let project = object(repository, "project").ok()?;
            (
                optional_string(repository, "name", false).ok()?,
                optional_string(repository, "webUrl", false).ok()?,
                match project {
                    None => None,
                    Some(project) => optional_string(project, "name", false).ok()?,
                },
            )
        }
    };
    let source_ref = trimmed_non_empty(map.get("sourceRefName")?)?;
    let target_ref = trimmed_non_empty(map.get("targetRefName")?)?;
    let status = map.get("status")?.as_str()?.to_owned();
    let is_draft = optional_bool(map, "isDraft").ok()?;
    let creation_date = optional_date(map, "creationDate").ok()?;
    let closed_date = optional_date(map, "closedDate").ok()?;
    let web_link = match object(map, "_links").ok()? {
        None => None,
        Some(links) => match object(links, "web").ok()? {
            None => None,
            Some(web) => Some(web.get("href")?.as_str()?.to_owned()),
        },
    };

    let state = normalize_state(&status);
    let terminal_at = closed_date.map(|date| date.to_iso_string());
    let url = azure_devops_pull_request_web_url(&AzurePullRequestUrlInput {
        pull_request_id: number,
        web_link: web_link.as_deref(),
        repository_web_url: repository_web_url.as_deref(),
        rest_api_url: rest_url.as_deref(),
        project_name: project_name.as_deref(),
        repository_name: repository_name.as_deref(),
    });
    Some(NormalizedChangeRequest {
        number,
        title,
        url,
        base_ref_name: normalize_ref_name(&target_ref),
        head_ref_name: normalize_ref_name(&source_ref),
        state,
        is_draft: (is_draft == Some(true)).then_some(true),
        closed_at: Some(if state == ChangeRequestState::Closed { terminal_at.clone() } else { None }),
        merged_at: Some(if state == ChangeRequestState::Merged { terminal_at } else { None }),
        updated_at: closed_date.or(creation_date),
        is_cross_repository: None,
        head_repository_name_with_owner: None,
        head_repository_owner_login: None,
    })
}

/// `decodeAzureDevOpsPullRequestListJson`.
pub fn decode_azure_devops_pull_request_list_json(raw: &str) -> Result<Vec<NormalizedChangeRequest>, String> {
    decode_list(raw, decode_azure_devops_pull_request)
}

/// `decodeAzureDevOpsPullRequestJson`.
pub fn decode_azure_devops_pull_request_json(raw: &str) -> Result<NormalizedChangeRequest, String> {
    decode_one(raw, decode_azure_devops_pull_request)
}
