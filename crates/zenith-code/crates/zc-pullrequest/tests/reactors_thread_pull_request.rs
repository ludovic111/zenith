//! Port of `orchestration/ThreadPullRequestReactor.test.ts`. `TestClock.adjust("1 minute")`
//! is `tokio::time::advance` on a paused runtime; `Queue.take(reads)` waits on the fake
//! projections' read channel.

mod support_reactors;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use support_reactors::*;
use tokio_util::sync::CancellationToken;
use zc_contracts::{OrchestrationShellSnapshot, RepositoryIdentity};
use zc_ports::contracts as ports;
use zc_ports::git::GitBranchPullRequest;
use zc_ports::TaggedError;
use zc_pullrequest::reactors::{ThreadPullRequestDeps, ThreadPullRequestReactor, BACKFILL_ATTEMPTS, SWEEP_INTERVAL};

const NOW: &str = "2026-09-01T12:00:00.000Z";
const PROJECT_ID: &str = "project";
const REPOSITORY: &str = "owner/repository";
const REPOSITORY_KEY: &str = "github.com/owner/repository";

fn reference(number: i64) -> Value {
    json!({"projectId": PROJECT_ID, "repository": REPOSITORY, "number": number, "url": format!("https://github.com/{REPOSITORY}/pull/{number}")})
}

fn branch_pull_request(number: i64, state: &str) -> GitBranchPullRequest {
    let mut pull_request = reference(number);
    pull_request["title"] = json!("Branch pull request");
    pull_request["baseRef"] = json!("main");
    pull_request["headRef"] = json!("feature");
    pull_request["state"] = json!(state);
    pull_request.as_object_mut().unwrap().remove("projectId");
    pull_request.as_object_mut().unwrap().remove("repository");
    GitBranchPullRequest {
        pull_request: ports::VcsStatusPullRequest(pull_request),
        repository_key: Some(REPOSITORY_KEY.into()),
        updated_at: Some(NOW.into()),
        closed_at: None,
        merged_at: None,
    }
}

fn summary(input: &Value, state: &str) -> Value {
    merged(
        input.clone(),
        json!({
            "provider": "github", "title": "Pull request", "url": format!("https://github.com/{REPOSITORY}/pull/{}", input["number"]),
            "state": state, "headBranch": "feature", "baseBranch": "main", "updatedAt": NOW,
        }),
    )
}

fn thread(id: &str, overrides: Value) -> Value {
    merged(
        json!({
            "id": id, "projectId": PROJECT_ID, "title": id, "modelSelection": {"instanceId": "codex", "model": "gpt-5"},
            "runtimeMode": "full-access", "interactionMode": "default", "pullRequests": [], "branch": "feature", "worktreePath": null,
            "latestTurn": null, "createdAt": NOW, "updatedAt": NOW, "archivedAt": null, "settledOverride": null, "settledAt": null,
            "session": null, "latestUserMessageAt": NOW, "hasPendingApprovals": false, "hasPendingUserInput": false,
            "hasActionableProposedPlan": false,
        }),
        overrides,
    )
}

fn identity() -> Value {
    json!({
        "canonicalKey": REPOSITORY_KEY, "displayName": REPOSITORY, "rootPath": "/workspace/project",
        "locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": format!("git@github.com:{REPOSITORY}.git")},
    })
}

fn project() -> Value {
    json!({
        "id": PROJECT_ID, "title": "Project", "workspaceRoot": "/workspace/project", "repositoryIdentity": identity(),
        "defaultModelSelection": null, "scripts": [], "createdAt": NOW, "updatedAt": NOW,
    })
}

#[derive(Default)]
struct Options {
    threads: Vec<Value>,
    branch: Option<BranchHook>,
    summary: Option<RefHook<Value>>,
    existing_worktrees: Vec<String>,
    project: Option<Value>,
    resolve: Option<IdentityHook>,
    /// Serve full sweep reads from this instead of `threads`.
    shell: Option<ShellHook>,
}

struct Harness {
    reactor: ThreadPullRequestReactor,
    projections: Arc<Projections>,
    reads: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Option<String>>>,
    engine: Arc<Engine>,
    git: Arc<Git>,
    pull_requests: Arc<PullRequestFake>,
    activation: Latch,
}

