//! Ports of `vcs/GitVcsDriverCore.test.ts`: remotes, fetches, commit, push and pull.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::*;
use zc_ports::git::CreateWorktreeOptions;
use zc_vcs::contracts::*;
use zc_vcs::driver_core::{GitCommitOptions, GitPushStatus};
use zc_vcs::git_exec::ExecuteGitResult;

fn bare_remote() -> Tmp {
    let remote = Tmp::new("git-remote-");
    git(&remote.path, &["init", "--bare"]);
    remote
}

#[tokio::test]
async fn explains_a_real_fetch_failure_for_a_missing_local_remote() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    let missing = cwd.join("private-missing-remote");
    git(&cwd.path, &["remote", "add", "origin", &missing]);
    let (driver, _w) = driver();
    let error = driver.fetch_remote(cwd.str(), "origin", None).await.unwrap_err();
    assert!(error.detail.contains("could not access the remote repository"), "{}", error.detail);
    assert_eq!(error.exit_code, Some(128));
    assert!(error.stderr_length.unwrap() > 0);
    assert!(!error.detail.contains(&missing));
}

#[tokio::test]
async fn reports_fetch_failures_without_retaining_remote_output() {
    let scenarios = [
        ("fatal: Authentication failed for", "could not authenticate"),
        ("git@example.com: Permission denied (publickey).", "could not authenticate"),
        ("fatal: could not read Username: terminal prompts disabled", "could not authenticate"),
        ("fatal: Could not resolve host: example.com", "could not reach the remote"),
        ("ssh: connect to host example.com port 22: Connection refused", "could not reach the remote"),
        ("remote: Repository not found.", "could not access the remote repository"),
        ("fatal: remote does not appear to be a git repository", "could not access the remote repository"),
        (
            "error: cannot lock ref 'refs/remotes/origin/main': is at abc but expected def",
            "could not update a local reference",
        ),
        (
            "fatal: Unable to create '/repo/.git/FETCH_HEAD.lock': File exists.",
            "could not update a local reference",
        ),
        (
            "remote: Help: authentication failed, connection refused, cannot lock ref\nremote: unrelated service error",
            "git fetch origin failed",
        ),
        (
            "fatal: unable to access 'https://example.com/repo.git/': Could not resolve host: example.com",
            "could not reach the remote",
        ),
        ("fatal: unexpected remote failure", "git fetch origin failed"),
    ];
    for (stderr_head, expected) in scenarios {
        let secret = "secret-fetch-token";
        let stderr = format!("{stderr_head}\nhttps://user:{secret}@example.com/private?token={secret}");
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let reply = stderr.clone();
        let recorder = Recorder::responding(move |input| {
            if input.args.first().map(String::as_str) != Some("fetch") {
                return not_a_repository();
            }
            assert_eq!(input.args, ["fetch", "--quiet", "origin"]);
            let env = input.env.as_ref().unwrap();
            assert_eq!(env.get("LC_ALL").cloned().flatten().as_deref(), Some("C"));
            assert_eq!(env.get("GIT_TERMINAL_PROMPT").cloned().flatten().as_deref(), Some("0"));
            counter.fetch_add(1, Ordering::SeqCst);
            Some(Ok(ExecuteGitResult {
                exit_code: 128,
                stdout: secret.into(),
                stderr: reply.clone(),
                ..ExecuteGitResult::default()
            }))
        });
        let cwd = Tmp::new("git-vcs-driver-test-");
        let (driver, _w) = driver_with(recorder);
        let error = driver.fetch_remote(cwd.str(), "origin", None).await.unwrap_err();
        assert!(error.detail.to_lowercase().contains(expected), "{stderr_head}: {}", error.detail);
        assert_eq!(error.exit_code, Some(128));
        assert_eq!(error.stderr_length, Some(stderr.len()));
        assert_eq!(error.stdout_length, Some(secret.len()));
        assert!(!error.to_string().contains(secret));
        assert!(!serde_json::to_string(&error).unwrap().contains(secret));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn does_not_retry_a_scoped_fetch_after_a_network_or_auth_failure() {
    for (stderr, expected) in [
        (
            "fatal: Could not resolve host",
            "Git could not reach the remote. Check the server's network connection and remote host, then retry.",
        ),
        (
            "fatal: Authentication failed",
            "Git could not authenticate with the remote. Check Git credentials or SSH access on the server, then retry.",
        ),
    ] {
        let attempts = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let sink = attempts.clone();
        let recorder = Recorder::responding(move |input| {
            if input.args.first().map(String::as_str) != Some("fetch") {
                return not_a_repository();
            }
            sink.lock().unwrap().push(input.args.clone());
            Some(Ok(ExecuteGitResult {
                exit_code: 128,
                stderr: stderr.into(),
                ..ExecuteGitResult::default()
            }))
        });
        let cwd = Tmp::new("git-vcs-driver-test-");
        let (driver, _w) = driver_with(recorder);
        let error = driver.fetch_remote(cwd.str(), "origin", Some("main")).await.unwrap_err();
        assert_eq!(error.detail, expected);
        let attempts = attempts.lock().unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0], ["fetch", "--quiet", "origin", "+refs/heads/main:refs/remotes/origin/main"]);
    }
}

