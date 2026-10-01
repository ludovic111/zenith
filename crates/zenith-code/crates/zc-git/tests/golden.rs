//! Golden comparison against the TypeScript GitManager: the same scripted scenario (a dirty
//! repository → commit with a generated message → push to a local bare remote → PR through a
//! fake `gh`) runs through the TS code (from source, `golden/ts_oracle.mjs`) and through
//! [`zc_git::GitManager`], each on its own identical repository. Both must leave the same git
//! history (same SHAs: commit dates are pinned), call `gh` with the same argv (the PR body
//! file's random name aside) and the same body, emit the same progress events and result, and
//! hand the text generation the same inputs.
//!
//! Needs `node` and `code/apps/server/node_modules` (symlinked from a checkout that has them);
//! without them the test prints why and passes vacuously.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, Once};

use async_trait::async_trait;
use common::*;
use serde_json::{json, Value};
use zc_core::process::{ProcessRunError, ProcessRunInput, ProcessRunOutput, ProcessRunner, SystemProcessRunner};
use zc_core::vcs_process::VcsProcess;
use zc_git::{FixedProvider, GitManager, GitManagerDeps, SettingsSources};
use zc_ports::text_generation::*;
use zc_ports::TextGeneration;
use zc_sourcecontrol::github::{GitHubCli, GitHubSourceControlProvider};
use zc_vcs::GitVcsDriver;

const COMMIT_SUBJECT: &str = "Add the golden widget";
const COMMIT_BODY: &str = "Explains the widget.";
const PR_TITLE: &str = "Golden widget";
const PR_BODY: &str = "## Summary\n- Adds the golden widget\n";
const PR_URL: &str = "https://github.com/acme-labs/widget-shop/pull/7";
const BRANCH: &str = "feature/golden-widget";

static DATES: Once = Once::new();

/// Pin commit dates so both runs produce the same SHAs.
fn pin_dates() {
    DATES.call_once(|| {
        // SAFETY: once, before this binary spawns any process.
        unsafe {
            std::env::set_var("GIT_AUTHOR_DATE", "2026-01-02T03:04:05Z");
            std::env::set_var("GIT_COMMITTER_DATE", "2026-01-02T03:04:05Z");
        }
    });
}

fn server_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../code/apps/server")
}

fn oracle_available() -> Result<(), String> {
    if !server_dir().join("node_modules/effect").exists() {
        return Err(format!("{} has no node_modules", server_dir().display()));
    }
    match Command::new("node").arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
        _ => Err("node is not installed".into()),
    }
}

/// A fake `gh` in its own directory: logs argv (and the PR body), answers the quota probe,
/// `pr list` (empty until `pr create` ran), `pr create` and `repo view`.
fn fake_gh() -> Tmp {
    let bin = Tmp::new("zc-git-golden-bin-");
    let pr = json!([{
        "number": 7,
        "title": PR_TITLE,
        "url": PR_URL,
        "baseRefName": "main",
        "headRefName": BRANCH,
        "state": "OPEN",
        "isDraft": false,
        "isCrossRepository": false,
        "headRepository": {"nameWithOwner": "acme-labs/widget-shop"},
        "headRepositoryOwner": {"login": "acme-labs"}
    }]);
    std::fs::write(bin.path.join("pr.json"), pr.to_string()).unwrap();
    let script = format!(
        r#"#!/bin/sh
d="$(dirname "$0")"
printf '%s\n' "$*" >> "$d/gh.log"
case "$1 $2" in
  "api rate_limit") echo '{{"data":{{"rateLimit":{{"cost":1,"limit":5000,"remaining":4999,"resetAt":"2099-01-01T00:00:00Z"}}}}}}' ;;
  "pr list") if [ -f "$d/created" ]; then cat "$d/pr.json"; else echo '[]'; fi ;;
  "pr create")
    touch "$d/created"
    prev=""
    for a in "$@"; do
      if [ "$prev" = "--body-file" ]; then printf 'BODY:%s\n' "$(cat "$a")" >> "$d/gh.log"; fi
      prev="$a"
    done
    echo "{PR_URL}" ;;
  "repo view") echo main ;;
  *) echo "unknown command: $*" >&2; exit 1 ;;
esac
"#
    );
    let path = bin.path.join("gh");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// The same repository each time: `main` (README, AGENTS.md) pushed to a bare remote that reads
/// as GitHub, a feature branch with an uncommitted change.
fn scenario_repo() -> (Tmp, Tmp) {
    pin_dates();
    let repo = repo();
    write(&repo.path, "AGENTS.md", "Keep subjects short.\n");
    git(&repo.path, &["add", "AGENTS.md"]);
    git(&repo.path, &["commit", "-m", "Add agent notes"]);
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    configure_visible_remote(&repo.path, "origin", "https://github.com/acme-labs/widget-shop.git", remote.str());
    git(&repo.path, &["push", "-u", "origin", "main"]);
    git(&repo.path, &["checkout", "-b", BRANCH]);
    write(&repo.path, "widget.txt", "golden\n");
    write(&repo.path, "README.md", "hello\nwidget\n");
    (repo, remote)
}