/// The engine applies each sync to the snapshot, as the projection would.
fn apply_sync(projections: &Projections, command: &Value) -> Result<(), TaggedError> {
    if command["type"] != "thread.pull-request.sync" {
        return Err(TaggedError::new("Defect", format!("Unexpected command: {}", command["type"])));
    }
    projections.update(|snapshot| {
        snapshot.snapshot_sequence += 1;
        for thread in &mut snapshot.threads {
            if thread.id.as_str() == command["threadId"] {
                thread.branch_pull_request = Some(decode(command["branchPullRequest"].clone()));
                if let Some(linked) = command.get("linkedPullRequest") {
                    thread.linked_pull_request = Some(decode(linked.clone()));
                }
            }
        }
    });
    Ok(())
}

fn harness(options: Options) -> Harness {
    let project = options.project.unwrap_or_else(project);
    let snapshot: OrchestrationShellSnapshot =
        decode(json!({"snapshotSequence": 1, "projects": [project.clone()], "threads": options.threads, "updatedAt": NOW}));
    let (projections, reads) = Projections::new(snapshot, options.shell);
    let applied = projections.clone();
    let engine = Engine::new(Some(hook(move |command: Value| {
        let result = apply_sync(&applied, &command);
        async move { result }
    })));
    let git = Git::new(options.branch);
    let pull_requests = Arc::new(PullRequestFake {
        summary_hook: options.summary,
        default_summary: Some(Arc::new(|input: &Value| summary(input, "open"))),
        ..Default::default()
    });
    let project_identity = project["repositoryIdentity"].clone();
    let resolve = options.resolve.unwrap_or_else(|| {
        hook(move |_| {
            let identity: Option<RepositoryIdentity> = decode(project_identity.clone());
            async move { identity }
        })
    });
    let existing: HashSet<String> = options.existing_worktrees.into_iter().collect();
    let reactor = ThreadPullRequestReactor::new(
        ThreadPullRequestDeps {
            engine: engine.clone(),
            projections: projections.clone(),
            git: git.clone(),
            pull_requests: pull_requests.clone(),
            repository_identities: Arc::new(Identities(resolve)),
            uuids: counter_uuids(),
            path_exists: Arc::new(move |path: &str| existing.contains(path)),
            interval: SWEEP_INTERVAL,
        },
        CancellationToken::new(),
    );
    Harness {
        reactor,
        projections,
        reads,
        engine,
        git,
        pull_requests,
        activation: Latch::new(),
    }
}

impl Harness {
    async fn start(&self) {
        self.reactor.start_with_activation(Some(self.activation.activation())).await;
        self.activation.open();
        take(&self.reads).await;
        self.reactor.drain().await;
    }

    /// `TestClock.adjust("1 minute")`, then the periodic pass.
    async fn tick(&self) {
        tokio::time::advance(Duration::from_secs(60)).await;
        take(&self.reads).await;
        self.reactor.drain().await;
    }