#[tokio::test]
async fn ensure_remote_reuses_an_existing_remote_across_transports() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["remote", "add", "origin", "https://github.com/pingdotgg/t3code.git"]);
    let (driver, _w) = driver();
    for url in [
        "git@github.com:pingdotgg/t3code.git",
        "ssh://git@github.com/pingdotgg/t3code",
        "ssh://github.com/pingdotgg/t3code",
        "ssh://git@github.com:22/pingdotgg/t3code",
        "ssh://git@github.com:22/pingdotgg/t3code.git",
    ] {
        assert_eq!(driver.ensure_remote(cwd.str(), "pingdotgg", url).await.unwrap(), "origin", "{url}");
    }
    let added = driver.ensure_remote(cwd.str(), "octocat", "git@github.com:octocat/t3code.git").await.unwrap();
    assert_eq!(added, "octocat");
    assert_eq!(git(&cwd.path, &["remote"]), "octocat\norigin");
    let suffixed = driver.ensure_remote(cwd.str(), "octocat", "git@github.com:other/t3code.git").await.unwrap();
    assert_eq!(suffixed, "octocat-1");
}

#[tokio::test]
async fn stages_selected_files_and_commits_only_those() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    write(&cwd.path, "a.txt", "a\n");
    write(&cwd.path, "b.txt", "b\n");
    let (driver, _w) = driver();
    let context = driver.prepare_commit_context(cwd.str(), Some(&["a.txt".to_owned()])).await.unwrap().unwrap();
    assert!(context.staged_summary.contains("a.txt"));
    assert!(!context.staged_summary.contains("b.txt"));
    assert!(context.staged_patch.contains("+a"));
    let sha = driver.commit(cwd.str(), "Add a", "", GitCommitOptions::default()).await.unwrap();
    assert!(sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(git(&cwd.path, &["log", "-1", "--pretty=%s"]), "Add a");
    let status = git(&cwd.path, &["status", "--porcelain"]);
    assert!(status.contains("?? b.txt"));
    assert!(!status.contains("a.txt"));
}

#[tokio::test]
async fn treats_selected_file_paths_literally_and_skips_empty_commits() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    let (driver, _w) = driver();
    assert!(driver.prepare_commit_context(cwd.str(), None).await.unwrap().is_none());
    write(&cwd.path, "selected[1].txt", "literal\n");
    write(&cwd.path, "selected1.txt", "pattern match\n");
    driver.prepare_commit_context(cwd.str(), Some(&["selected[1].txt".to_owned()])).await.unwrap();
    assert_eq!(git(&cwd.path, &["diff", "--cached", "--name-only"]), "selected[1].txt");
    assert!(git(&cwd.path, &["status", "--porcelain"]).contains("?? selected1.txt"));
}

