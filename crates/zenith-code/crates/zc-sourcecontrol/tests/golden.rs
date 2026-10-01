//! Golden comparison against the TypeScript source control code: the same fake `gh`, `glab`,
//! `az` and `tea` executables (recorded outputs) answer both sides, the TS providers run over
//! the real `VcsProcess` (`golden/ts_oracle.mjs`, through node and the real effect/contracts
//! packages), and the wire JSON of every result and error must match. Discovery auth parsing,
//! probes and the pure helpers (transport-safe values, clone progress, `Retry-After`, forge
//! detection, Forgejo remotes and logins, Azure web URLs) are compared the same way.
//!
//! Needs `node` and `code/apps/server/node_modules` (symlinked from a checkout that has them);
//! without them the test prints why and passes vacuously. Nested error causes are compared by
//! name below the first level (see [`normalize`]).

#![allow(clippy::result_large_err, clippy::type_complexity)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use common::http::*;
use common::*;
use serde_json::{json, Map, Value};
use zc_contracts::{SourceControlProviderInfo, SourceControlRepositoryVisibility};
use zc_sourcecontrol::azure::{AzureDevOpsCli, AzureDevOpsSourceControlProvider};
use zc_sourcecontrol::bitbucket::{BitbucketApi, BitbucketApiConfig, BitbucketSourceControlProvider, StaticBitbucketSettings};
use zc_sourcecontrol::discovery::{probe_source_control_provider, AuthProbeInput, CliDiscoverySpec, DiscoverySpec, RefinementInput};
use zc_sourcecontrol::forgejo::{ForgejoCli, ForgejoEnvironment, ForgejoSourceControlProvider};
use zc_sourcecontrol::github::{GitHubCli, GitHubSourceControlProvider};
use zc_sourcecontrol::gitlab::{GitLabCli, GitLabSourceControlProvider};
use zc_sourcecontrol::provider::*;
use zc_sourcecontrol::util::system_clock;

fn server_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../code/apps/server")
}

fn oracle_available() -> Result<(), String> {
    let server = server_dir();
    if !server.join("node_modules/effect").exists() {
        return Err(format!("{} has no node_modules", server.display()));
    }
    match Command::new("node").arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
        _ => Err("node is not installed".into()),
    }
}