    fn thread_field(&self, field: &str) -> Vec<Value> {
        encode(&self.projections.get())["threads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|thread| thread[field].clone())
            .collect()
    }

    fn commands(&self) -> Vec<Value> {
        self.engine.commands()
    }
}

fn branch_hook<F>(f: F) -> Option<BranchHook>
where
    F: Fn(&str, &str, bool) -> Result<Option<GitBranchPullRequest>, TaggedError> + Send + Sync + 'static,
{
    Some(hook(move |(cwd, branch, refresh): (String, String, bool)| {
        let result = f(&cwd, &branch, refresh);
        async move { result }
    }))
}

fn summary_hook<F>(f: F) -> Option<RefHook<Value>>
where
    F: Fn(&Value) -> Value + Send + Sync + 'static,
{
    Some(hook(move |input: Value| {
        let result = Ok(f(&input));
        async move { result }
    }))
}

fn session_set(thread_id: &str) -> Value {
    event(
        "thread.session-set",
        2,
        "turn-finished",
        thread_id,
        json!({"threadId": thread_id, "session": {
            "threadId": thread_id, "status": "ready", "providerName": "Codex", "runtimeMode": "full-access",
            "activeTurnId": null, "lastError": null, "updatedAt": NOW,
        }}),
    )
}

fn turn_diff_completed(thread_id: &str) -> Value {
    event(
        "thread.turn-diff-completed",
        3,
        "checkpoint-finished",
        thread_id,
        json!({
            "threadId": thread_id, "turnId": "turn", "checkpointTurnCount": 1, "checkpointRef": "checkpoint", "status": "ready",
            "files": [], "assistantMessageId": null, "completedAt": NOW,
        }),
    )
}

#[tokio::test(start_paused = true)]
async fn discovers_saved_branch_prs_without_a_client_and_shares_branch_lookups() {
    let fixture = harness(Options {
        threads: vec![
            thread("first", json!({})),
            thread("second", json!({})),
            thread("archived", json!({"archivedAt": NOW})),
            thread("no-branch", json!({"branch": null})),
        ],
        branch: branch_hook(|_, _, _| Ok(Some(branch_pull_request(42, "open")))),
        ..Default::default()
    });
    fixture.start().await;
    assert_eq!(
        fixture.git.calls(),
        vec![
            json!({"cwd": "/workspace/project", "branch": "feature", "refresh": false}),
            json!({"cwd": "/workspace/project", "branch": "feature", "refresh": false}),
        ]
    );
    let thread_ids: Vec<Value> = fixture.commands().iter().map(|command| command["threadId"].clone()).collect();
    assert_eq!(thread_ids, vec![json!("first"), json!("second")]);
    assert_eq!(fixture.thread_field("branchPullRequest")[0], reference(42));
    let command = &fixture.commands()[0];
    assert!(command["commandId"].as_str().unwrap().starts_with("server:thread-pull-request:first:"));
    assert_eq!(command["projectId"], PROJECT_ID);
    assert_eq!(command["snapshotSequence"], 1);
    assert_eq!(
        command["expected"],
        json!({"workspaceRoot": "/workspace/project", "branch": "feature", "worktreePath": null, "linkedPullRequest": null, "branchPullRequest": null})
    );
    assert!(command.get("linkedPullRequest").is_none());

    fixture.tick().await;
    assert_eq!(fixture.commands().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn replaces_terminal_manual_links_but_preserves_open_links_and_explicit_unlink() {
    let fixture = harness(Options {
        threads: vec![
            thread("merged", json!({"linkedPullRequest": reference(1)})),
            thread("closed", json!({"linkedPullRequest": reference(2)})),
            thread("open", json!({"linkedPullRequest": reference(3)})),
            thread("unlinked", json!({"linkedPullRequest": null})),
        ],
        branch: branch_hook(|_, _, _| Ok(Some(branch_pull_request(42, "open")))),
        summary: summary_hook(|input| {
            let state = match input["number"].as_i64() {
                Some(1) => "merged",
                Some(2) => "closed",
                _ => "open",
            };
            summary(input, state)
        }),
        ..Default::default()
    });
    fixture.start().await;
    assert_eq!(
        fixture.thread_field("linkedPullRequest"),
        vec![reference(42), reference(42), reference(3), Value::Null]
    );
    assert!(fixture.thread_field("branchPullRequest").iter().all(|link| link["number"] == 42));
}

async fn refreshes_discovery_once_after_a_turn_ends(session_first: bool) {
    let detected: Arc<Mutex<Option<GitBranchPullRequest>>> = Arc::new(Mutex::new(None));
    let state = detected.clone();
    let fixture = harness(Options {
        threads: vec![thread("turn-thread", json!({}))],
        branch: branch_hook(move |_, _, refresh| {
            let mut detected = state.lock().unwrap();
            if refresh {
                *detected = Some(branch_pull_request(42, "open"));
            }
            Ok(detected.clone())
        }),
        ..Default::default()
    });
    fixture.start().await;
    assert!(fixture.commands().is_empty());
    let events = if session_first {
        [session_set("turn-thread"), turn_diff_completed("turn-thread")]
    } else {
        [turn_diff_completed("turn-thread"), session_set("turn-thread")]
    };
    for event in events {
        fixture.engine.publish(event);
        assert_eq!(take(&fixture.reads).await.as_deref(), Some("turn-thread"));
        fixture.reactor.drain().await;
    }
    assert_eq!(fixture.commands()[0]["branchPullRequest"], reference(42));
    assert_eq!(fixture.git.calls().iter().filter(|call| call["refresh"] == true).count(), 1);
}

#[tokio::test(start_paused = true)]
async fn refreshes_discovery_once_after_a_turn_ends_with_session_first_event_ordering() {
    refreshes_discovery_once_after_a_turn_ends(true).await;
}

#[tokio::test(start_paused = true)]
async fn refreshes_discovery_once_after_a_turn_ends_with_checkpoint_first_event_ordering() {
    refreshes_discovery_once_after_a_turn_ends(false).await;
}

#[tokio::test(start_paused = true)]
async fn refreshes_the_project_identity_when_a_turn_adds_the_remote() {
    let fixture = harness(Options {
        threads: vec![thread("new-remote", json!({}))],
        project: Some(merged(project(), json!({"repositoryIdentity": null}))),
        branch: branch_hook(|_, _, _| Ok(Some(branch_pull_request(42, "open")))),
        resolve: Some(hook(|(_, refresh): (String, bool)| async move { refresh.then(|| decode(identity())) })),
        ..Default::default()
    });
    fixture.start().await;
    assert!(fixture.commands().is_empty());

    fixture.engine.publish(turn_diff_completed("new-remote"));
    take(&fixture.reads).await;
    fixture.reactor.drain().await;
    assert_eq!(fixture.commands()[0]["branchPullRequest"], reference(42));
}

#[tokio::test(start_paused = true)]
async fn uses_live_worktrees_and_falls_back_to_the_project_for_removed_worktrees() {
    let fixture = harness(Options {
        threads: vec![
            thread("live", json!({"worktreePath": "/workspace/worktree"})),
            thread("removed", json!({"worktreePath": "/workspace/removed"})),
        ],
        existing_worktrees: vec!["/workspace/worktree".into()],
        branch: branch_hook(|_, _, _| Ok(Some(branch_pull_request(42, "open")))),
        ..Default::default()
    });
    fixture.start().await;
    let cwds: HashSet<String> = fixture.git.calls().iter().map(|call| call["cwd"].as_str().unwrap().to_owned()).collect();
    assert_eq!(cwds, HashSet::from(["/workspace/project".to_owned(), "/workspace/worktree".to_owned()]));
}

#[tokio::test(start_paused = true)]
async fn retains_only_terminal_prs_on_shared_checkouts_and_clears_a_removed_branch() {
    let fixture = harness(Options {
        threads: vec![
            thread("terminal", json!({"branch": "main", "branchPullRequest": reference(1)})),
            thread("open", json!({"branchPullRequest": reference(2)})),
            thread("worktree", json!({"worktreePath": "/workspace/worktree", "branchPullRequest": reference(1)})),
            thread(
                "cleared",
                json!({"branch": null, "branchPullRequest": reference(1), "linkedPullRequest": reference(3)}),
            ),
        ],
        summary: summary_hook(|input| summary(input, if input["number"] == 1 { "merged" } else { "open" })),
        ..Default::default()
    });
    fixture.start().await;
    assert_eq!(
        fixture.thread_field("branchPullRequest"),
        vec![reference(1), Value::Null, Value::Null, Value::Null]
    );
    assert_eq!(fixture.thread_field("linkedPullRequest")[3], reference(3));
}

#[tokio::test(start_paused = true)]
async fn keeps_saved_links_on_lookup_failures_and_rejects_a_different_repository() {
    let fixture = harness(Options {
        threads: vec![
            thread("failed", json!({"branch": "failed", "branchPullRequest": reference(1)})),
            thread("wrong-repository", json!({"branch": "wrong-repository", "branchPullRequest": reference(2)})),
            thread("healthy", json!({})),
        ],
        branch: branch_hook(|cwd, branch, _| {
            if branch == "failed" {
                return Err(git_manager_error(cwd, "Lookup failed"));
            }
            let mut detected = branch_pull_request(42, "open");
            if branch == "wrong-repository" {
                detected.repository_key = Some("github.com/other/repository".into());
            }
            Ok(Some(detected))
        }),
        ..Default::default()
    });
    fixture.start().await;
    assert_eq!(fixture.thread_field("branchPullRequest"), vec![reference(1), reference(2), reference(42)]);
    assert_eq!(fixture.commands().len(), 1);
}

async fn retries_settled_backfills_and_stops_after_success(settled_override: Value) {
    let online = Arc::new(AtomicBool::new(false));
    let connected = online.clone();
    let fixture = harness(Options {
        threads: vec![
            thread("backfill", json!({"settledOverride": settled_override, "settledAt": NOW})),
            thread(
                "known",
                json!({"branch": "known", "settledOverride": settled_override, "settledAt": NOW, "branchPullRequest": reference(1)}),
            ),
        ],
        branch: branch_hook(move |cwd, _, _| {
            if connected.load(Ordering::SeqCst) {
                Ok(Some(branch_pull_request(42, "merged")))
            } else {
                Err(git_manager_error(cwd, "Offline"))
            }
        }),
        ..Default::default()
    });
    fixture.start().await;
    assert!(fixture.commands().is_empty());
    // A one-thread read cannot show that other pending threads are gone.
    fixture.engine.publish(event(
        "thread.unarchived",
        2,
        "gone-unarchived",
        "gone",
        json!({"threadId": "gone", "updatedAt": NOW}),
    ));
    assert_eq!(take(&fixture.reads).await.as_deref(), Some("gone"));
    fixture.reactor.drain().await;
    online.store(true, Ordering::SeqCst);
    fixture.tick().await;
    assert_eq!(fixture.commands()[0]["threadId"], "backfill");
    fixture.tick().await;
    let branches: Vec<Value> = fixture.git.calls().iter().map(|call| call["branch"].clone()).collect();
    assert_eq!(branches, vec![json!("feature"), json!("feature"), json!("feature")]);
}

#[tokio::test(start_paused = true)]
async fn retries_settled_backfills_and_stops_after_success_with_settled_override_settled() {
    retries_settled_backfills_and_stops_after_success(json!("settled")).await;
}

#[tokio::test(start_paused = true)]
async fn retries_settled_backfills_and_stops_after_success_with_settled_override_null() {
    retries_settled_backfills_and_stops_after_success(Value::Null).await;
}

#[tokio::test(start_paused = true)]
async fn stops_retrying_a_settled_backfill_after_repeated_lookup_failures() {
    let fixture = harness(Options {
        threads: vec![thread("backfill", json!({"settledOverride": "settled", "settledAt": NOW}))],
        branch: branch_hook(|cwd, _, _| Err(git_manager_error(cwd, "No gh"))),
        ..Default::default()
    });
    fixture.start().await;
    for _attempt in 1..BACKFILL_ATTEMPTS + 2 {
        fixture.tick().await;
    }
    assert_eq!(fixture.git.calls().len(), BACKFILL_ATTEMPTS as usize);
    assert!(fixture.commands().is_empty());
}

/// What one run of startup plus two periodic passes saw.
#[derive(Debug, PartialEq)]
struct Discovery {
    branch_calls: Vec<String>,
    commands: Vec<String>,
}

async fn discover(query: Arc<zc_projections::ProjectionSnapshotQuery>, honour_unsettled: bool) -> (Vec<Vec<String>>, Discovery) {
    let numbers: HashMap<&str, i64> = HashMap::from([("open", 1), ("resumed", 2), ("linked", 3), ("settled", 4), ("backfill", 5), ("archived", 6)]);
    let online = Arc::new(AtomicBool::new(false));
    let connected = online.clone();
    let reads: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let recorded = reads.clone();
    let fixture = harness(Options {
        threads: Vec::new(),
        shell: Some(hook(move |unsettled_only: bool| {
            let query = query.clone();
            let recorded = recorded.clone();
            async move {
                let snapshot = zc_ports::ProjectionReads::get_shell_snapshot(&*query, honour_unsettled && unsettled_only).await?;
                let mut ids: Vec<String> = snapshot.threads.iter().map(|thread| thread.id.to_string()).collect();
                ids.sort();
                recorded.lock().unwrap().push(ids);
                Ok(snapshot)
            }
        })),
        branch: branch_hook(move |cwd, branch, _| {
            if branch == "backfill" && !connected.load(Ordering::SeqCst) {
                return Err(git_manager_error(cwd, "Offline"));
            }
            Ok(numbers.get(branch).map(|number| branch_pull_request(*number, "open")))
        }),
        ..Default::default()
    });
    // Startup, then two periodic passes. The startup backfill lookup fails, so the first
    // periodic pass retries it from the full read.
    fixture.start().await;
    let startup_calls = fixture.git.calls().len();
    let startup_commands = fixture.commands().len();
    online.store(true, Ordering::SeqCst);
    for _pass in 0..2 {
        fixture.tick().await;
    }
    let mut branch_calls: Vec<String> = fixture.git.calls()[startup_calls..]
        .iter()
        .map(|call| call["branch"].as_str().unwrap().to_owned())
        .collect();
    branch_calls.sort();
    let mut commands: Vec<String> = fixture.commands()[startup_commands..]
        .iter()
        .map(|command| format!("{} {}", command["threadId"].as_str().unwrap(), command["branchPullRequest"]["number"]))
        .collect();
    commands.sort();
    let reads = reads.lock().unwrap().clone();
    (reads, Discovery { branch_calls, commands })
}

#[tokio::test(start_paused = true)]
async fn discovers_the_same_prs_from_the_unsettled_read_as_from_the_full_read() {
    let db = zc_db::Db::open_in_memory().unwrap();
    db.call(|conn| {
        let sql = |error| zc_db::DbError::sql("test", error);
        conn.execute_batch(&format!(
            r#"
            INSERT INTO projection_projects (project_id, title, workspace_root, scripts_json, created_at, updated_at)
            VALUES ('project', 'Project', '/workspace/project', '[]', '{NOW}', '{NOW}'),
              ('dormant', 'Dormant', '/workspace/dormant', '[]', '{NOW}', '{NOW}');
            INSERT INTO projection_threads (thread_id, project_id, title, model_selection_json, runtime_mode, interaction_mode, branch, branch_pull_request_json, created_at, updated_at, archived_at, settled_override, settled_at)
            VALUES
              ('open', 'project', 'Open', '{{"instanceId":"codex","model":"gpt-5"}}', 'full-access', 'default', 'open', NULL, '{NOW}', '{NOW}', NULL, NULL, NULL),
              ('resumed', 'project', 'Resumed', '{{"instanceId":"codex","model":"gpt-5"}}', 'full-access', 'default', 'resumed', NULL, '{NOW}', '{NOW}', NULL, 'active', NULL),
              ('linked', 'project', 'Linked', '{{"instanceId":"codex","model":"gpt-5"}}', 'full-access', 'default', 'linked', '{{"projectId":"project","repository":"owner/repository","number":3,"url":"https://github.com/owner/repository/pull/3"}}', '{NOW}', '{NOW}', NULL, NULL, NULL),
              ('settled', 'project', 'Settled', '{{"instanceId":"codex","model":"gpt-5"}}', 'full-access', 'default', 'settled', '{{"projectId":"project","repository":"owner/repository","number":4,"url":"https://github.com/owner/repository/pull/4"}}', '{NOW}', '{NOW}', NULL, 'settled', '{NOW}'),
              ('backfill', 'project', 'Backfill', '{{"instanceId":"codex","model":"gpt-5"}}', 'full-access', 'default', 'backfill', NULL, '{NOW}', '{NOW}', NULL, 'settled', '{NOW}'),
              ('imported', 'dormant', 'Imported', '{{"instanceId":"codex","model":"gpt-5"}}', 'full-access', 'default', NULL, NULL, '{NOW}', '{NOW}', NULL, 'settled', '{NOW}'),
              ('archived', 'project', 'Archived', '{{"instanceId":"codex","model":"gpt-5"}}', 'full-access', 'default', 'archived', NULL, '{NOW}', '{NOW}', '{NOW}', NULL, NULL);
            "#
        ))
        .map_err(sql)
    })
    .await
    .unwrap();
    let identity: RepositoryIdentity = decode(identity());
    let identities = zc_projections::FixedRepositoryIdentities(HashMap::from([
        ("/workspace/project".to_owned(), Some(identity.clone())),
        ("/workspace/dormant".to_owned(), Some(identity)),
    ]));
    let query = Arc::new(zc_projections::ProjectionSnapshotQuery::new(
        db,
        Arc::new(identities),
        Arc::new(zc_projections::NoThreadLiveState),
    ));

    let (unsettled_reads, unsettled) = discover(query.clone(), true).await;
    let (full_reads, full) = discover(query, false).await;
    assert_eq!(unsettled, full);
    assert_eq!(unsettled.commands, ["backfill 5", "open 1", "open 1", "resumed 2", "resumed 2"]);
    // The last pass has no backfill left, so it reads no settled thread.
    assert!(full_reads.last().unwrap().contains(&"imported".to_owned()));
    assert_eq!(unsettled_reads.last().unwrap(), &["linked", "open", "resumed"]);
}

#[tokio::test(start_paused = true)]
async fn matches_azure_ssh_projects_to_https_prs_with_the_provider_repository_selector() {
    let fixture = harness(Options {
        threads: vec![thread("azure", json!({}))],
        project: Some(merged(
            project(),
            json!({"repositoryIdentity": {
                "canonicalKey": "ssh.dev.azure.com/v3/org/project/repository", "displayName": "v3/org/project/repository",
                "name": "repository", "provider": "azure-devops", "rootPath": "/workspace/project",
                "locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": "git@ssh.dev.azure.com:v3/org/project/repository"},
            }}),
        )),
        branch: branch_hook(|_, _, _| {
            let mut detected = branch_pull_request(42, "open");
            detected.repository_key = Some("dev.azure.com/org/project/_git/repository".into());
            detected.pull_request.0["url"] = json!("https://dev.azure.com/org/project/_git/repository/pullrequest/42");
            Ok(Some(detected))
        }),
        ..Default::default()
    });
    fixture.start().await;
    assert_eq!(
        fixture.commands()[0]["branchPullRequest"],
        json!({"projectId": PROJECT_ID, "repository": "repository", "number": 42, "url": "https://dev.azure.com/org/project/_git/repository/pullrequest/42"})
    );
}

async fn rejects_the_groups_links_if_a_remote_changes_during_a_summary_read(primary: bool) {
    let current_identity = Arc::new(Mutex::new(identity()));
    let detected = Arc::new(Mutex::new(branch_pull_request(42, "open")));
    let (identity_state, detected_state) = (current_identity.clone(), detected.clone());
    let (identity_writer, detected_writer) = (current_identity.clone(), detected.clone());
    let fixture = harness(Options {
        threads: vec![thread("manual", json!({"linkedPullRequest": reference(1)})), thread("automatic", json!({}))],
        branch: branch_hook(move |_, _, _| Ok(Some(detected_state.lock().unwrap().clone()))),
        resolve: Some(hook(move |(_, refresh): (String, bool)| {
            let resolved = if refresh { identity_state.lock().unwrap().clone() } else { identity() };
            async move { Some(decode::<RepositoryIdentity>(resolved)) }
        })),
        summary: summary_hook(move |input| {
            if primary {
                *identity_writer.lock().unwrap() = merged(
                    identity(),
                    json!({"canonicalKey": "github.com/other/repository", "displayName": "other/repository"}),
                );
            } else {
                let mut moved = branch_pull_request(99, "open");
                moved.repository_key = Some("github.com/other/repository".into());
                moved.pull_request.0["url"] = json!("https://github.com/other/repository/pull/99");
                *detected_writer.lock().unwrap() = moved;
            }
            summary(input, "merged")
        }),
        ..Default::default()
    });
    fixture.start().await;
    assert!(fixture.commands().is_empty());
    assert_eq!(fixture.thread_field("linkedPullRequest")[0], reference(1));
    assert!(!fixture.pull_requests.summary_calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn rejects_the_groups_links_if_the_primary_remote_changes_during_a_summary_read() {
    rejects_the_groups_links_if_a_remote_changes_during_a_summary_read(true).await;
}

#[tokio::test(start_paused = true)]
async fn rejects_the_groups_links_if_the_branch_remote_changes_during_a_summary_read() {
    rejects_the_groups_links_if_a_remote_changes_during_a_summary_read(false).await;
}
