//! The Forgejo cases of `SourceControlDiscovery.test.ts` that exercise `ForgejoCli` and the
//! Forgejo discovery spec (the pull request provider ones belong to WP-22).

#![allow(clippy::result_large_err, clippy::type_complexity)]

mod common;

use std::sync::Arc;

use common::http::*;
use common::*;
use serde_json::{json, Value};
use zc_contracts::{SourceControlProviderAuthStatus as Auth, SourceControlProviderInfo, SourceControlProviderKind};
use zc_core::process::ProcessRunInput;
use zc_sourcecontrol::discovery::{AuthProbeInput, RefinementInput};
use zc_sourcecontrol::forgejo::cli::{ForgejoApiInput, ForgejoRepositoryInput};
use zc_sourcecontrol::forgejo::provider::discovery;
use zc_sourcecontrol::forgejo::{ForgejoCli, ForgejoCommand, ForgejoEnvironment};
use zc_sourcecontrol::provider::SourceControlProviderContext;
use zc_sourcecontrol::util::ManualClock;

/// A Linux-layout home whose `fj` keys file holds `keys` (none when `None`).
fn home(keys: Option<&str>) -> (Tmp, ForgejoEnvironment) {
    let home = Tmp::new("zc-forgejo-home-");
    if let Some(keys) = keys {
        write(&home.path, ".local/share/forgejo-cli/keys.json", keys);
    }
    let environment = ForgejoEnvironment {
        platform: "linux".into(),
        home: home.path.clone(),
        data_home: None,
        app_data: None,
    };
    (home, environment)
}

fn logins(entries: Value) -> String {
    entries.to_string()
}

fn output(stdout: &str, stderr: &str) -> Result<zc_core::process::ProcessRunOutput, zc_core::process::ProcessRunError> {
    exit(0, stdout, stderr)
}

fn forgejo_context(base_url: &str, remote_url: &str, requested_host: Option<&str>) -> SourceControlProviderContext {
    SourceControlProviderContext {
        provider: SourceControlProviderInfo {
            kind: SourceControlProviderKind::Forgejo,
            name: "Forgejo".into(),
            base_url: base_url.into(),
        },
        remote_name: "origin".into(),
        remote_url: remote_url.into(),
        requested_host: requested_host.map(Into::into),
    }
}

#[test]
fn discovers_forgejo_accounts_and_retains_the_server_port() {
    let spec = discovery();
    let auth = (spec.parse_auth)(&AuthProbeInput {
        stdout: logins(
            json!([{"name": "work", "url": "http://forgejo.local:3000", "ssh_host": "git.forgejo.local", "user": "maria", "default": "true", "valid": "true"}]),
        ),
        ..AuthProbeInput::default()
    });
    assert_eq!(auth.status, Auth::Authenticated);
    assert_eq!(auth.account.0.as_deref(), Some("maria"));
    assert_eq!(auth.host.0.as_deref(), Some("forgejo.local:3000"));
    let revoked = (spec.parse_auth)(&AuthProbeInput {
        stdout: logins(json!([{"name": "work", "url": "http://forgejo.local:3000", "user": "maria", "default": "true", "valid": "false"}])),
        ..AuthProbeInput::default()
    });
    assert_eq!(revoked.status, Auth::Unauthenticated);
    let refined = (spec.refine_unknown_remote.unwrap())(&RefinementInput {
        cwd: "/repo".into(),
        context: SourceControlProviderContext {
            provider: SourceControlProviderInfo {
                kind: SourceControlProviderKind::Unknown,
                name: "git.forgejo.local".into(),
                base_url: "https://git.forgejo.local".into(),
            },
            remote_name: "origin".into(),
            remote_url: "git@git.forgejo.local:maria/project.git".into(),
            requested_host: None,
        },
        auth: AuthProbeInput {
            stdout: logins(json!([{"name": "work", "url": "http://forgejo.local:3000", "ssh_host": "git.forgejo.local", "user": "maria", "default": "true"}])),
            ..AuthProbeInput::default()
        },
    })
    .unwrap();
    assert_eq!(
        (refined.kind, refined.name.as_str(), refined.base_url.as_str()),
        (SourceControlProviderKind::Forgejo, "Forgejo / Gitea", "http://forgejo.local:3000")
    );
}

