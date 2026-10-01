//! Golden comparison against the TypeScript driver: scripted repositories are read by the TS
//! `GitVcsDriver` (through node and the real effect/contracts packages, see
//! `golden/ts_oracle.mjs`) and by the Rust driver, and the wire JSON must match.
//!
//! Needs `node` and `code/apps/server/node_modules` (symlinked from a checkout that has them);
//! without them the test prints why and passes vacuously. Timestamps (`generatedAt`,
//! `observedAt`) are normalized; everything else, including diff hashes, must be identical.

#![allow(clippy::result_large_err)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use common::*;
use serde_json::{json, Value};
use zc_core::vcs_process::VcsProcess;
use zc_vcs::contracts::*;
use zc_vcs::vcs_driver::{DiffCheckpointsInput, GitVcsProcessDriver, VcsDriver};
use zc_vcs::GitVcsDriver;

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

fn run_oracle(base_dir: &Path, ops: &[Value]) -> serde_json::Map<String, Value> {
    let script = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/ts_oracle.mjs")).unwrap();
    let server = std::fs::canonicalize(server_dir()).unwrap();
    let mut child = Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", &script])
        .current_dir(&server)
        .env("ZC_SERVER_SRC", server.join("src"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let request = json!({"baseDir": base_dir, "ops": ops});
    use std::io::Write;
    child.stdin.take().unwrap().write_all(request.to_string().as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "oracle failed: {}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Replace wall-clock fields with a placeholder, and drop the Node error nested inside a
/// `PlatformError` defect (Effect keeps libuv's message there; Rust reports the same
/// `PlatformError` name and message without it).
fn normalize(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if (key == "generatedAt" || key == "observedAt") && entry.is_string() {
                    *entry = json!("<time>");
                } else if key == "cause" {
                    if let Some(cause) = entry.as_object_mut() {
                        cause.remove("cause");
                    }
                } else {
                    normalize(entry);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize),
        _ => {}
    }
}

fn ok_or_error<T: serde::Serialize, E: serde::Serialize>(result: Result<T, E>) -> Value {
    match result {
        Ok(value) => json!({"ok": serde_json::to_value(value).unwrap()}),
        Err(error) => json!({"error": serde_json::to_value(error).unwrap()}),
    }
}

struct Rust {
    core: GitVcsDriver,
    vcs: GitVcsProcessDriver,
}

impl Rust {
    async fn run(&self, op: &str, input: &Value) -> Value {
        let cwd = input["cwd"].as_str().unwrap_or_default();
        match op {
            "status" => ok_or_error(self.core.status(cwd).await),
            "listRefs" => {
                let input: VcsListRefsInput = serde_json::from_value(input.clone()).unwrap();
                ok_or_error(self.core.list_refs(&input).await)
            }
            "reviewPreview" => {
                let input: ReviewDiffPreviewInput = serde_json::from_value(input.clone()).unwrap();
                ok_or_error(self.core.get_review_diff_preview(&input).await)
            }
            "reviewFileContents" => {
                let input: ReviewDiffFileContentsInput = serde_json::from_value(input.clone()).unwrap();
                ok_or_error(self.core.get_review_diff_file_contents(&input).await)
            }
            "detectRepository" => ok_or_error(self.vcs.detect_repository(cwd).await),
            "listWorkspaceFiles" => ok_or_error(self.vcs.list_workspace_files(cwd).await),
            "listRemotes" => ok_or_error(self.vcs.list_remotes(cwd).await),
            "filterIgnoredPaths" => {
                let paths: Vec<String> = serde_json::from_value(input["paths"].clone()).unwrap();
                ok_or_error(self.vcs.filter_ignored_paths(cwd, &paths).await)
            }
            "capture" => {
                let checkpoint_ref = input["checkpointRef"].as_str().unwrap();
                ok_or_error(
                    self.vcs
                        .checkpoints()
                        .unwrap()
                        .capture_checkpoint(cwd, checkpoint_ref)
                        .await
                        .map(|()| Value::Null),
                )
            }
            "diffCheckpoints" => ok_or_error(
                self.vcs
                    .checkpoints()
                    .unwrap()
                    .diff_checkpoints(&DiffCheckpointsInput {
                        cwd: cwd.into(),
                        from_checkpoint_ref: input["fromCheckpointRef"].as_str().unwrap().into(),
                        to_checkpoint_ref: input["toCheckpointRef"].as_str().unwrap().into(),
                        fallback_from_to_head: input["fallbackFromToHead"].as_bool().unwrap_or(false),
                        ignore_whitespace: input["ignoreWhitespace"].as_bool().unwrap_or(false),
                        numstat: input["format"] == json!("numstat"),
                    })
                    .await,
            ),
            "statusDetailsRemote" => ok_or_error(self.core.status_details_remote(cwd, false).await.map(|d| {
                json!({
                    "isRepo": d.is_repo,
                    "defaultBranch": d.default_branch,
                    "isDefaultBranch": d.is_default_branch,
                    "branch": d.branch,
                    "upstreamRef": d.upstream_ref,
                    "hasUpstream": d.has_upstream,
                    "aheadCount": d.ahead_count,
                    "behindCount": d.behind_count,
                    "aheadOfDefaultCount": d.ahead_of_default_count,
                })
            })),
            other => panic!("unknown op {other}"),
        }
    }
}

/// Builds the scripted repositories. Returns (repo, linked worktree, unborn repo, detached
/// clone, plain directory) and keeps the temp dirs alive.
struct Fixture {
    _dirs: Vec<Tmp>,
    repo: String,
    linked: String,
    unborn: String,
    detached: String,
    plain: String,
}

fn fixture() -> Fixture {
    let repo = Tmp::new("zc-golden-repo-");
    let remote = Tmp::new("zc-golden-remote-");
    let others = Tmp::new("zc-golden-others-");
    let r = &repo.path;
    git(r, &["init"]);
    git(r, &["config", "user.email", "test@test.com"]);
    git(r, &["config", "user.name", "Test"]);
    write(r, "README.md", "# golden\n\nsome text\n");
    write(r, "src/app.ts", "export const a = 1;\nexport const b = 2;\n");
    write(r, "docs/Guide.md", "guide\n");
    write(r, "b.txt", "one\ntwo\nthree\nfour\n");
    write(r, ".gitignore", "*.log\nbuild/\n");
    commit_all(r, "initial");
    git(r, &["branch", "-M", "main"]);
    git(&remote.path, &["init", "--bare"]);
    git(r, &["remote", "add", "origin", remote.str()]);
    git(r, &["remote", "add", "fork", "git@github.com:someone/golden.git"]);
    git(r, &["push", "-u", "origin", "main"]);
    git(r, &["remote", "set-head", "origin", "main"]);
    // Same-age branches, to exercise the locale tie-break.
    for name in ["zeta", "Alpha", "alpha-2", "_under", "feature-b", "release/1.0"] {
        git(r, &["branch", name]);
    }
    git(r, &["push", "origin", "main:only-remote", "main:Alpha"]);
    git(r, &["fetch", "origin"]);
    git(r, &["checkout", "-b", "feature/a"]);
    write(r, "src/app.ts", "export const a = 1;\nexport const b = 3;\n");
    write(r, "feature.md", "feature work\n");
    commit_all(r, "feature commit");
    git(r, &["push", "-u", "origin", "feature/a"]);
    // A dirty tree of every kind.
    write(r, "src/app.ts", "export const a = 10;\nexport const b = 3;\n");
    write(r, "docs/Guide.md", "guide, staged\n");
    git(r, &["add", "docs/Guide.md"]);
    git(r, &["mv", "b.txt", "renamed.txt"]);
    write(r, "renamed.txt", "one\ntwo\nTHREE\nfour\n");
    write(r, "README.md", "#  golden\n\nsome text\n");
    write(r, "new file.txt", "untracked\n");
    write(r, "ünïcode.txt", "unicode\n");
    write(r, "Zebra.txt", "z\n");
    write(r, "_lead.txt", "l\n");
    std::fs::write(r.join("bin.dat"), b"bin\0ary").unwrap();
    write(r, "debug.log", "ignored\n");

    let linked = others.join("linked");
    git(r, &["worktree", "add", "-b", "wt-branch", &linked]);
    write(&linked, "wt.txt", "in the worktree\n");

    let unborn = others.join("unborn");
    std::fs::create_dir(&unborn).unwrap();
    git(&unborn, &["init"]);
    write(&unborn, "staged.txt", "staged\n");
    git(&unborn, &["add", "staged.txt"]);
    write(&unborn, "untracked.txt", "untracked\n");

    let detached = others.join("detached");
    git(&remote.path, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git(&others.path, &["clone", "-q", remote.str(), "detached"]);
    let head = git(&detached, &["rev-parse", "HEAD"]);
    git(&detached, &["checkout", "-q", "--detach", &head]);

    let plain = others.join("plain");
    std::fs::create_dir(&plain).unwrap();

    Fixture {
        repo: repo.str().to_owned(),
        linked,
        unborn,
        detached,
        plain,
        _dirs: vec![repo, remote, others],
    }
}

fn read_ops(f: &Fixture) -> Vec<Value> {
    let repo = &f.repo;
    let mut ops = Vec::new();
    let mut add = |id: &str, op: &str, input: Value| ops.push(json!({"id": id, "op": op, "input": input}));
    for (name, cwd) in [
        ("repo", repo.as_str()),
        ("linked", f.linked.as_str()),
        ("unborn", f.unborn.as_str()),
        ("detached", f.detached.as_str()),
        ("plain", f.plain.as_str()),
    ] {
        add(&format!("status:{name}"), "status", json!({"cwd": cwd}));
        add(&format!("listRefs:{name}"), "listRefs", json!({"cwd": cwd}));
        add(&format!("preview:{name}"), "reviewPreview", json!({"cwd": cwd}));
        add(&format!("detect:{name}"), "detectRepository", json!({"cwd": cwd}));
        add(&format!("remoteDetails:{name}"), "statusDetailsRemote", json!({"cwd": cwd}));
    }
    add("listRefs:all", "listRefs", json!({"cwd": repo, "includeMatchingRemoteRefs": true}));
    add("listRefs:remote", "listRefs", json!({"cwd": repo, "refKind": "remote"}));
    add("listRefs:query", "listRefs", json!({"cwd": repo, "refKind": "local", "query": "A"}));
    add("listRefs:page", "listRefs", json!({"cwd": repo, "cursor": 2, "limit": 3}));
    add("listRefs:refresh", "listRefs", json!({"cwd": repo, "refresh": true, "limit": 50}));
    add("preview:base", "reviewPreview", json!({"cwd": repo, "baseRef": "main"}));
    add("preview:ws", "reviewPreview", json!({"cwd": repo, "baseRef": "main", "ignoreWhitespace": true}));
    add(
        "preview:file",
        "reviewPreview",
        json!({"cwd": repo, "file": {"path": "src/app.ts", "previousPath": null, "sourceKind": "working-tree"}}),
    );
    add(
        "preview:rename",
        "reviewPreview",
        json!({"cwd": repo, "file": {"path": "renamed.txt", "previousPath": "b.txt", "sourceKind": "working-tree"}}),
    );
    add(
        "preview:branchFile",
        "reviewPreview",
        json!({"cwd": repo, "baseRef": "main", "file": {"path": "feature.md", "previousPath": null, "sourceKind": "branch-range"}}),
    );
    add("preview:nested", "reviewPreview", json!({"cwd": format!("{repo}/src"), "baseRef": "main"}));
    add(
        "contents:wt",
        "reviewFileContents",
        json!({"cwd": repo, "sourceKind": "working-tree", "changeType": "change", "baseRef": "HEAD", "headRef": null, "oldPath": "src/app.ts", "newPath": "src/app.ts"}),
    );
    add(
        "contents:branch",
        "reviewFileContents",
        json!({"cwd": repo, "sourceKind": "branch-range", "changeType": "change", "baseRef": "main", "headRef": "feature/a", "oldPath": "src/app.ts", "newPath": "src/app.ts"}),
    );
    add(
        "contents:missing",
        "reviewFileContents",
        json!({"cwd": repo, "sourceKind": "working-tree", "changeType": "new", "baseRef": "HEAD", "headRef": null, "oldPath": "nope.ts", "newPath": "nope.ts"}),
    );
    add("files:repo", "listWorkspaceFiles", json!({"cwd": repo}));
    add("remotes:repo", "listRemotes", json!({"cwd": repo}));
    add(
        "ignored:repo",
        "filterIgnoredPaths",
        json!({"cwd": repo, "paths": ["keep.ts", "debug.log", "build/out.js", "src/app.ts"]}),
    );
    add("detect:nested", "detectRepository", json!({"cwd": format!("{repo}/src")}));
    ops
}

fn checkpoint(side: &str, n: u32) -> String {
    format!("refs/t3/checkpoints/golden/{side}/turn/{n}")
}

fn tree_of(cwd: &str, reference: &str) -> String {
    git(cwd, &["rev-parse", &format!("{reference}^{{tree}}")])
}

#[tokio::test]
async fn rust_and_ts_drivers_agree_on_wire_json() {
    if let Err(reason) = oracle_available() {
        eprintln!("skipping the golden comparison: {reason}");
        return;
    }
    let f = fixture();
    let base = Tmp::new("zc-golden-base-");
    let rust = Rust {
        core: GitVcsDriver::new(base.path.join("worktrees")),
        vcs: GitVcsProcessDriver::new(VcsProcess::default()),
    };

    // Phase 1: reads, then the first checkpoint of each side.
    let mut ops = read_ops(&f);
    let capture = |side: &str, n: u32| json!({"cwd": f.repo, "checkpointRef": checkpoint(side, n)});
    ops.push(json!({"id": "capture:1", "op": "capture", "input": capture("ts", 1)}));
    // An unborn repository (no HEAD to seed the index from) and a linked worktree (shared
    // refs, its own index).
    ops.push(json!({"id": "capture:unborn", "op": "capture", "input": {"cwd": f.unborn, "checkpointRef": checkpoint("ts", 1)}}));
    ops.push(json!({"id": "capture:linked", "op": "capture", "input": {"cwd": f.linked, "checkpointRef": checkpoint("ts", 11)}}));
    let ts = run_oracle(&base.path, &ops);
    let mut mismatches = Vec::new();
    let mut compared = 0;
    for op in &ops {
        let id = op["id"].as_str().unwrap();
        let input: Value = serde_json::from_str(&op["input"].to_string().replace("/golden/ts/", "/golden/rs/")).unwrap();
        let mut expected = ts.get(id).cloned().unwrap_or(Value::Null);
        let mut actual = rust.run(op["op"].as_str().unwrap(), &input).await;
        normalize(&mut expected);
        normalize(&mut actual);
        compared += 1;
        if expected != actual {
            mismatches.push(format!(
                "{id}\n  ts:   {}\n  rust: {}",
                serde_json::to_string(&expected).unwrap(),
                serde_json::to_string(&actual).unwrap()
            ));
        }
    }

    // Phase 2: change the tree, capture again on both sides, and diff.
    write(&f.repo, "src/app.ts", "export const a = 11;\nexport const b = 3;\n");
    write(&f.repo, "second turn.txt", "second\n");
    std::fs::remove_file(Path::new(&f.repo).join("Zebra.txt")).unwrap();
    let diff = |side: &str, format: &str| json!({"cwd": f.repo, "fromCheckpointRef": checkpoint(side, 1), "toCheckpointRef": checkpoint(side, 2), "ignoreWhitespace": false, "format": format});
    let phase2 = vec![
        json!({"id": "capture:2", "op": "capture", "input": capture("ts", 2)}),
        json!({"id": "diff:patch", "op": "diffCheckpoints", "input": diff("ts", "patch")}),
        json!({"id": "diff:numstat", "op": "diffCheckpoints", "input": diff("ts", "numstat")}),
        json!({"id": "diff:fallback", "op": "diffCheckpoints", "input": {"cwd": f.repo, "fromCheckpointRef": "refs/t3/checkpoints/golden/missing", "toCheckpointRef": checkpoint("ts", 2), "ignoreWhitespace": true, "fallbackFromToHead": true}}),
        json!({"id": "diff:missing", "op": "diffCheckpoints", "input": {"cwd": f.repo, "fromCheckpointRef": "refs/t3/checkpoints/golden/missing", "toCheckpointRef": checkpoint("ts", 2), "ignoreWhitespace": false}}),
    ];
    let ts2 = run_oracle(&base.path, &phase2);
    for op in &phase2 {
        let id = op["id"].as_str().unwrap();
        let rust_input: Value = serde_json::from_str(&op["input"].to_string().replace("/golden/ts/", "/golden/rs/")).unwrap();
        let expected = ts2.get(id).cloned().unwrap_or(Value::Null);
        let actual = rust.run(op["op"].as_str().unwrap(), &rust_input).await;
        compared += 1;
        if expected != actual {
            mismatches.push(format!(
                "{id}\n  ts:   {}\n  rust: {}",
                serde_json::to_string(&expected).unwrap(),
                serde_json::to_string(&actual).unwrap()
            ));
        }
    }
    for (cwd, n) in [(&f.repo, 1), (&f.repo, 2), (&f.unborn, 1), (&f.repo, 11)] {
        compared += 1;
        let (ts_tree, rs_tree) = (tree_of(cwd, &checkpoint("ts", n)), tree_of(cwd, &checkpoint("rs", n)));
        if ts_tree != rs_tree {
            mismatches.push(format!("checkpoint tree {cwd} {n}: ts {ts_tree} rust {rs_tree}"));
        }
    }
    // Both sides write the same commit metadata.
    let meta = |reference: &str| git(&f.repo, &["log", "-1", "--format=%an <%ae>|%cn|%s", reference]);
    assert_eq!(meta(&checkpoint("ts", 2)).replace("/ts/", "/rs/"), meta(&checkpoint("rs", 2)));

    assert!(
        mismatches.is_empty(),
        "{} of {compared} comparisons differ:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
    eprintln!("golden: {compared} comparisons identical");
    let _ = Arc::new(());
}
