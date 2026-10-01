//! Port of `project/RepositoryIdentityResolver.test.ts`.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use zc_contracts::RepositoryIdentity;
use zc_core::process::{ProcessRunError, ProcessRunInput, ProcessRunOutput, ProcessRunner};
use zc_project::{RepositoryIdentities, RepositoryIdentityOptions, RepositoryIdentityRefiner};

/// A scripted `git`: answers `rev-parse` with the current root and `remote -v` with the current
/// remote, recording every call.
#[derive(Default)]
struct Script {
    calls: Mutex<Vec<Vec<String>>>,
    root: Mutex<String>,
    remote: Mutex<String>,
    root_failures: Mutex<usize>,
}

struct ScriptedRunner(Arc<Script>);

#[async_trait]
impl ProcessRunner for ScriptedRunner {
    async fn run(&self, input: ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
        let script = &self.0;
        script.calls.lock().unwrap().push(input.args.clone());
        let root_lookup = input.args.iter().any(|arg| arg == "rev-parse");
        let mut failures = script.root_failures.lock().unwrap();
        if root_lookup && *failures > 0 {
            *failures -= 1;
            return Ok(ProcessRunOutput {
                stderr: "temporary Git failure".into(),
                code: Some(1),
                ..ProcessRunOutput::default()
            });
        }
        let stdout = if root_lookup {
            format!("{}\n", script.root.lock().unwrap())
        } else {
            format!("origin\t{} (fetch)\n", script.remote.lock().unwrap())
        };
        Ok(ProcessRunOutput {
            stdout,
            code: Some(0),
            ..ProcessRunOutput::default()
        })
    }
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

struct TestRefiner {
    refinements: Arc<Mutex<usize>>,
    fails: Arc<Mutex<bool>>,
}

#[async_trait]
impl RepositoryIdentityRefiner for TestRefiner {
    async fn refine(&self, identity: RepositoryIdentity) -> Result<RepositoryIdentity, String> {
        *self.refinements.lock().unwrap() += 1;
        if *self.fails.lock().unwrap() {
            return Err("account unavailable".into());
        }
        Ok(if identity.canonical_key.starts_with("ssh.forge.test/") {
            RepositoryIdentity {
                provider: Some("forgejo".into()),
                web_url: Some("http://forge.test:3000/git/team/repo".into()),
                ..identity
            }
        } else {
            identity
        })
    }
}

#[tokio::test(start_paused = true)]
async fn refreshes_the_git_root_only_when_requested() {
    let script = Arc::new(Script::default());
    *script.root.lock().unwrap() = "/repo".into();
    *script.remote.lock().unwrap() = "git@github.com:Octo-Org/sample-app.git".into();
    let refinements = Arc::new(Mutex::new(0));
    let fails = Arc::new(Mutex::new(false));
    let resolver = RepositoryIdentities::new(RepositoryIdentityOptions {
        runner: Arc::new(ScriptedRunner(script.clone())),
        refine: Some(Arc::new(TestRefiner {
            refinements: refinements.clone(),
            fails: fails.clone(),
        })),
        ..RepositoryIdentityOptions::default()
    });

    let first = resolver.resolve("/repo/packages/web", false).await;
    *script.root.lock().unwrap() = "/repo/packages/web".into();
    // Longer than the one-minute cadence of the background sweeps.
    tokio::time::advance(Duration::from_secs(600)).await;
    let second = resolver.resolve("/repo/packages/web", false).await;
    assert_eq!(first.as_ref().unwrap().canonical_key, "github.com/octo-org/sample-app");
    assert_eq!(second, first);
    assert_eq!(*refinements.lock().unwrap(), 1);
    assert_eq!(
        *script.calls.lock().unwrap(),
        vec![
            args(&["-C", "/repo/packages/web", "rev-parse", "--show-toplevel"]),
            args(&["-C", "/repo", "remote", "-v"])
        ]
    );

    let refreshed = resolver.resolve("/repo/packages/web", true).await;
    assert_eq!(refreshed.as_ref().unwrap().root_path.as_deref(), Some("/repo/packages/web"));
    assert_eq!(resolver.resolve("/repo/packages/web", false).await, refreshed);
    assert_eq!(
        script.calls.lock().unwrap()[2..].to_vec(),
        vec![
            args(&["-C", "/repo/packages/web", "rev-parse", "--show-toplevel"]),
            args(&["-C", "/repo/packages/web", "remote", "-v"])
        ]
    );
    *script.remote.lock().unwrap() = "git@ssh.forge.test:team/repo.git".into();
    let root = "/repo/packages/web";
    let forgejo = resolver.resolve(root, true).await.unwrap();
    assert_eq!(forgejo.web_url.as_deref(), Some("http://forge.test:3000/git/team/repo"));
    assert_eq!(forgejo.provider.as_deref(), Some("forgejo"));
    assert_eq!(forgejo.canonical_key, "ssh.forge.test/team/repo");
    assert_eq!(forgejo.locator.remote_url, "git@ssh.forge.test:team/repo.git");
    assert_eq!(resolver.resolve(root, false).await.as_ref(), Some(&forgejo));
    assert_eq!(*refinements.lock().unwrap(), 3);
    *fails.lock().unwrap() = true;
    let unavailable = resolver.resolve(root, true).await.unwrap();
    assert_eq!(unavailable.web_url, None);
    assert_eq!(unavailable.canonical_key, "ssh.forge.test/team/repo");
}

#[tokio::test(start_paused = true)]
async fn retries_git_root_discovery_after_the_negative_ttl() {
    let script = Arc::new(Script::default());
    *script.root.lock().unwrap() = "/repo".into();
    *script.remote.lock().unwrap() = "git@github.com:Octo-Org/sample-app.git".into();
    *script.root_failures.lock().unwrap() = 1;
    let resolver = RepositoryIdentities::new(RepositoryIdentityOptions {
        runner: Arc::new(ScriptedRunner(script.clone())),
        ..RepositoryIdentityOptions::default()
    });
    assert_eq!(resolver.resolve("/repo/packages/web", false).await, None);
    assert_eq!(resolver.resolve("/repo/packages/web", false).await, None);
    tokio::time::advance(Duration::from_secs(60)).await;
    let recovered = resolver.resolve("/repo/packages/web", false).await;
    assert_eq!(recovered.unwrap().root_path.as_deref(), Some("/repo"));
    assert_eq!(
        *script.calls.lock().unwrap(),
        vec![
            args(&["-C", "/repo/packages/web", "rev-parse", "--show-toplevel"]),
            args(&["-C", "/repo/packages/web", "rev-parse", "--show-toplevel"]),
            args(&["-C", "/repo", "remote", "-v"]),
        ]
    );
}

fn git(cwd: &Path, arguments: &[&str]) {
    let status = std::process::Command::new("git").arg("-C").arg(cwd).args(arguments).output().expect("git runs");
    assert!(status.status.success(), "git {arguments:?}: {}", String::from_utf8_lossy(&status.stderr));
}

fn real(path: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap()
}

#[tokio::test]
async fn normalizes_equivalent_github_remotes_into_a_stable_identity() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["remote", "add", "origin", "git@github.com:Octo-Org/sample-app.git"]);
    let identity = RepositoryIdentities::default().resolve(dir.path().to_str().unwrap(), false).await.unwrap();
    assert_eq!(identity.canonical_key, "github.com/octo-org/sample-app");
    assert_eq!(real(Path::new(identity.root_path.as_deref().unwrap())), real(dir.path()));
    assert_eq!(identity.display_name.as_deref(), Some("octo-org/sample-app"));
    assert_eq!(identity.provider.as_deref(), Some("github"));
    assert_eq!(identity.owner.as_deref(), Some("octo-org"));
    assert_eq!(identity.name.as_deref(), Some("sample-app"));
}