#[tokio::test]
async fn rejects_http_failures_even_when_tea_exits_successfully() {
    let (_home, environment) = home(None);
    let runner = ScriptedRunner::new(|input| {
        if input.args[0] == "api" {
            assert_eq!(input.stdin.as_deref(), Some(r#"{"state":"closed"}"#));
            assert!(input.args.contains(&"work".to_owned()));
            assert!(input.args.contains(&"http://forgejo.local:3000/api/v1/repos/maria/project/pulls/42".to_owned()));
        }
        if input.args[0] == "login" {
            return ok(&logins(
                json!([{"name": "work", "url": "http://forgejo.local:3000", "ssh_host": "forgejo.local", "user": "maria", "default": "true"}]),
            ));
        }
        output(r#"{"message":"not found"}"#, "HTTP/1.1 404 Not Found\n")
    });
    let cli = ForgejoCli::new(runner.process(), environment, ManualClock::new(0));
    let error = cli
        .api(&ForgejoApiInput {
            target: ForgejoRepositoryInput {
                cwd: "/repo".into(),
                repository: Some("http://forgejo.local:3000/maria/project".into()),
                ..ForgejoRepositoryInput::default()
            },
            path: "repos/maria/project/pulls/42".into(),
            method: Some("PATCH".into()),
            body: Some(json!({"state": "closed"})),
        })
        .await
        .unwrap_err();
    assert_eq!(error.detail, "Forgejo repository or pull request was not found.");
    assert_eq!(error.http_status, Some(404));
}

#[tokio::test]
async fn routes_mounted_repositories_without_repeating_the_mount_in_api_paths() {
    let (_home, environment) = home(Some(
        &json!({"hosts": {"code.test/forgejo": {"type": "Application", "token": "test-token"}}}).to_string(),
    ));
    let runner = ScriptedRunner::new(|input: &ProcessRunInput| {
        if input.command == "git" {
            return exit(2, "", "");
        }
        if input.args[0] == "login" {
            return ok(&logins(
                json!([{"name": "mounted", "url": "https://code.test/forgejo", "ssh_host": "code.test", "user": "maria", "default": "true"}]),
            ));
        }
        assert_eq!(input.command, "tea");
        let last = input.args.last().unwrap();
        if last.ends_with("/user") {
            assert!(!input.args.contains(&"--repo".to_owned()));
        }
        let supported = [
            "https://code.test/forgejo/api/v1/user",
            "https://code.test/forgejo/api/v1/repos/maria/project/pulls?state=open",
            "https://code.test/forgejo/api/v1/repos/maria/project",
            "https://code.test/forgejo/api/v1/repos/reviewer/project/contents/file.ts",
        ];
        if supported.contains(&last.as_str()) {
            output("[]", "HTTP/1.1 200 OK\n")
        } else {
            output("{}", "HTTP/1.1 404 Not Found\n")
        }
    });
    let cli = ForgejoCli::new(runner.process(), environment, ManualClock::new(0));
    let viewer = cli
        .api(&ForgejoApiInput {
            target: ForgejoRepositoryInput {
                cwd: "/upstream-only".into(),
                host: Some("code.test".into()),
                ..ForgejoRepositoryInput::default()
            },
            path: "user".into(),
            ..ForgejoApiInput::default()
        })
        .await
        .unwrap();
    assert_eq!(viewer.stdout, "[]");
    let mounted = cli
        .resolve_repository(&ForgejoRepositoryInput {
            cwd: "/upstream-only".into(),
            host: Some("code.test".into()),
            repository: Some("maria/project".into()),
            ..ForgejoRepositoryInput::default()
        })
        .await
        .unwrap();
    assert_eq!(
        (mounted.base_url.as_str(), mounted.repository.as_str()),
        ("https://code.test/forgejo", "maria/project")
    );
    for path in [
        "repos/forgejo/maria/project/pulls?state=open",
        "repos/forgejo/maria/project",
        "repos/reviewer/project/contents/file.ts",
    ] {
        let result = cli
            .api(&ForgejoApiInput {
                target: ForgejoRepositoryInput {
                    cwd: "/repo".into(),
                    repository: Some("forgejo/maria/project".into()),
                    context: Some(forgejo_context(
                        "https://code.test/forgejo",
                        "https://code.test/forgejo/maria/project.git",
                        None,
                    )),
                    ..ForgejoRepositoryInput::default()
                },
                path: path.into(),
                ..ForgejoApiInput::default()
            })
            .await
            .unwrap();
        assert_eq!(result.stdout, "[]", "{path}");
    }
    let same_owner = cli
        .resolve_repository(&ForgejoRepositoryInput {
            cwd: "/repo".into(),
            repository: Some("forgejo/project".into()),
            context: Some(forgejo_context("https://code.test/forgejo", "ssh://git@code.test/forgejo/project.git", None)),
            ..ForgejoRepositoryInput::default()
        })
        .await
        .unwrap();
    assert_eq!((same_owner.command, same_owner.repository.as_str()), (ForgejoCommand::Tea, "forgejo/project"));
}

#[tokio::test]
async fn prefers_fj_for_http_and_ported_ssh_aliases_on_root_servers() {
    let server = MockServer::start(|seen| {
        assert_eq!(seen.headers.get("authorization").map(String::as_str), Some("token test-token"));
        if seen.target.ends_with("/user") {
            assert_eq!(seen.method, "GET");
            return reply(200, r#"{"login":"maria"}"#);
        }
        assert_eq!(seen.method, "POST");
        assert_eq!(serde_json::from_str::<Value>(&seen.body).unwrap(), json!({"body": "verified through fj"}));
        reply(201, r#"{"id":99}"#)
    })
    .await;
    let port = url::Url::parse(&server.base).unwrap().port().unwrap();
    let host = format!("forgejo.local:{port}");
    let (_home, environment) = home(Some(
        &json!({
            "hosts": {host.clone(): {"type": "Application", "token": "test-token"}, "forgejo.local:4000": {"type": "OAuth", "token": "other-token"}},
            "aliases": {"ssh.forgejo.local:2222": host.clone()},
        })
        .to_string(),
    ));
    let remote_lines = format!(
        "upstream\thttp://{host}/maria/project.git (fetch)\nother\thttp://{host}/maria/other.git (fetch)\nunrelated\thttp://other.local:{port}/maria/project.git (fetch)"
    );
    let expected_host = format!("http://{host}");
    let runner = ScriptedRunner::new(move |input| {
        if input.command == "git" {
            return ok(&remote_lines);
        }
        assert_eq!(input.command, "fj");
        assert_eq!(input.args, ["--host", expected_host.as_str(), "whoami"]);
        ok("")
    });
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .resolve("forgejo.local", "127.0.0.1:0".parse().unwrap())
        .build()
        .unwrap();
    let cli = ForgejoCli::with_http_client(runner.process(), environment, ManualClock::new(0), client);
    for remote_url in [
        format!("http://{host}/maria/project.git"),
        "ssh://git@ssh.forgejo.local:2222/maria/project.git".to_owned(),
        "ssh://git@forgejo.local:2222/maria/project.git".to_owned(),
    ] {
        let result = cli
            .api(&ForgejoApiInput {
                target: ForgejoRepositoryInput {
                    cwd: "/repo".into(),
                    repository: Some("maria/project".into()),
                    context: Some(forgejo_context(&format!("http://{host}"), &remote_url, Some(&host))),
                    ..ForgejoRepositoryInput::default()
                },
                path: "repos/maria/project/issues/42/comments".into(),
                method: Some("POST".into()),
                body: Some(json!({"body": "verified through fj"})),
            })
            .await
            .unwrap();
        assert_eq!(result.stdout, r#"{"id":99}"#, "{remote_url}");
    }
    assert_eq!(runner.lines().iter().filter(|l| l.starts_with("fj")).count(), 1);
    let targets: Vec<String> = server.seen().iter().map(|s| s.target.clone()).collect();
    assert_eq!(targets, vec!["/api/v1/repos/maria/project/issues/42/comments"; 3]);
    let viewer = cli
        .api(&ForgejoApiInput {
            target: ForgejoRepositoryInput {
                cwd: "/upstream-only".into(),
                host: Some(host.clone()),
                ..ForgejoRepositoryInput::default()
            },
            path: "user".into(),
            ..ForgejoApiInput::default()
        })
        .await
        .unwrap();
    assert_eq!(viewer.stdout, r#"{"login":"maria"}"#);
    assert_eq!(server.seen().last().unwrap().target, "/api/v1/user");
    let upstream = cli
        .resolve_repository(&ForgejoRepositoryInput {
            cwd: "/upstream-only".into(),
            host: Some(host.clone()),
            repository: Some("maria/project".into()),
            ..ForgejoRepositoryInput::default()
        })
        .await
        .unwrap();
    assert_eq!((upstream.base_url, upstream.repository), (format!("http://{host}"), "maria/project".to_owned()));
}

#[tokio::test]
async fn falls_back_to_tea_when_fj_is_missing_or_has_no_account_for_this_server() {
    for scenario in ["missing-cli", "missing-account", "stale-invalid-storage"] {
        let keys = match scenario {
            "stale-invalid-storage" => "invalid json".to_owned(),
            "missing-cli" => json!({"hosts": {"forgejo.local:3000": {"type": "Application", "token": "test-token"}}}).to_string(),
            _ => json!({"hosts": {"other.local": {"type": "Application", "token": "test-token"}}}).to_string(),
        };
        let (_home, environment) = home(Some(&keys));
        let runner = ScriptedRunner::new(|input| {
            if input.command == "git" {
                return exit(2, "", "");
            }
            if input.command == "fj" {
                return missing(input);
            }
            assert_eq!(input.command, "tea");
            if input.args.last().unwrap().ends_with("/user") {
                assert!(!input.args.contains(&"--repo".to_owned()));
            }
            if input.args[0] == "login" {
                return ok(&logins(
                    json!([{"name": "work", "url": "https://forgejo.local:3000", "user": "maria", "default": "true", "valid": "true"}]),
                ));
            }
            output("[]", "HTTP/1.1 200 OK\n")
        });
        let cli = ForgejoCli::new(runner.process(), environment, ManualClock::new(0));
        let result = cli
            .api(&ForgejoApiInput {
                target: ForgejoRepositoryInput {
                    cwd: "/repo".into(),
                    repository: Some("https://forgejo.local:3000/maria/project".into()),
                    ..ForgejoRepositoryInput::default()
                },
                path: "repos/maria/project/pulls".into(),
                ..ForgejoApiInput::default()
            })
            .await
            .unwrap();
        assert_eq!(result.stdout, "[]");
        let commands: Vec<String> = runner.calls().iter().map(|c| c.command.clone()).collect();
        let expected: &[&str] = if scenario == "missing-account" {
            &["tea", "tea"]
        } else {
            &["fj", "tea", "tea"]
        };
        assert_eq!(commands, expected, "{scenario}");
        let viewer = cli
            .api(&ForgejoApiInput {
                target: ForgejoRepositoryInput {
                    cwd: "/upstream-only".into(),
                    host: Some("forgejo.local:3000".into()),
                    ..ForgejoRepositoryInput::default()
                },
                path: "user".into(),
                ..ForgejoApiInput::default()
            })
            .await
            .unwrap();
        assert_eq!(viewer.stdout, "[]");
    }
}

#[tokio::test]
async fn managed_discovery_falls_back_to_tea_and_reports_its_login() {
    let (_home, environment) = home(None);
    let clis = FakeClis::new();
    clis.respond("tea", "--version", "tea version 0.16.0\n", "", 0);
    clis.respond(
        "tea",
        "login status --output json",
        &logins(json!([{"name": "forgejo", "url": "https://forgejo.example.test", "ssh_host": "forgejo.example.test", "user": "forgejo-user", "valid": "true", "default": "true"}])),
        "",
        0,
    );
    let cwd = Tmp::new("zc-forgejo-discovery-");
    let process = clis.process();
    let cli = ForgejoCli::new(process.clone(), environment, ManualClock::new(0));
    let managed = zc_sourcecontrol::forgejo::ForgejoDiscovery::new(cli, process.clone());
    let item = zc_sourcecontrol::discovery::ManagedCliDiscovery::probe(&managed, cwd.str()).await;
    let encoded = serde_json::to_value(&item).unwrap();
    assert_eq!(encoded["executable"], "tea");
    assert_eq!(encoded["status"], "available");
    assert_eq!(encoded["version"], json!({"_tag": "Some", "value": "tea version 0.16.0"}));
    assert_eq!(encoded["auth"]["account"], json!({"_tag": "Some", "value": "forgejo-user"}));
    let _ = Arc::strong_count(&clis);
}
