//! Golden comparison with the TS server's bootstrap: the scenarios of
//! `golden/bootstrap_oracle.ts.txt` run through the real `ws.ts` route (a scratch copy of
//! `server.test.ts` with the scenarios appended, run by vitest) and through
//! [`BootstrapDispatcher`] with the same scripted engine, git and setup runner. The dispatched
//! commands (wire JSON) and the result must match, with UUIDs and timestamps normalized.
//!
//! Needs `code/apps/server/node_modules` (symlinked from a checkout that has them); without
//! them the test prints why and passes vacuously.

use super::*;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn server_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../code/apps/server")
}

/// The scratch copy of `server.test.ts` with the golden scenarios appended; removed on drop.
struct ScratchTest(PathBuf);

impl Drop for ScratchTest {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Runs the TS scenarios with vitest, writing `<out>/<scenario>.json`; `None` when the TS
/// toolchain is not available here.
fn run_ts(out: &Path) -> Option<()> {
    let server = std::fs::canonicalize(server_dir()).ok()?;
    let vp = server.join("node_modules/.bin/vp");
    if !vp.exists() {
        eprintln!("skipping the TS bootstrap golden: {} has no node_modules", server.display());
        return None;
    }
    let mut source = std::fs::read_to_string(server.join("src/server.test.ts")).ok()?;
    source.push_str(&std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/bootstrap_oracle.ts.txt")).unwrap());
    let scratch = ScratchTest(server.join("src/zc-golden-bootstrap.test.ts"));
    std::fs::write(&scratch.0, source).unwrap();
    let mut child = Command::new(&vp)
        .args(["test", "run", "src/zc-golden-bootstrap.test.ts", "-t", "zc golden", "--testTimeout", "30000"])
        .current_dir(&server)
        .env("ZC_GOLDEN_OUT", out)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let output = child.wait_with_output().unwrap();
            assert!(status.success(), "the TS golden scenarios failed: {}", String::from_utf8_lossy(&output.stdout));
            return Some(());
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!("the TS golden scenarios timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

struct Scenario {
    name: &'static str,
    is_repository: bool,
    has_commit: bool,
    create_ok: bool,
    prepare: bool,
    run_setup_script: bool,
    branch: Option<&'static str>,
    setup: fn() -> Setup,
    fail_setup_activity: Option<usize>,
    fail_thread_delete: bool,
}

#[allow(clippy::too_many_arguments)]
fn scenario(
    name: &'static str,
    is_repository: bool,
    has_commit: bool,
    create_ok: bool,
    prepare: bool,
    run_setup_script: bool,
    branch: Option<&'static str>,
    setup: fn() -> Setup,
) -> Scenario {
    Scenario {
        name,
        is_repository,
        has_commit,
        create_ok,
        prepare,
        run_setup_script,
        branch,
        setup,
        fail_setup_activity: None,
        fail_thread_delete: false,
    }
}

/// The same table as `zcScenarios` in the TS block.
fn scenarios() -> Vec<Scenario> {
    vec![
        scenario("non-repo", false, true, false, true, true, None, || Setup::None),
        scenario("no-commit", true, false, false, true, true, Some("main"), || Setup::None),
        scenario("setup-failure", true, true, true, true, true, Some("main"), || Setup::Fails),
        Scenario {
            fail_setup_activity: Some(2),
            ..scenario("setup-activity-failure", true, true, true, true, true, Some("main"), || Setup::Started {
                r#async: true,
                exit: None,
            })
        },
        scenario("worktree-defect", true, true, false, true, false, Some("main"), || Setup::None),
        Scenario {
            fail_thread_delete: true,
            ..scenario("cleanup-failure", true, true, false, true, false, Some("main"), || Setup::None)
        },
        scenario("create-only", true, true, true, false, false, None, || Setup::None),
    ]
}

fn command(scenario: &Scenario) -> ThreadTurnStartCommand {
    let model = json!({"instanceId": "codex", "model": "gpt-5-codex"});
    let mut bootstrap = json!({
        "createThread": {"projectId": "project-default", "title": "Bootstrap Thread", "modelSelection": model, "runtimeMode": "full-access",
                         "interactionMode": "default", "branch": scenario.branch, "worktreePath": null, "createdAt": NOW},
        "runSetupScript": scenario.run_setup_script,
    });
    if scenario.prepare {
        bootstrap["prepareWorktree"] = json!({"projectCwd": "/tmp/project", "baseBranch": "main", "branch": "t3code/bootstrap-refName"});
    }
    decode(json!({
        "type": "thread.turn.start", "commandId": format!("cmd-golden-{}", scenario.name), "threadId": format!("thread-golden-{}", scenario.name),
        "message": {"messageId": format!("msg-golden-{}", scenario.name), "role": "user", "text": "hello", "attachments": []},
        "modelSelection": model, "runtimeMode": "full-access", "interactionMode": "default", "bootstrap": bootstrap, "createdAt": NOW,
    }))
}

/// UUIDs and timestamps differ between runs (the TS route also re-stamps `createdAt` with its
/// test clock before the bootstrap sees the command).
fn normalize(value: &mut Value) {
    let uuid = Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
    let iso = Regex::new(r"^-?\d{4,6}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$").unwrap();
    fn walk(value: &mut Value, uuid: &Regex, iso: &Regex) {
        match value {
            Value::String(text) => {
                let replaced = uuid.replace_all(text, "<uuid>").into_owned();
                *text = if iso.is_match(&replaced) { "<time>".into() } else { replaced };
            }
            Value::Array(items) => items.iter_mut().for_each(|item| walk(item, uuid, iso)),
            Value::Object(map) => map.values_mut().for_each(|item| walk(item, uuid, iso)),
            _ => {}
        }
    }
    walk(value, &uuid, &iso);
}

#[tokio::test(flavor = "multi_thread")]
async fn bootstrap_sequences_match_the_typescript_server() {
    let (_dir, out) = temp_dir();
    let ran = tokio::task::spawn_blocking({
        let out = out.clone();
        move || run_ts(&out)
    })
    .await
    .unwrap();
    if ran.is_none() {
        return;
    }
    for scenario in scenarios() {
        let git = ScriptedGit::new(
            scenario.is_repository,
            scenario.has_commit,
            if scenario.create_ok { Ok(()) } else { Err("worktree exploded".into()) },
        );
        git.progress.store(false, std::sync::atomic::Ordering::SeqCst);
        let f = fixture(git, (scenario.setup)()).await;
        *f.engine.fail_setup_activity.lock().unwrap() = scenario.fail_setup_activity;
        if scenario.fail_thread_delete {
            f.engine.fail_types.lock().unwrap().push("thread.delete".into());
        }
        let result = match f.dispatcher.dispatch_bootstrap_turn_start(command(&scenario)).await {
            Ok(result) => json!({"ok": {"sequence": result.sequence}}),
            Err(error) => json!({"error": {"message": error.message, "bootstrapThreadDisposition": error.bootstrap_thread_disposition}}),
        };
        let mut actual = json!({"commands": f.engine.commands(), "result": result});
        let mut expected: Value = serde_json::from_str(&std::fs::read_to_string(out.join(format!("{}.json", scenario.name))).unwrap()).unwrap();
        normalize(&mut actual);
        normalize(&mut expected);
        let (want, got) = (expected["commands"].as_array().unwrap(), actual["commands"].as_array().unwrap());
        for (index, (w, g)) in want.iter().zip(got).enumerate() {
            assert_eq!(g, w, "{}: command #{index}", scenario.name);
        }
        assert_eq!(got.len(), want.len(), "{}: command count", scenario.name);
        assert_eq!(actual["result"], expected["result"], "{}: result", scenario.name);
        eprintln!("{}: {} commands match", scenario.name, got.len());
    }
}