/// Runs `gh` from `bin`, everything else for real.
struct BinRunner(PathBuf);

#[async_trait]
impl ProcessRunner for BinRunner {
    async fn run(&self, mut input: ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
        if input.command == "gh" {
            input.command = self.0.join("gh").to_string_lossy().into_owned();
        }
        SystemProcessRunner.run(input).await
    }
}

/// The fake text generation of both sides, recording its inputs as the TS objects.
struct GoldenText {
    commit: Mutex<Vec<Value>>,
    pr: Mutex<Vec<Value>>,
}

fn policy_json(policy: &Option<TextGenerationPolicy>) -> Value {
    policy.as_ref().map(|p| serde_json::to_value(p).unwrap()).unwrap_or(Value::Null)
}

#[async_trait]
impl TextGeneration for GoldenText {
    async fn generate_commit_message(
        &self,
        input: CommitMessageGenerationInput,
    ) -> Result<CommitMessageGenerationResult, zc_ports::contracts::TextGenerationError> {
        let mut recorded = json!({
            "cwd": input.cwd,
            "branch": input.branch,
            "stagedSummary": input.staged_summary,
            "stagedPatch": input.staged_patch,
            "policy": policy_json(&input.policy),
            "modelSelection": input.model_selection.0,
        });
        if input.include_branch {
            recorded["includeBranch"] = json!(true);
        }
        self.commit.lock().unwrap().push(recorded);
        Ok(CommitMessageGenerationResult {
            subject: COMMIT_SUBJECT.into(),
            body: COMMIT_BODY.into(),
            branch: None,
        })
    }

    async fn generate_pr_content(&self, input: PrContentGenerationInput) -> Result<PrContentGenerationResult, zc_ports::contracts::TextGenerationError> {
        let mut recorded = json!({
            "cwd": input.cwd,
            "baseBranch": input.base_branch,
            "headBranch": input.head_branch,
            "commitSummary": input.commit_summary,
            "diffSummary": input.diff_summary,
            "diffPatch": input.diff_patch,
            "policy": policy_json(&input.policy),
            "modelSelection": input.model_selection.0,
        });
        if let Some(template) = input.change_request_template {
            recorded["changeRequestTemplate"] = json!(template);
        }
        self.pr.lock().unwrap().push(recorded);
        Ok(PrContentGenerationResult {
            title: PR_TITLE.into(),
            body: PR_BODY.into(),
        })
    }

    async fn generate_branch_name(&self, _: BranchNameGenerationInput) -> Result<String, zc_ports::contracts::TextGenerationError> {
        Ok("unused".into())
    }

    async fn generate_thread_title(&self, _: ThreadTitleGenerationInput) -> Result<ThreadTitleGenerationResult, zc_ports::contracts::TextGenerationError> {
        Ok(ThreadTitleGenerationResult {
            title: "unused".into(),
            needs_refinement: None,
        })
    }
}

/// What a run left behind.
#[derive(Debug, PartialEq)]
struct Outcome {
    output: Value,
    gh: Vec<String>,
    history: String,
    remote_refs: String,
    status: String,
}