#[tokio::test]
async fn creates_a_worktree_from_the_latest_fetched_remote_commit() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let remote = bare_remote();
    let peer = Tmp::new("git-peer-");
    let branch = init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["remote", "add", "origin", remote.str()]);
    git(&cwd.path, &["push", "-u", "origin", &branch]);
    git(&remote.path, &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")]);
    let before = git(&cwd.path, &["rev-parse", &format!("refs/remotes/origin/{branch}")]);
    git(&peer.path, &["clone", remote.str(), "."]);
    git(&peer.path, &["config", "user.email", "test@test.com"]);
    git(&peer.path, &["config", "user.name", "Test"]);
    write(&peer.path, "remote-change.txt", "remote\n");
    commit_all(&peer.path, "remote change");
    git(&peer.path, &["push", "origin", &branch]);
    let remote_head = git(&peer.path, &["rev-parse", "HEAD"]);
    assert_ne!(before, remote_head);
    git(&peer.path, &["push", "origin", "HEAD:refs/heads/unrelated"]);

    let (driver, _w) = driver();
    driver.fetch_remote(cwd.str(), "origin", Some(&format!("origin/{branch}"))).await.unwrap();
    assert!(!driver.remote_branch_exists(cwd.str(), "origin", "unrelated").await.unwrap());
    assert!(driver.remote_branch_exists(cwd.str(), "origin", &branch).await.unwrap());
    assert!(!driver.remote_branch_exists(cwd.str(), "origin", "local-only").await.unwrap());
    let resolved = driver.resolve_remote_tracking_commit(cwd.str(), &branch, "origin").await.unwrap();
    let explicit = driver
        .resolve_remote_tracking_commit(cwd.str(), &format!("origin/{branch}"), "origin")
        .await
        .unwrap();
    assert_eq!(resolved.commit_sha, remote_head);
    assert_eq!(resolved.remote_ref_name, format!("origin/{branch}"));
    assert_eq!(explicit, resolved);
    assert_eq!(git(&cwd.path, &["rev-parse", &branch]), before);

    let parent = Tmp::new("git-fetched-worktrees-");
    let worktree = parent.join("fetched-origin");
    driver
        .create_worktree(
            &VcsCreateWorktreeInput {
                cwd: cwd.str().into(),
                ref_name: resolved.commit_sha.clone(),
                new_ref_name: Some("t3code/fetched-origin".into()),
                base_ref_name: Some(resolved.remote_ref_name.clone()),
                path: Some(worktree.clone()),
            },
            &CreateWorktreeOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), remote_head);
    assert_eq!(
        driver
            .read_config_value(&worktree, "branch.t3code/fetched-origin.gh-merge-base")
            .await
            .unwrap()
            .as_deref(),
        Some(branch.as_str())
    );
    assert_eq!(driver.read_config_value(&worktree, "branch.t3code/fetched-origin.remote").await.unwrap(), None);
    let status = driver.status_details(&worktree).await.unwrap();
    assert_eq!(status.ahead_count, 0);
    assert_eq!(status.ahead_of_default_count, 0);

    // A missing remote branch falls back to fetching the whole remote.
    driver.fetch_remote(cwd.str(), "origin", Some("local-only")).await.unwrap();
    assert!(driver.remote_branch_exists(cwd.str(), "origin", "unrelated").await.unwrap());
}

#[tokio::test]
async fn pushes_with_upstream_setup_and_skips_when_up_to_date() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let remote = bare_remote();
    init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["remote", "add", "origin", remote.str()]);
    let (driver, _w) = driver();
    driver
        .create_ref(&VcsCreateRefInput {
            cwd: cwd.str().into(),
            ref_name: "feature/push".into(),
            switch_ref: Some(true),
        })
        .await
        .unwrap();
    write(&cwd.path, "feature.txt", "feature\n");
    driver.prepare_commit_context(cwd.str(), None).await.unwrap();
    driver.commit(cwd.str(), "Add feature", "", GitCommitOptions::default()).await.unwrap();
    let pushed = driver.push_current_branch(cwd.str(), None, None).await.unwrap();
    assert_eq!(pushed.status, GitPushStatus::Pushed);
    assert_eq!(pushed.branch, "feature/push");
    assert_eq!(pushed.set_upstream, Some(true));
    assert_eq!(git(&cwd.path, &["rev-parse", "--abbrev-ref", "@{upstream}"]), "origin/feature/push");
    let skipped = driver.push_current_branch(cwd.str(), None, None).await.unwrap();
    assert_eq!(skipped.status, GitPushStatus::SkippedUpToDate);
    assert_eq!(skipped.branch, "feature/push");
}

#[tokio::test]
async fn pushes_upstream_branches_to_the_remote_branch_name() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let remote = bare_remote();
    init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["branch", "-M", "main"]);
    git(&cwd.path, &["remote", "add", "origin", remote.str()]);
    git(&cwd.path, &["push", "-u", "origin", "main"]);
    write(&cwd.path, "upstream.txt", "upstream\n");
    let (driver, _w) = driver();
    driver.prepare_commit_context(cwd.str(), None).await.unwrap();
    driver.commit(cwd.str(), "Add upstream update", "", GitCommitOptions::default()).await.unwrap();
    let pushed = driver.push_current_branch(cwd.str(), None, None).await.unwrap();
    assert_eq!(pushed.status, GitPushStatus::Pushed);
    assert_eq!(pushed.branch, "main");
    assert_eq!(pushed.upstream_branch.as_deref(), Some("origin/main"));
    assert_eq!(pushed.set_upstream, Some(false));
    assert_eq!(git(&remote.path, &["log", "-1", "--pretty=%s", "main"]), "Add upstream update");
    assert_ne!(git_status(&remote.path, &["show-ref", "--verify", "--quiet", "refs/heads/origin/main"]), 0);
}