#[tokio::test]
async fn returns_the_top_level_root_from_a_nested_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("packages").join("web");
    std::fs::create_dir_all(&nested).unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["remote", "add", "origin", "git@github.com:Octo-Org/sample-app.git"]);
    let identity = RepositoryIdentities::default().resolve(nested.to_str().unwrap(), false).await.unwrap();
    assert_eq!(identity.canonical_key, "github.com/octo-org/sample-app");
    assert_eq!(real(Path::new(identity.root_path.as_deref().unwrap())), real(dir.path()));
}

#[tokio::test]
async fn returns_none_for_non_git_folders_and_repos_without_remotes() {
    let non_git = tempfile::tempdir().unwrap();
    let no_remote = tempfile::tempdir().unwrap();
    git(no_remote.path(), &["init", "-q"]);
    let resolver = RepositoryIdentities::default();
    assert_eq!(resolver.resolve(non_git.path().to_str().unwrap(), false).await, None);
    assert_eq!(resolver.resolve(no_remote.path().to_str().unwrap(), false).await, None);
}

#[tokio::test]
async fn refreshes_the_primary_upstream_after_add_or_replace_before_expiry() {
    for change in ["add", "replace"] {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["remote", "add", "origin", "git@github.com:someone/sample-app.git"]);
        if change == "replace" {
            git(dir.path(), &["remote", "add", "upstream", "git@github.com:Octo-Org/previous.git"]);
        }
        let resolver = RepositoryIdentities::default();
        let initial = resolver.resolve(cwd, false).await.unwrap();
        assert_eq!(
            initial.canonical_key,
            if change == "add" {
                "github.com/someone/sample-app"
            } else {
                "github.com/octo-org/previous"
            }
        );
        git(
            dir.path(),
            &[
                "remote",
                if change == "add" { "add" } else { "set-url" },
                "upstream",
                "git@github.com:Octo-Org/sample-app.git",
            ],
        );
        assert_eq!(resolver.resolve(cwd, false).await.as_ref(), Some(&initial));
        let identity = resolver.resolve(cwd, true).await.unwrap();
        assert_eq!(identity.locator.remote_name, "upstream");
        assert_eq!(identity.canonical_key, "github.com/octo-org/sample-app");
        assert_eq!(identity.display_name.as_deref(), Some("octo-org/sample-app"));
        assert_eq!(resolver.resolve(cwd, false).await.as_ref(), Some(&identity));
    }
}