fn replace_paths(value: &mut Value, from: &[(&str, &str)]) {
    match value {
        Value::String(text) => {
            for (path, marker) in from {
                *text = text.replace(path, marker);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| replace_paths(item, from)),
        Value::Object(map) => map.values_mut().for_each(|item| replace_paths(item, from)),
        _ => {}
    }
}

fn collect(mut output: Value, repo: &Tmp, remote: &Tmp, bin: &Tmp) -> Outcome {
    replace_paths(&mut output, &[(repo.str(), "<repo>"), (remote.str(), "<remote>")]);
    let body_file = regex::Regex::new(r"--body-file \S+").unwrap();
    let gh = std::fs::read_to_string(bin.path.join("gh.log"))
        .unwrap_or_default()
        .lines()
        .map(|line| body_file.replace(line, "--body-file <body-file>").into_owned())
        .collect();
    Outcome {
        output,
        gh,
        history: git(&repo.path, &["log", "--all", "--format=%H %P %an <%ae> %s%n%b"]),
        remote_refs: git(&remote.path, &["for-each-ref", "--format=%(refname) %(objectname)"]),
        status: git(&repo.path, &["status", "--porcelain=v1", "--branch"]),
    }
}

async fn run_rust(action: &str) -> Outcome {
    let (repo, remote) = scenario_repo();
    let bin = fake_gh();
    let temp = Tmp::new("zc-git-golden-temp-");
    let process = VcsProcess::new(Arc::new(BinRunner(bin.path.clone())));
    let provider = Arc::new(GitHubSourceControlProvider::new(GitHubCli::new(
        process,
        zc_sourcecontrol::util::system_clock(),
    )));
    let text = Arc::new(GoldenText {
        commit: Mutex::new(Vec::new()),
        pr: Mutex::new(Vec::new()),
    });
    let manager = GitManager::new(GitManagerDeps {
        git: GitVcsDriver::new(temp.path.join("worktrees")),
        providers: Arc::new(FixedProvider(provider)),
        text_generation: text.clone(),
        settings: SettingsSources {
            settings: MemorySettings::new(json!({})),
            provider_status: Arc::new(FixedProviders(Vec::new())),
            projections: None,
        },
        setup_scripts: None,
        temp_dir: temp.path.clone(),
        uuids: Arc::new(zc_core::uuid_v4),
    });
    let (options, events) = recorder();
    let input = serde_json::from_value(json!({"actionId": "test-action-id", "cwd": repo.str(), "action": action})).unwrap();
    let mut output = json!({
        "events": events.lock().unwrap().clone(),
        "textInputs": {},
    });
    let result = manager.run_stacked_action(input, options).await;
    output["events"] = json!(events.lock().unwrap().clone());
    output["textInputs"] = json!({"commit": text.commit.lock().unwrap().clone(), "pr": text.pr.lock().unwrap().clone()});
    match result {
        Ok(result) => output["result"] = serde_json::to_value(result).unwrap(),
        Err(error) => output["error"] = json!({"message": error.message()}),
    }
    collect(output, &repo, &remote, &bin)
}

fn run_ts(action: &str) -> Outcome {
    let (repo, remote) = scenario_repo();
    let bin = fake_gh();
    let temp = Tmp::new("zc-git-golden-ts-temp-");
    let script = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/ts_oracle.mjs")).unwrap();
    let server = std::fs::canonicalize(server_dir()).unwrap();
    let path = format!("{}:{}", bin.path.display(), std::env::var("PATH").unwrap_or_default());
    let mut child = Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", &script])
        .current_dir(&server)
        .env("ZC_SERVER_SRC", server.join("src"))
        .env("PATH", path)
        .env("TMPDIR", &temp.path)
        .env_remove("GH_HOST")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            json!({
                "cwd": repo.str(),
                "action": action,
                "actionId": "test-action-id",
                "settings": {},
                "commit": {"subject": COMMIT_SUBJECT, "body": COMMIT_BODY},
                "pr": {"title": PR_TITLE, "body": PR_BODY},
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "oracle failed: {}", String::from_utf8_lossy(&output.stderr));
    let mut value: Value = serde_json::from_slice(&output.stdout).unwrap();
    if let Some(error) = value.get_mut("error") {
        error.as_object_mut().unwrap().remove("tag");
    }
    collect(value, &repo, &remote, &bin)
}

fn assert_same(ts: &Outcome, rust: &Outcome) {
    if std::env::var_os("ZC_GOLDEN_PRINT").is_some() {
        eprintln!("TS: {ts:#?}\nRust: {rust:#?}");
    }
    assert_eq!(ts.gh, rust.gh, "gh argv differ");
    assert_eq!(ts.history, rust.history, "git history differs");
    assert_eq!(ts.remote_refs, rust.remote_refs, "remote refs differ");
    assert_eq!(ts.status, rust.status, "working tree status differs");
    assert_eq!(ts.output["textInputs"], rust.output["textInputs"], "text generation inputs differ");
    assert_eq!(ts.output["events"], rust.output["events"], "progress events differ");
    assert_eq!(ts.output["result"], rust.output["result"], "results differ");
    assert_eq!(ts.output["error"], rust.output["error"], "errors differ");
}

#[tokio::test]
async fn commit_push_pr_matches_the_typescript_git_manager() {
    if let Err(why) = oracle_available() {
        eprintln!("skipping the TS golden comparison: {why}");
        return;
    }
    let ts = run_ts("commit_push_pr");
    let rust = run_rust("commit_push_pr").await;
    assert_same(&ts, &rust);
    // The scenario did what it says.
    let result = &rust.output["result"];
    assert_eq!(result["commit"]["status"], json!("created"));
    assert_eq!(result["push"]["status"], json!("pushed"));
    assert_eq!(result["pr"]["status"], json!("created"));
    assert_eq!(result["pr"]["url"], json!(PR_URL));
    assert!(rust
        .gh
        .iter()
        .any(|line| line.starts_with("pr create --base main --head feature/golden-widget --title Golden widget")));
    assert!(rust.gh.iter().any(|line| line == "BODY:## Summary"), "{:?}", rust.gh);
    let kinds: Vec<&str> = rust.output["events"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds.first(), Some(&"action_started"));
    assert_eq!(kinds.last(), Some(&"action_finished"));
}

#[tokio::test]
async fn commit_only_matches_the_typescript_git_manager() {
    if let Err(why) = oracle_available() {
        eprintln!("skipping the TS golden comparison: {why}");
        return;
    }
    let ts = run_ts("commit");
    let rust = run_rust("commit").await;
    assert_same(&ts, &rust);
    assert!(rust.gh.is_empty(), "{:?}", rust.gh);
}

#[tokio::test]
async fn create_pr_on_a_dirty_tree_fails_like_the_typescript_git_manager() {
    if let Err(why) = oracle_available() {
        eprintln!("skipping the TS golden comparison: {why}");
        return;
    }
    let ts = run_ts("create_pr");
    let rust = run_rust("create_pr").await;
    assert_same(&ts, &rust);
    assert_eq!(
        rust.output["error"]["message"],
        json!("Git manager failed in runStackedAction: Commit local changes before creating a PR.")
    );
}