#[tokio::test]
async fn publishes_a_branch_tracking_its_base_under_its_own_name() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let remote = bare_remote();
    init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["branch", "-M", "main"]);
    git(&cwd.path, &["remote", "add", "origin", remote.str()]);
    git(&cwd.path, &["push", "-u", "origin", "main"]);
    git(&cwd.path, &["checkout", "-b", "dev"]);
    git(&cwd.path, &["push", "-u", "origin", "dev"]);
    let dev_sha = git(&cwd.path, &["rev-parse", "HEAD"]);
    git(&cwd.path, &["checkout", "-b", "feature/x", "origin/dev"]);
    write(&cwd.path, "feature.txt", "feature\n");
    let (driver, _w) = driver();
    driver.prepare_commit_context(cwd.str(), None).await.unwrap();
    driver.commit(cwd.str(), "Add feature", "", GitCommitOptions::default()).await.unwrap();
    let pushed = driver.push_current_branch(cwd.str(), None, None).await.unwrap();
    assert_eq!(pushed.status, GitPushStatus::Pushed);
    assert_eq!(pushed.upstream_branch.as_deref(), Some("origin/feature/x"));
    assert_eq!(pushed.set_upstream, Some(true));
    assert_eq!(git(&remote.path, &["log", "-1", "--pretty=%s", "feature/x"]), "Add feature");
    assert_eq!(git(&remote.path, &["rev-parse", "dev"]), dev_sha);
    assert_eq!(git(&cwd.path, &["rev-parse", "--abbrev-ref", "@{upstream}"]), "origin/feature/x");
    assert_eq!(
        driver.read_config_value(cwd.str(), "branch.feature/x.gh-merge-base").await.unwrap().as_deref(),
        Some("dev")
    );
}

#[tokio::test]
async fn keeps_a_recorded_merge_base_when_publishing_a_tracked_branch() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let remote = bare_remote();
    init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["branch", "-M", "main"]);
    git(&cwd.path, &["remote", "add", "origin", remote.str()]);
    git(&cwd.path, &["push", "-u", "origin", "main"]);
    git(&cwd.path, &["checkout", "-b", "feature/y", "origin/main"]);
    git(&cwd.path, &["config", "branch.feature/y.gh-merge-base", "release/v2"]);
    write(&cwd.path, "feature.txt", "feature\n");
    let (driver, _w) = driver();
    driver.prepare_commit_context(cwd.str(), None).await.unwrap();
    driver.commit(cwd.str(), "Add feature", "", GitCommitOptions::default()).await.unwrap();
    let pushed = driver.push_current_branch(cwd.str(), None, None).await.unwrap();
    assert_eq!(pushed.upstream_branch.as_deref(), Some("origin/feature/y"));
    assert_eq!(pushed.set_upstream, Some(true));
    assert_eq!(
        driver.read_config_value(cwd.str(), "branch.feature/y.gh-merge-base").await.unwrap().as_deref(),
        Some("release/v2")
    );
}

#[tokio::test]
async fn still_pushes_a_git_mangled_tracking_alias_to_its_upstream_head() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let remote = bare_remote();
    init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["branch", "-M", "main"]);
    git(&cwd.path, &["remote", "add", "my-org/upstream", remote.str()]);
    git(&cwd.path, &["push", "my-org/upstream", "main:effect-atom"]);
    git(&cwd.path, &["fetch", "my-org/upstream"]);
    git(&cwd.path, &["checkout", "--track", "my-org/upstream/effect-atom"]);
    assert_eq!(git(&cwd.path, &["rev-parse", "--abbrev-ref", "HEAD"]), "upstream/effect-atom");
    write(&cwd.path, "alias.txt", "alias\n");
    let (driver, _w) = driver();
    driver.prepare_commit_context(cwd.str(), None).await.unwrap();
    driver.commit(cwd.str(), "Add alias update", "", GitCommitOptions::default()).await.unwrap();
    let pushed = driver.push_current_branch(cwd.str(), None, None).await.unwrap();
    assert_eq!(pushed.branch, "upstream/effect-atom");
    assert_eq!(pushed.upstream_branch.as_deref(), Some("my-org/upstream/effect-atom"));
    assert_eq!(pushed.set_upstream, Some(false));
    assert_eq!(git(&remote.path, &["log", "-1", "--pretty=%s", "effect-atom"]), "Add alias update");
}