#[tokio::test]
async fn uses_the_last_path_segment_as_the_name_for_nested_groups() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["remote", "add", "origin", "git@gitlab.com:Octo-Org/platform/sample-app.git"]);
    let identity = RepositoryIdentities::default().resolve(dir.path().to_str().unwrap(), false).await.unwrap();
    assert_eq!(identity.canonical_key, "gitlab.com/octo-org/platform/sample-app");
    assert_eq!(identity.display_name.as_deref(), Some("octo-org/platform/sample-app"));
    assert_eq!(identity.owner.as_deref(), Some("octo-org"));
    assert_eq!(identity.name.as_deref(), Some("sample-app"));
}

#[tokio::test]
async fn keeps_none_cached_until_the_negative_ttl_expires() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    git(dir.path(), &["init", "-q"]);
    let resolver = RepositoryIdentities::new(RepositoryIdentityOptions {
        negative_ttl: Duration::from_millis(50),
        positive_ttl: Duration::from_secs(1),
        cache_capacity: 16,
        ..RepositoryIdentityOptions::default()
    });
    assert_eq!(resolver.resolve(cwd, false).await, None);
    git(dir.path(), &["remote", "add", "origin", "git@github.com:Octo-Org/sample-app.git"]);
    for _ in 0..3 {
        assert_eq!(resolver.resolve(cwd, false).await, None);
    }
    tokio::time::sleep(Duration::from_millis(120)).await;
    let refreshed = resolver.resolve(cwd, false).await.unwrap();
    assert_eq!(refreshed.canonical_key, "github.com/octo-org/sample-app");
    assert_eq!(refreshed.name.as_deref(), Some("sample-app"));
}

#[tokio::test]
async fn refreshes_cached_identities_after_the_positive_ttl() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["remote", "add", "origin", "git@github.com:Octo-Org/sample-app.git"]);
    let resolver = RepositoryIdentities::new(RepositoryIdentityOptions {
        negative_ttl: Duration::from_millis(50),
        positive_ttl: Duration::from_millis(100),
        cache_capacity: 16,
        ..RepositoryIdentityOptions::default()
    });
    assert_eq!(resolver.resolve(cwd, false).await.unwrap().canonical_key, "github.com/octo-org/sample-app");
    git(dir.path(), &["remote", "set-url", "origin", "git@github.com:Octo-Org/sample-app-next.git"]);
    assert_eq!(resolver.resolve(cwd, false).await.unwrap().canonical_key, "github.com/octo-org/sample-app");
    tokio::time::sleep(Duration::from_millis(180)).await;
    let refreshed = resolver.resolve(cwd, false).await.unwrap();
    assert_eq!(refreshed.canonical_key, "github.com/octo-org/sample-app-next");
    assert_eq!(refreshed.display_name.as_deref(), Some("octo-org/sample-app-next"));
    assert_eq!(refreshed.name.as_deref(), Some("sample-app-next"));
}

#[test]
fn parses_and_picks_remotes() {
    use zc_project::identity::{parse_remote_fetch_urls, pick_primary_remote};
    let remotes = parse_remote_fetch_urls("zeta\tgit@h:a/z.git (fetch)\nzeta\tgit@h:a/z.git (push)\nalpha\tgit@h:a/a.git (fetch)\n\nbad line\n");
    assert_eq!(pick_primary_remote(&remotes), Some(("alpha".into(), "git@h:a/a.git".into())));
    let with_origin = parse_remote_fetch_urls("origin\tgit@h:a/o.git (fetch)\nalpha\tgit@h:a/a.git (fetch)\n");
    assert_eq!(pick_primary_remote(&with_origin).unwrap().0, "origin");
    assert_eq!(pick_primary_remote(&[]), None);
}