fn run_oracle(cases: &[Value], bin: &Path, home: &Path, bitbucket_base: &str) -> Map<String, Value> {
    let script = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/ts_oracle.mjs")).unwrap();
    let server = std::fs::canonicalize(server_dir()).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let mut child = Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", &script])
        .current_dir(&server)
        .env("ZC_SERVER_SRC", server.join("src"))
        .env("PATH", path)
        .env("HOME", home)
        .env_remove("GH_HOST")
        .env_remove("XDG_DATA_HOME")
        .env("T3CODE_BITBUCKET_API_BASE_URL", bitbucket_base)
        .env("T3CODE_BITBUCKET_EMAIL", "someone@example.test")
        .env("T3CODE_BITBUCKET_API_TOKEN", "test-token")
        .env_remove("T3CODE_BITBUCKET_ACCESS_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(json!({"cases": cases}).to_string().as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "oracle failed: {}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Keeps the first-level cause's name and message; deeper causes are compared by name (the TS
/// side nests Node/Effect internals there, e.g. a schema `Cause` or a `PlatformError`).
fn normalize(value: &mut Value, depth: usize) {
    if let Value::Object(map) = value {
        if let Some(cause) = map.get_mut("cause") {
            if depth >= 1 {
                // A failed `decodeJsonResult` keeps an Effect `Cause` (with the whole schema AST)
                // as the cause; Rust reports it as a `SchemaError`.
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
    }
}

fn context(value: &Value) -> Option<SourceControlProviderContext> {
    let context = value.get("context")?;
    Some(SourceControlProviderContext {
        provider: serde_json::from_value::<SourceControlProviderInfo>(context["provider"].clone()).unwrap(),
        remote_name: context["remoteName"].as_str().unwrap().into(),
        remote_url: context["remoteUrl"].as_str().unwrap().into(),
        requested_host: context.get("requestedHost").and_then(Value::as_str).map(Into::into),
    })
}

fn str_field(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_owned()
}

fn state(value: &Value) -> ChangeRequestStateFilter {
    match value["state"].as_str().unwrap() {
        "open" => ChangeRequestStateFilter::Open,
        "closed" => ChangeRequestStateFilter::Closed,
        "merged" => ChangeRequestStateFilter::Merged,
        _ => ChangeRequestStateFilter::All,
    }
}

fn outcome<T: serde::Serialize>(result: Result<T, zc_sourcecontrol::SourceControlProviderError>) -> Value {
    match result {
        Ok(value) => json!({"ok": serde_json::to_value(value).unwrap()}),
        Err(error) => json!({"error": serde_json::to_value(error).unwrap()}),
    }
}

struct Rust {
    process: zc_core::vcs_process::VcsProcess,
    home: PathBuf,
    bitbucket_base: String,
    _worktrees: Tmp,
}

impl Rust {
    /// A fresh provider per case, like the oracle (which builds its layers per case), so a
    /// rate-limit pause of one case does not leak into the next.
    fn provider(&self, name: &str) -> Arc<dyn SourceControlProvider> {
        let process = self.process.clone();
        match name {
            "bitbucket" => Arc::new(BitbucketSourceControlProvider::new(self.bitbucket())),
            "github" => Arc::new(GitHubSourceControlProvider::new(GitHubCli::new(process, system_clock()))),
            "gitlab" => Arc::new(GitLabSourceControlProvider::new(GitLabCli::new(process))),
            "azure-devops" => Arc::new(AzureDevOpsSourceControlProvider::new(AzureDevOpsCli::new(process))),
            _ => Arc::new(ForgejoSourceControlProvider::new(
                ForgejoCli::new(
                    process.clone(),
                    ForgejoEnvironment {
                        home: self.home.clone(),
                        data_home: None,
                        app_data: None,
                        ..ForgejoEnvironment::from_process()
                    },
                    system_clock(),
                ),
                process,
            )),
        }
    }

    fn bitbucket(&self) -> BitbucketApi {
        BitbucketApi::new(
            BitbucketApiConfig {
                base_url: self.bitbucket_base.clone(),
                access_token: None,
                email: Some("someone@example.test".into()),
                api_token: Some("test-token".into()),
            },
            Arc::new(StaticBitbucketSettings(None)),
            system_clock(),
            zc_vcs::GitVcsDriver::new(&self._worktrees.path),
            zc_vcs::registry::VcsDriverRegistry::new(
                zc_vcs::registry::VcsProjectConfig::new(),
                Arc::new(zc_vcs::vcs_driver::GitVcsProcessDriver::new(zc_core::vcs_process::VcsProcess::default())),
            ),
        )
    }

    fn spec(name: &str) -> CliDiscoverySpec {
        match name {
            "github" => zc_sourcecontrol::github::provider::discovery(),
            "gitlab" => zc_sourcecontrol::gitlab::provider::discovery(),
            "azure-devops" => zc_sourcecontrol::azure::provider::discovery(),
            _ => zc_sourcecontrol::forgejo::provider::discovery(),
        }
    }

    async fn run(&self, case: &Value) -> Value {
        let input = &case["input"];
        match case["op"].as_str().unwrap() {
            "provider" => {
                let provider = self.provider(case["kind"].as_str().unwrap());
                let cwd = str_field(input, "cwd");
                match case["method"].as_str().unwrap() {
                    "getChangeRequest" => outcome(
                        provider
                            .get_change_request(GetChangeRequestInput {
                                cwd,
                                context: context(input),
                                reference: str_field(input, "reference"),
                            })
                            .await,
                    ),
                    "listChangeRequests" => outcome(
                        provider
                            .list_change_requests(ListChangeRequestsInput {
                                cwd,
                                context: context(input),
                                source: None,
                                head_selector: str_field(input, "headSelector"),
                                state: state(input),
                                limit: input["limit"].as_u64().map(|l| l as u32),
                            })
                            .await,
                    ),
                    "getRepositoryCloneUrls" => outcome(
                        provider
                            .get_repository_clone_urls(RepositoryCloneUrlsInput {
                                cwd,
                                context: context(input),
                                repository: str_field(input, "repository"),
                            })
                            .await,
                    ),
                    "createRepository" => outcome(
                        provider
                            .create_repository(CreateRepositoryInput {
                                cwd,
                                repository: str_field(input, "repository"),
                                visibility: serde_json::from_value::<SourceControlRepositoryVisibility>(input["visibility"].clone()).unwrap(),
                            })
                            .await,
                    ),
                    "getDefaultBranch" => outcome(provider.get_default_branch(DefaultBranchInput { cwd, context: context(input) }).await),
                    "checkoutChangeRequest" => outcome(
                        provider
                            .checkout_change_request(CheckoutChangeRequestInput {
                                cwd,
                                context: context(input),
                                reference: str_field(input, "reference"),
                                force: input["force"].as_bool().unwrap_or(false),
                            })
                            .await
                            .map(|()| Value::Null),
                    ),
                    other => panic!("unknown method {other}"),
                }
            }
            "parseAuth" => {
                let auth = (Self::spec(case["kind"].as_str().unwrap()).parse_auth)(&AuthProbeInput {
                    stdout: str_field(input, "stdout"),
                    stderr: str_field(input, "stderr"),
                    exit_code: input["exitCode"].as_i64().unwrap() as i32,
                });
                json!({"ok": auth})
            }
            "refine" => {
                let spec = Self::spec(case["kind"].as_str().unwrap());
                let refined = spec.refine_unknown_remote.and_then(|refine| {
                    refine(&RefinementInput {
                        cwd: str_field(input, "cwd"),
                        context: context(input).unwrap(),
                        auth: AuthProbeInput {
                            stdout: str_field(&input["auth"], "stdout"),
                            stderr: str_field(&input["auth"], "stderr"),
                            exit_code: input["auth"]["exitCode"].as_i64().unwrap() as i32,
                        },
                    })
                });
                json!({"ok": refined})
            }
            "probeBitbucket" => {
                let spec = DiscoverySpec::Api(zc_sourcecontrol::bitbucket::provider::discovery(self.bitbucket()));
                json!({"ok": probe_source_control_provider(&spec, &self.process, case["cwd"].as_str().unwrap()).await})
            }
            "probe" => {
                let spec = DiscoverySpec::Cli(Self::spec(case["kind"].as_str().unwrap()));
                json!({"ok": probe_source_control_provider(&spec, &self.process, case["cwd"].as_str().unwrap()).await})
            }
            "pure" => json!({"ok": pure(case["fn"].as_str().unwrap(), case["args"].as_array().unwrap())}),
            other => panic!("unknown op {other}"),
        }
    }
}

fn pure(name: &str, args: &[Value]) -> Value {
    use zc_sourcecontrol::forgejo::cli::{match_forgejo_login, parse_forgejo_logins, parse_forgejo_remote};
    let text = |index: usize| args[index].as_str().unwrap_or_default().to_owned();
    match name {
        "transportSafe" => json!(transport_safe_source_control_error_value(&text(0))),
        "cloneProgress" => match zc_sourcecontrol::clone_progress::parse_git_clone_progress_line(&text(0)) {
            Some(line) => json!({"stage": line.stage, "percent": line.percent, "detail": line.detail}),
            None => Value::Null,
        },
        "retryAt" => json!(zc_sourcecontrol::rate_limit::retry_at_from_header(args[0].as_str(), args[1].as_i64().unwrap())),
        "detectProvider" => json!(zc_sourcecontrol::registry::detect_provider_from_remote_url(&text(0))),
        "forgejoRemote" => match parse_forgejo_remote(&text(0)) {
            Some(remote) => json!({"host": remote.host, "hostname": remote.hostname, "ssh": remote.ssh, "path": remote.path}),
            None => Value::Null,
        },
        "forgejoLogin" => {
            let logins = parse_forgejo_logins(&args[0].to_string());
            let remote = parse_forgejo_remote(&text(1)).unwrap();
            json!(match_forgejo_login(&logins, &remote, args[2].as_str(), args[3].as_bool().unwrap_or(false)).map(|l| l.name))
        }
        "azureWebUrl" => {
            let input = &args[0];
            let get = |key: &str| input.get(key).and_then(Value::as_str);
            json!(zc_sourcecontrol::azure::pull_requests::azure_devops_pull_request_web_url(
                &zc_sourcecontrol::azure::pull_requests::AzurePullRequestUrlInput {
                    pull_request_id: input["pullRequestId"].as_i64().unwrap(),
                    web_link: get("webLink"),
                    repository_web_url: get("repositoryWebUrl"),
                    rest_api_url: get("restApiUrl"),
                    project_name: get("projectName"),
                    repository_name: get("repositoryName"),
                }
            ))
        }
        "ownerRef" => match parse_source_control_owner_ref(&text(0)) {
            Some(selector) => json!({"owner": selector.owner, "refName": selector.ref_name}),
            None => Value::Null,
        },
        other => panic!("unknown pure fn {other}"),
    }
}

const QUOTA_ARGS: &str =
    "api rate_limit --hostname github.com --jq .resources.graphql | {data:{rateLimit:{cost:1,limit:.limit,remaining:.remaining,resetAt:(.reset|todateiso8601)}}}";
const PR_VIEW: &str =
    "--json number,title,url,baseRefName,headRefName,state,isDraft,mergedAt,closedAt,updatedAt,isCrossRepository,headRepository,headRepositoryOwner";
const PR_LIST: &str = "--json number,title,url,baseRefName,headRefName,state,isDraft,mergedAt,closedAt,isCrossRepository,headRepository,headRepositoryOwner";

fn record_fake_outputs(clis: &FakeClis) {
    clis.respond(
        "gh",
        QUOTA_ARGS,
        r#"{"data":{"rateLimit":{"cost":1,"limit":5000,"remaining":4999,"resetAt":"2099-01-01T00:00:00Z"}}}"#,
        "",
        0,
    );
    clis.respond(
        "gh",
        &format!("pr view 42 {PR_VIEW}"),
        r#"{"number":42,"title":"Add PR thread creation","url":"https://github.com/octocat/demo/pull/42","baseRefName":"main","headRefName":"feature/pr-threads","state":"OPEN","isDraft":true,"mergedAt":null,"updatedAt":"2026-08-24T12:34:56Z","isCrossRepository":true,"headRepository":{"nameWithOwner":"someone/demo"},"headRepositoryOwner":{"login":"someone"}}"#,
        "",
        0,
    );
    clis.respond(
        "gh",
        &format!("pr view 43 {PR_VIEW}"),
        r#"{"number":43,"title":"  Trimmed  \n","url":" https://github.com/octocat/demo/pull/43 ","baseRefName":" main ","headRefName":"\tfeature/x\t","state":"CLOSED","mergedAt":"2026-01-01T00:00:00Z","closedAt":"2026-01-01T00:00:00Z","headRepository":{"id":"R_1","name":"demo"},"headRepositoryOwner":{"login":" octocat "}}"#,
        "",
        0,
    );
    clis.respond(
        "gh",
        &format!("pr view 44 {PR_VIEW}"),
        "",
        "GraphQL: Could not resolve to a PullRequest with the number of 44. (repository.pullRequest)",
        1,
    );
    clis.respond(
        "gh",
        &format!("pr view 45 {PR_VIEW}"),
        "",
        "To get started with GitHub CLI, please run:  gh auth login",
        1,
    );
    clis.respond("gh", &format!("pr view 46 {PR_VIEW}"), "not json", "", 0);
    clis.respond("gh", &format!("pr view 47 {PR_VIEW}"), "", "HTTP 429: API rate limit exceeded", 1);
    clis.respond(
        "gh",
        &format!("pr list --head feature/list --state open --limit 1 {PR_LIST}"),
        r#"[{"number":0,"title":"invalid","url":"u","baseRefName":"main","headRefName":"x"},{"number":7,"title":"Old gh","url":"https://github.com/octocat/demo/pull/7","baseRefName":"main","headRefName":"feature/list","state":"OPEN","isCrossRepository":false,"headRepository":{"id":"R","name":"demo"},"headRepositoryOwner":{"login":"octocat"}},{"number":8,"title":"Bad date","url":"u","baseRefName":"main","headRefName":"x","updatedAt":"yesterday"}]"#,
        "",
        0,
    );
    clis.respond(
        "gh",
        &format!("pr list --head feature/all --state all --limit 20 {PR_VIEW}"),
        r#"[{"number":9,"title":"Merged","url":"https://github.com/octocat/demo/pull/9","baseRefName":"main","headRefName":"feature/all","state":"MERGED","mergedAt":"2026-01-02T00:00:00Z","updatedAt":"2026-01-02T00:00:00.123Z"}]"#,
        "",
        0,
    );
    clis.respond(
        "gh",
        "repo view octocat/demo --json nameWithOwner,url,sshUrl",
        r#"{"nameWithOwner":" octocat/demo ","url":"https://github.com/octocat/demo","sshUrl":"git@github.com:octocat/demo.git"}"#,
        "",
        0,
    );
    clis.respond(
        "gh",
        "repo view octocat/broken --json nameWithOwner,url,sshUrl",
        r#"{"nameWithOwner":"","url":"x"}"#,
        "",
        0,
    );
    clis.respond(
        "gh",
        "repo create octocat/new --private",
        "✓ Created repository octocat/new on github.com\nhttps://github.com/octocat/new\n",
        "",
        0,
    );
    clis.respond("gh", "repo view --json defaultBranchRef --jq .defaultBranchRef.name", "trunk\n", "", 0);
    clis.respond(
        "gh",
        "--version",
        "gh version 2.83.0 (2026-01-01)\nhttps://github.com/cli/cli/releases/latest\n",
        "",
        0,
    );
    clis.respond(
        "gh",
        "auth status --json hosts",
        "",
        "unknown flag: --json\n\nUsage:  gh auth status [flags]\n",
        1,
    );

    clis.respond(
        "glab",
        "mr view 42 --output json",
        r#"{"iid":42,"title":"Add MR","web_url":"https://gitlab.com/group/demo/-/merge_requests/42","target_branch":"main","source_branch":"feature/mr","state":"closed","closed_at":"2026-08-23T10:00:00Z","source_project_id":101,"target_project_id":100,"source_project":{"path_with_namespace":"someone/demo"},"updated_at":"2026-08-23T10:00:00.000+02:00","work_in_progress":true}"#,
        "",
        0,
    );
    clis.respond(
        "glab",
        "mr view 43 --output json",
        r#"{"iid":43,"title":"Namespaced","web_url":"https://gitlab.com/g/p/-/merge_requests/43","target_branch":"main","source_branch":"x","source_project":{"namespace":{"full_path":"Group/Sub"}},"target_project":{"pathWithNamespace":"group/sub"}}"#,
        "",
        0,
    );
    clis.respond("glab", "mr view 44 --output json", "", "404 merge request not found", 1);
    clis.respond(
        "glab",
        "mr list --source-branch feature/all --all --per-page 20 --output json",
        r#"[{"iid":0,"title":"x","web_url":"u","target_branch":"m","source_branch":"s"},{"iid":5,"title":" MR ","web_url":"https://gitlab.com/g/p/-/merge_requests/5","target_branch":"main","source_branch":"feature/all","state":"merged","merged_at":"2026-08-23T11:00:00Z","draft":false}]"#,
        "",
        0,
    );
    clis.respond(
        "glab",
        "api projects/group%2Fdemo",
        r#"{"path_with_namespace":"group/demo","web_url":"https://gitlab.com/group/demo","http_url_to_repo":"https://gitlab.com/group/demo.git","ssh_url_to_repo":"git@gitlab.com:group/demo.git"}"#,
        "",
        0,
    );
    clis.respond("glab", "api projects/:fullpath", r#"{"default_branch":" develop "}"#, "", 0);
    clis.respond("glab", "--version", "glab 1.50.0 (2026-01-01)\n", "", 0);
    clis.respond(
        "glab",
        "auth status",
        "gitlab.com\n  ✓ Logged in to gitlab.com as gitlab-user (/home/x/.config/glab-cli/config.yml)\n  ✓ Token: **************\n",
        "",
        0,
    );

    clis.respond(
        "az",
        "repos pr show --detect true --id 42 --only-show-errors --output json",
        r#"{"pullRequestId":42,"title":"Add Azure","sourceRefName":"refs/heads/feature/x","targetRefName":"refs/heads/main","status":"completed","creationDate":"2026-01-02T00:00:00.000Z","closedDate":"2026-01-03T00:00:00Z","isDraft":true,"_links":{"web":{"href":"https://dev.azure.com/acme/project/_git/repo/pullrequest/42"}}}"#,
        "",
        0,
    );
    clis.respond(
        "az",
        "repos pr show --detect true --id 863 --only-show-errors --output json",
        r#"{"pullRequestId":863,"title":"Fix link","url":"https://dev.azure.com/example-org/a8fe/_apis/git/repositories/1610/pullRequests/863","repository":{"name":"CV engine","project":{"name":"CV engine"}},"sourceRefName":"refs/heads/feature/link","targetRefName":"refs/heads/main","status":"abandoned","closedDate":"2026-01-04T00:00:00Z"}"#,
        "",
        0,
    );
    clis.respond("az", "repos pr show --detect true --id 44 --only-show-errors --output json", "not-json", "", 0);
    clis.respond(
        "az",
        "repos pr show --detect true --id 45 --only-show-errors --output json",
        "",
        "ERROR: The pull request does not exist.",
        1,
    );
    clis.respond(
        "az",
        "repos pr list --detect true --source-branch feature/merged --status completed --top 10 --only-show-errors --output json",
        r#"[{"pullRequestId":7,"title":"Merged work","sourceRefName":"refs/heads/feature/merged","targetRefName":"refs/heads/main","status":"completed","closedDate":"2026-01-03T00:00:00.000Z","repository":{"webUrl":"https://dev.azure.com/acme/project/_git/repo/"}}]"#,
        "",
        0,
    );
    clis.respond(
        "az",
        "repos show --detect true --repository repo --only-show-errors --output json",
        r#"{"name":"repo","webUrl":"https://dev.azure.com/acme/project/_git/repo","remoteUrl":"https://dev.azure.com/acme/project/_git/repo","sshUrl":"git@ssh.dev.azure.com:v3/acme/project/repo","project":{"name":"project"},"defaultBranch":"refs/heads/main"}"#,
        "",
        0,
    );
    clis.respond(
        "az",
        "repos show --detect true --only-show-errors --output json",
        r#"{"name":"repo","webUrl":"w","remoteUrl":"r","sshUrl":"s","defaultBranch":"refs/heads/develop"}"#,
        "",
        0,
    );
    clis.respond(
        "az",
        "--version",
        "azure-cli                         2.70.0\n\ncore                              2.70.0\n",
        "",
        0,
    );
    clis.respond("az", "account show --query user.name -o tsv", "", "Please run 'az login' to setup account.", 1);

    let logins = r#"[{"name":"work","url":"https://forgejo.example.test","ssh_host":"forgejo.example.test","user":"maria","default":"true","valid":"true"}]"#;
    clis.respond("tea", "login list --output json", logins, "", 0);
    clis.respond("tea", "login status --output json", logins, "", 0);
    clis.respond("tea", "--version", "\u{1b}[1mtea version 0.16.0\u{1b}[0m\n", "", 0);
    clis.respond(
        "tea",
        "api --include --login work --repo maria/project --method GET https://forgejo.example.test/api/v1/repos/maria/project/pulls/12",
        r#"{"number":12,"title":"[WIP] Forgejo PR","html_url":"https://forgejo.example.test/maria/project/pulls/12","state":"open","merged":false,"base":{"ref":"main","sha":"a","repo":{"full_name":"maria/project","owner":{"login":"maria"}}},"head":{"ref":"feature/f","sha":"b","repo":{"full_name":"fork/project","owner":{"login":"fork"}}},"updated_at":"2026-02-01T00:00:00Z"}"#,
        "HTTP/1.1 200 OK\n",
        0,
    );
    clis.respond(
        "tea",
        "api --include --login work --repo maria/project --method GET https://forgejo.example.test/api/v1/repos/maria/project/pulls/13",
        "{}",
        "HTTP/1.1 404 Not Found\n",
        0,
    );
    clis.respond(
        "tea",
        "api --include --login work --repo maria/project --method GET https://forgejo.example.test/api/v1/repos/maria/project",
        r#"{"full_name":"maria/project","clone_url":"https://forgejo.example.test/maria/project.git","ssh_url":"git@forgejo.example.test:maria/project.git","default_branch":"main"}"#,
        "HTTP/1.1 200 OK\n",
        0,
    );
}

fn cases(cwd: &str) -> Vec<Value> {
    let forgejo_context = json!({"provider": {"kind": "forgejo", "name": "Forgejo", "baseUrl": "https://forgejo.example.test"}, "remoteName": "origin", "remoteUrl": "git@forgejo.example.test:maria/project.git"});
    let provider = |id: &str, kind: &str, method: &str, input: Value| json!({"id": id, "op": "provider", "kind": kind, "method": method, "input": input});
    let mut cases = vec![
        provider("gh-view", "github", "getChangeRequest", json!({"cwd": cwd, "reference": "42"})),
        provider("gh-view-trimmed", "github", "getChangeRequest", json!({"cwd": cwd, "reference": "43"})),
        provider("gh-not-found", "github", "getChangeRequest", json!({"cwd": cwd, "reference": "44"})),
        provider("gh-auth", "github", "getChangeRequest", json!({"cwd": cwd, "reference": "45"})),
        provider("gh-decode", "github", "getChangeRequest", json!({"cwd": cwd, "reference": "46"})),
        provider("gh-rate-limit", "github", "getChangeRequest", json!({"cwd": cwd, "reference": "47"})),
        provider(
            "gh-list-open",
            "github",
            "listChangeRequests",
            json!({"cwd": cwd, "headSelector": "feature/list", "state": "open"}),
        ),
        provider(
            "gh-list-all",
            "github",
            "listChangeRequests",
            json!({"cwd": cwd, "headSelector": "feature/all", "state": "all"}),
        ),
        provider(
            "gh-clone-urls",
            "github",
            "getRepositoryCloneUrls",
            json!({"cwd": cwd, "repository": "octocat/demo"}),
        ),
        provider(
            "gh-clone-urls-bad",
            "github",
            "getRepositoryCloneUrls",
            json!({"cwd": cwd, "repository": "octocat/broken"}),
        ),
        provider(
            "gh-create",
            "github",
            "createRepository",
            json!({"cwd": cwd, "repository": "octocat/new", "visibility": "private"}),
        ),
        provider("gh-default-branch", "github", "getDefaultBranch", json!({"cwd": cwd})),
        provider(
            "gh-unknown-command",
            "github",
            "checkoutChangeRequest",
            json!({"cwd": cwd, "reference": "https://user:pw@github.com/o/r/pull/1?x=1"}),
        ),
        provider("glab-view", "gitlab", "getChangeRequest", json!({"cwd": cwd, "reference": "42"})),
        provider("glab-view-namespaced", "gitlab", "getChangeRequest", json!({"cwd": cwd, "reference": "43"})),
        provider("glab-not-found", "gitlab", "getChangeRequest", json!({"cwd": cwd, "reference": "44"})),
        provider(
            "glab-list-all",
            "gitlab",
            "listChangeRequests",
            json!({"cwd": cwd, "headSelector": "fork:feature/all", "state": "all"}),
        ),
        provider(
            "glab-clone-urls",
            "gitlab",
            "getRepositoryCloneUrls",
            json!({"cwd": cwd, "repository": "group/demo"}),
        ),
        provider("glab-default-branch", "gitlab", "getDefaultBranch", json!({"cwd": cwd})),
        provider("az-view", "azure-devops", "getChangeRequest", json!({"cwd": cwd, "reference": "#42"})),
        provider(
            "az-view-rest-url",
            "azure-devops",
            "getChangeRequest",
            json!({"cwd": cwd, "reference": "https://dev.azure.com/acme/project/_git/repo/pullrequest/863"}),
        ),
        provider("az-decode", "azure-devops", "getChangeRequest", json!({"cwd": cwd, "reference": "44"})),
        provider("az-not-found", "azure-devops", "getChangeRequest", json!({"cwd": cwd, "reference": "45"})),
        provider(
            "az-list",
            "azure-devops",
            "listChangeRequests",
            json!({"cwd": cwd, "headSelector": "origin:feature/merged", "state": "merged", "limit": 10}),
        ),
        provider(
            "az-clone-urls",
            "azure-devops",
            "getRepositoryCloneUrls",
            json!({"cwd": cwd, "repository": "repo"}),
        ),
        provider("az-default-branch", "azure-devops", "getDefaultBranch", json!({"cwd": cwd})),
        provider(
            "tea-view",
            "forgejo",
            "getChangeRequest",
            json!({"cwd": cwd, "context": forgejo_context, "reference": "#12"}),
        ),
        provider(
            "tea-not-found",
            "forgejo",
            "getChangeRequest",
            json!({"cwd": cwd, "context": forgejo_context, "reference": "13"}),
        ),
        provider(
            "tea-bad-reference",
            "forgejo",
            "getChangeRequest",
            json!({"cwd": cwd, "context": forgejo_context, "reference": "latest"}),
        ),
        provider(
            "tea-clone-urls",
            "forgejo",
            "getRepositoryCloneUrls",
            json!({"cwd": cwd, "context": forgejo_context, "repository": "maria/project"}),
        ),
        provider(
            "tea-default-branch",
            "forgejo",
            "getDefaultBranch",
            json!({"cwd": cwd, "context": forgejo_context}),
        ),
    ];
    let bitbucket_context = json!({"provider": {"kind": "bitbucket", "name": "Bitbucket", "baseUrl": "https://bitbucket.org"}, "remoteName": "origin", "remoteUrl": "git@bitbucket.org:team/demo.git"});
    cases.extend([
        provider(
            "bb-view",
            "bitbucket",
            "getChangeRequest",
            json!({"cwd": cwd, "context": bitbucket_context, "reference": "#42"}),
        ),
        provider(
            "bb-view-forbidden",
            "bitbucket",
            "getChangeRequest",
            json!({"cwd": cwd, "context": bitbucket_context, "reference": "https://bitbucket.org/team/demo/pull-requests/43/diff"}),
        ),
        provider(
            "bb-view-invalid",
            "bitbucket",
            "getChangeRequest",
            json!({"cwd": cwd, "context": bitbucket_context, "reference": "44"}),
        ),
        provider(
            "bb-list",
            "bitbucket",
            "listChangeRequests",
            json!({"cwd": cwd, "context": bitbucket_context, "headSelector": "fork:feature/\"q\"", "state": "closed", "limit": 99}),
        ),
        provider(
            "bb-clone-urls",
            "bitbucket",
            "getRepositoryCloneUrls",
            json!({"cwd": cwd, "context": bitbucket_context, "repository": "team/demo"}),
        ),
        provider(
            "bb-default-branch",
            "bitbucket",
            "getDefaultBranch",
            json!({"cwd": cwd, "context": bitbucket_context}),
        ),
        provider("bb-no-remote", "bitbucket", "getDefaultBranch", json!({"cwd": cwd})),
        json!({"id": "probe-bitbucket", "op": "probeBitbucket", "cwd": cwd}),
    ]);
    for kind in ["github", "gitlab", "azure-devops", "forgejo"] {
        cases.push(json!({"id": format!("probe-{kind}"), "op": "probe", "kind": kind, "cwd": cwd}));
    }
    let auth = |id: &str, kind: &str, stdout: &str, stderr: &str, exit_code: i32| json!({"id": id, "op": "parseAuth", "kind": kind, "input": {"stdout": stdout, "stderr": stderr, "exitCode": exit_code}});
    let gh_hosts = json!({"hosts": {"github.com": [
        {"state": "error", "active": true, "host": "github.com", "login": "stale", "error": " The token in keyring is invalid. "},
        {"state": "success", "active": false, "host": "GitHub.com", "login": "backup"},
    ]}})
    .to_string();
    cases.extend([
        auth("auth-gh-inactive", "github", &gh_hosts, "", 0),
        auth(
            "auth-gh-invalid-only",
            "github",
            &json!({"hosts": {"github.com": [{"state": "error", "active": true, "host": "github.com", "login": "stale"}]}}).to_string(),
            "",
            1,
        ),
        auth("auth-gh-garbage", "github", "garbage", "- Token: gho_secret\nNot logged in", 1),
        auth("auth-gh-parsed-nothing", "github", "{}", "", 0),
        auth(
            "auth-glab-port",
            "gitlab",
            "self.example.test:8443\n  ✓ Logged in to self.example.test:8443 as someone\n",
            "",
            0,
        ),
        auth("auth-glab-account-line", "gitlab", "", "account: someone (default)\n", 0),
        auth(
            "auth-glab-failed",
            "gitlab",
            "",
            "  x gitlab.com: API call failed: 401\n  - Token: glpat-secret\n",
            1,
        ),
        auth("auth-glab-unknown", "gitlab", "✓ gitlab.example.test\n", "", 0),
        auth("auth-az-ok", "azure-devops", "someone@example.test\r\nextra\n", "", 0),
        auth("auth-az-empty", "azure-devops", "  \n", "", 0),
        auth(
            "auth-tea-invalid",
            "forgejo",
            "[{\"name\":\"a\",\"url\":\"http://f.test:3000\",\"user\":\"u\",\"default\":\"false\",\"valid\":\"false\"}]",
            "",
            0,
        ),
        auth("auth-tea-none", "forgejo", "not json", "", 0),
    ]);
    let refine = |id: &str, kind: &str, name: &str, remote_url: &str, stdout: &str, requested_host: Option<&str>| {
        let mut context =
            json!({"provider": {"kind": "unknown", "name": name, "baseUrl": format!("https://{name}")}, "remoteName": "origin", "remoteUrl": remote_url});
        if let Some(host) = requested_host {
            context["requestedHost"] = json!(host);
        }
        json!({"id": id, "op": "refine", "kind": kind, "input": {"cwd": "/repo", "context": context, "auth": {"stdout": stdout, "stderr": "", "exitCode": 0}}})
    };
    let forgejo_logins = r#"[{"name":"one","url":"http://forgejo.local:3000","ssh_host":"forgejo.local","user":"m","default":"true"},{"name":"two","url":"http://forgejo.local:4000","ssh_host":"forgejo.local","user":"m","default":"false"}]"#;
    cases.extend([
        refine(
            "refine-glab",
            "gitlab",
            "Git.Example.Test",
            "https://Git.Example.Test/g/p.git",
            "git.example.test\n  ✓ Logged in to git.example.test as someone\n",
            None,
        ),
        refine(
            "refine-glab-none",
            "gitlab",
            "other.test",
            "https://other.test/g/p.git",
            "git.example.test\n  ✓ Logged in to git.example.test as someone\n",
            None,
        ),
        refine(
            "refine-tea-ambiguous",
            "forgejo",
            "forgejo.local",
            "git@forgejo.local:m/p.git",
            forgejo_logins,
            None,
        ),
        refine(
            "refine-tea-requested",
            "forgejo",
            "forgejo.local",
            "git@forgejo.local:m/p.git",
            forgejo_logins,
            Some("forgejo.local:4000"),
        ),
    ]);
    let pure = |id: &str, name: &str, args: Value| json!({"id": id, "op": "pure", "fn": name, "args": args});
    for (index, value) in [
        "https://user:secret@example.test/org/repo/pull/42?token=secret#discussion",
        "  owner/repo\n\t xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx  ",
        "ssh://git@host.test:2222/o/r.git",
        "feature/é😀\u{7f}x",
        "HTTPS://Example.TEST:443/a b?c#d",
    ]
    .iter()
    .enumerate()
    {
        cases.push(pure(&format!("safe-{index}"), "transportSafe", json!([value])));
    }
    for (index, line) in [
        "Receiving objects:  40% (4/10), 1.00 MiB | 2.00 MiB/s",
        "Receiving objects: 100% (10/10), 2.50 MiB | 2.00 MiB/s, done.",
        "remote: Compressing objects:  50% (1/2)",
        "Resolving deltas: 100% (3/3), done.",
        "Updating files:  33% (1/3)",
        "remote: Enumerating objects: 10, done.",
        "Checking out files: 250% (5/2)",
        "fatal: early EOF",
    ]
    .iter()
    .enumerate()
    {
        cases.push(pure(&format!("progress-{index}"), "cloneProgress", json!([line])));
    }
    for (index, (value, now)) in [
        (json!("120"), 1_000),
        (json!(" 0 "), 5),
        (json!("Thu, 01 Jan 1970 00:02:01 GMT"), 1_000),
        (json!("1970-01-01T00:00:00.500Z"), 1_000),
        (json!("later"), 0),
        (Value::Null, 0),
        (json!("99999999999999999999"), 0),
    ]
    .into_iter()
    .enumerate()
    {
        cases.push(pure(&format!("retry-{index}"), "retryAt", json!([value, now])));
    }
    for (index, url) in [
        "git@github.com:o/r.git",
        "https://github.example.test:8443/o/r",
        "ssh://git@gitlab.example.test/o/r",
        "http://codeberg.org/o/r",
        "https://git.forgejo.example.test/o/r",
        "git@ssh.dev.azure.com:v3/o/p/r",
        "https://acme.visualstudio.com/p/_git/r",
        "https://bitbucket.example.test/o/r",
        "https://git.example.test/o/r",
        "/local/path",
        "file:///tmp/repo.git",
    ]
    .iter()
    .enumerate()
    {
        cases.push(pure(&format!("detect-{index}"), "detectProvider", json!([url])));
    }
    for (index, url) in [
        "git@forgejo.local:maria/project.git",
        "forgejo.local:maria/project",
        "ssh://git@Host.Test:2222/a/b.git",
        "http://forgejo.local:4000",
        "https://x.test/mount/o/r.git/",
        "/abs/path",
        "c:/windows",
    ]
    .iter()
    .enumerate()
    {
        cases.push(pure(&format!("forgejo-remote-{index}"), "forgejoRemote", json!([url])));
    }
    let logins: Value = serde_json::from_str(forgejo_logins).unwrap();
    for (index, (remote, requested, host_only)) in [
        ("git@forgejo.local:maria/project.git", Value::Null, false),
        ("git@forgejo.local:maria/project.git", json!("forgejo.local:4000"), false),
        ("http://forgejo.local:4000/maria/project.git", Value::Null, false),
        ("http://forgejo.local:4000", Value::Null, true),
        ("http://forgejo.local:5000/x/y", Value::Null, false),
    ]
    .into_iter()
    .enumerate()
    {
        cases.push(pure(
            &format!("forgejo-login-{index}"),
            "forgejoLogin",
            json!([logins, remote, requested, host_only]),
        ));
    }
    for (index, input) in [
        json!({"pullRequestId": 1, "webLink": " https://dev.azure.com/a/p/_git/r/pullrequest/1 "}),
        json!({"pullRequestId": 2, "repositoryWebUrl": "https://dev.azure.com/a/p/_git/r//"}),
        json!({"pullRequestId": 3, "restApiUrl": "https://dev.azure.com/org/x/_apis/git/repositories/y/pullRequests/3", "projectName": "My Project", "repositoryName": "R/1"}),
        json!({"pullRequestId": 4, "restApiUrl": "https://org.visualstudio.com/x/_apis/git/pullRequests/4", "projectName": "p", "repositoryName": "r"}),
        json!({"pullRequestId": 5, "restApiUrl": "https://example.test/not-azure/5", "projectName": "p", "repositoryName": "r"}),
        json!({"pullRequestId": 6}),
    ]
    .into_iter()
    .enumerate()
    {
        cases.push(pure(&format!("azure-url-{index}"), "azureWebUrl", json!([input])));
    }
    for (index, selector) in ["fork:feature/x", " fork : x ", "feature/x", "a/b:c", ":x", "owner:"].iter().enumerate() {
        cases.push(pure(&format!("owner-ref-{index}"), "ownerRef", json!([selector])));
    }
    cases
}

#[tokio::test]
async fn rust_matches_the_typescript_source_control_code() {
    if let Err(reason) = oracle_available() {
        eprintln!("skipping the TS golden comparison: {reason}");
        return;
    }
    let clis = FakeClis::new();
    record_fake_outputs(&clis);
    let cwd = Tmp::new("zc-golden-cwd-");
    let home = Tmp::new("zc-golden-home-");
    let server = MockServer::start(|seen| {
        let pull_request = json!({
            "id": 42, "title": " Add Bitbucket provider ", "state": "DECLINED", "draft": true, "updated_on": "2026-01-02T00:00:00.000Z",
            "links": {"html": {"href": "https://bitbucket.org/team/demo/pull-requests/42"}},
            "source": {"branch": {"name": "feature/x"}, "repository": {"full_name": "someone/demo"}},
            "destination": {"branch": {"name": "main"}, "repository": {"full_name": "team/demo", "workspace": {"slug": "team"}}},
        });
        let path = seen.target.split('?').next().unwrap_or_default().to_owned();
        match path.as_str() {
            "/2.0/repositories/team/demo/pullrequests/42" => reply(200, pull_request.to_string()),
            "/2.0/repositories/team/demo/pullrequests/43" => reply(403, r#"{"error":{"message":"secret"}}"#),
            "/2.0/repositories/team/demo/pullrequests/44" => reply(200, r#"{"id":44}"#),
            "/2.0/repositories/team/demo/pullrequests" => reply(200, json!({"values": [pull_request], "next": "https://api.bitbucket.org/2.0/x?page=2"}).to_string()),
            "/2.0/repositories/team/demo" => reply(
                200,
                json!({"full_name": "team/demo", "links": {"html": {"href": "https://bitbucket.org/team/demo"}, "clone": [{"name": "HTTPS", "href": "https://bitbucket.org/team/demo.git"}]}, "mainbranch": {"name": "main"}}).to_string(),
            ),
            "/2.0/repositories/team/demo/branching-model" => reply(200, r#"{"development":{"name":"develop","use_mainbranch":false}}"#),
            "/2.0/user" => reply(200, r#"{"display_name":" Some One ","account_id":"abc"}"#),
            _ => reply(404, "{}"),
        }
    })
    .await;
    let cases = cases(cwd.str());
    let (oracle_cases, bin, home_path, base) = (cases.clone(), clis.dir.path.clone(), home.path.clone(), server.base.clone());
    let ts = tokio::task::spawn_blocking(move || run_oracle(&oracle_cases, &bin, &home_path, &base))
        .await
        .unwrap();
    let mut ts_queries: Vec<String> = server.seen().iter().map(|s| format!("{} {}", s.method, s.target)).collect();

    let rust = Rust {
        process: clis.process(),
        home: home.path.clone(),
        bitbucket_base: server.base.clone(),
        _worktrees: Tmp::new("zc-golden-worktrees-"),
    };
    let mut mismatches = Vec::new();
    for case in &cases {
        let id = case["id"].as_str().unwrap();
        let mut expected = ts.get(id).cloned().unwrap_or(Value::Null);
        let mut actual = rust.run(case).await;
        for value in [&mut expected, &mut actual] {
            if let Some(error) = value.get_mut("error") {
                normalize(error, 0);
            }
        }
        if expected != actual {
            mismatches.push(format!("{id}:\n  ts:   {expected}\n  rust: {actual}"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} cases differ:\n{}",
        mismatches.len(),
        cases.len(),
        mismatches.join("\n")
    );
    // Both sides sent the same Bitbucket requests (method, path and query, in order).
    let all: Vec<String> = server.seen().iter().map(|s| format!("{} {}", s.method, s.target)).collect();
    let mut rust_queries = all[ts_queries.len()..].to_vec();
    ts_queries.sort();
    rust_queries.sort();
    assert_eq!(ts_queries, rust_queries);
    eprintln!("{} cases identical", cases.len());
}