#[tokio::test]
async fn pushes_to_the_requested_remote_instead_of_the_primary_remote() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let origin = bare_remote();
    let publish = bare_remote();
    init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["branch", "-M", "main"]);
    git(&cwd.path, &["remote", "add", "origin", origin.str()]);
    git(&cwd.path, &["remote", "add", "origin-1", publish.str()]);
    let (driver, _w) = driver();
    let pushed = driver.push_current_branch(cwd.str(), None, Some("origin-1")).await.unwrap();
    assert_eq!(pushed.branch, "main");
    assert_eq!(pushed.upstream_branch.as_deref(), Some("origin-1/main"));
    assert_eq!(pushed.set_upstream, Some(true));
    assert_eq!(git(&publish.path, &["log", "-1", "--pretty=%s", "main"]), "initial commit");
    assert_ne!(git_status(&origin.path, &["show-ref", "--verify", "--quiet", "refs/heads/main"]), 0);
}

#[tokio::test]
async fn pulls_fast_forward_and_reports_up_to_date() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let remote = bare_remote();
    let peer = Tmp::new("git-peer-");
    let branch = init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["remote", "add", "origin", remote.str()]);
    git(&cwd.path, &["push", "-u", "origin", &branch]);
    let (driver, _w) = driver();
    let up_to_date = driver.pull_current_branch(cwd.str()).await.unwrap();
    assert_eq!(up_to_date.status, VcsPullStatus::SkippedUpToDate);
    assert_eq!(up_to_date.upstream_ref.as_deref(), Some(format!("origin/{branch}").as_str()));
    git(&peer.path, &["clone", remote.str(), "."]);
    git(&peer.path, &["config", "user.email", "test@test.com"]);
    git(&peer.path, &["config", "user.name", "Test"]);
    git(&peer.path, &["checkout", &branch]);
    write(&peer.path, "peer.txt", "peer\n");
    commit_all(&peer.path, "peer change");
    git(&peer.path, &["push", "origin", &branch]);
    let pulled = driver.pull_current_branch(cwd.str()).await.unwrap();
    assert_eq!(pulled.status, VcsPullStatus::Pulled);
    assert_eq!(pulled.ref_name, branch);
    assert_eq!(read(&cwd.path, "peer.txt"), "peer\n");
}

#[tokio::test]
async fn refuses_to_pull_without_an_upstream() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    let (driver, _w) = driver();
    let error = driver.pull_current_branch(cwd.str()).await.unwrap_err();
    assert_eq!(error.detail, "Current branch has no upstream configured. Push with upstream first.");
    assert_eq!(error.operation, "GitVcsDriver.pullCurrentBranch");
}

#[tokio::test]
async fn refreshes_a_checked_out_branch_onto_a_target_commit() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    let base = git(&cwd.path, &["rev-parse", "HEAD"]);
    write(&cwd.path, "next.txt", "next\n");
    commit_all(&cwd.path, "next");
    let next = git(&cwd.path, &["rev-parse", "HEAD"]);
    git(&cwd.path, &["reset", "--hard", &base]);
    let (driver, _w) = driver();
    let moved = driver.refresh_checked_out_branch(cwd.str(), &next, None).await.unwrap();
    assert!(moved.moved && moved.on_target);
    assert_eq!(git(&cwd.path, &["rev-parse", "HEAD"]), next);
    // A rewritten head is only taken when HEAD sits on the allowed commit.
    git(&cwd.path, &["reset", "--hard", &base]);
    write(&cwd.path, "other.txt", "other\n");
    commit_all(&cwd.path, "other");
    let other = git(&cwd.path, &["rev-parse", "HEAD"]);
    let refused = driver.refresh_checked_out_branch(cwd.str(), &next, None).await.unwrap();
    assert!(!refused.moved && !refused.on_target);
    let reset = driver.refresh_checked_out_branch(cwd.str(), &next, Some(&other)).await.unwrap();
    assert!(reset.moved);
    assert_eq!(git(&cwd.path, &["rev-parse", "refs/t3code/pre-refresh"]), other);
}

#[tokio::test]
async fn lists_local_branch_names_and_resolves_commits() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["branch", "feature/b"]);
    let (driver, _w) = driver();
    let mut names = driver.list_local_branch_names(cwd.str()).await.unwrap();
    names.sort();
    let mut expected = vec![branch.clone(), "feature/b".to_owned()];
    expected.sort();
    assert_eq!(names, expected);
    assert_eq!(driver.resolve_commit(cwd.str(), "HEAD").await.unwrap(), git(&cwd.path, &["rev-parse", "HEAD"]));
    let range = driver.read_range_context(cwd.str(), &branch).await.unwrap();
    assert_eq!(range.commit_summary, "");
}
