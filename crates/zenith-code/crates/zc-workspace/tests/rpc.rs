//! The workspace RPC handlers: registration, payload checks, the `ws.ts` error folding, and the
//! wire values checked against the TypeScript contracts (`code/scripts/contracts-oracle.ts`,
//! when Node and `code/node_modules` are available).

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use serde_json::{json, Value};
use zc_contracts::{
    EditorId, FilesystemBrowseInput, LaunchEditorInput, ProjectEntryKind, ProjectListEntriesInput, ProjectReadFileInput, ProjectSearchContentsInput,
    ProjectSearchEntriesInput, ProjectWriteFileInput,
};
use zc_core::VcsProcess;
use zc_rpc::{Failure, RpcRouter};
use zc_workspace::launcher::command::LauncherEnv;
use zc_workspace::launcher::{DetachedLaunch, EnvSource, ExternalLauncher, LaunchSpawner};
use zc_workspace::platform::NodePlatform;
use zc_workspace::rpc::{self, WorkspaceRpcServices, METHOD_SCOPES};
use zc_workspace::{FffFactory, SearchIndexMap, WorkspaceEntries, WorkspaceFileSystem, WorkspacePaths};

struct NoSpawn;

impl LaunchSpawner for NoSpawn {
    fn spawn_detached(&self, _launch: &DetachedLaunch) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(libc::ENOENT))
    }
    fn run_probe(&self, _command: &str, _args: &[String]) -> futures::future::BoxFuture<'static, Option<(i32, String)>> {
        Box::pin(async { None })
    }
}

fn services() -> WorkspaceRpcServices {
    let entries = WorkspaceEntries::new(
        WorkspacePaths::new(),
        SearchIndexMap::new(Arc::new(FffFactory)),
        Arc::new(VcsProcess::default()),
    );
    let file_system = WorkspaceFileSystem::new(WorkspacePaths::new(), entries.clone());
    let launcher = ExternalLauncher::new(
        NodePlatform::Darwin,
        EnvSource::Fixed(LauncherEnv::from_pairs([("PATH", "/nonexistent")])),
        Arc::new(NoSpawn),
    );
    WorkspaceRpcServices {
        entries,
        file_system,
        launcher: Arc::new(launcher),
    }
}

fn temp_dir() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("zc-ws-rpc-").tempdir().unwrap()
}

fn path_of(dir: &tempfile::TempDir) -> String {
    dir.path().to_string_lossy().into_owned()
}

fn fail<E: serde::Serialize>(result: Result<impl std::fmt::Debug, Failure<E>>) -> Value {
    match result.unwrap_err() {
        Failure::Fail(error) => serde_json::to_value(error).unwrap(),
        Failure::Die(defect) => panic!("expected a typed failure, got a defect {defect}"),
        Failure::Interrupt => panic!("interrupted"),
    }
}

fn die<E>(result: Result<impl std::fmt::Debug, Failure<E>>) -> Value {
    match result.unwrap_err() {
        Failure::Die(defect) => defect,
        _ => panic!("expected a defect"),
    }
}

#[test]
fn registers_the_seven_methods_with_their_scopes() {
    let router = rpc::register(RpcRouter::builder(), services()).build().unwrap();
    for (tag, _scope) in METHOD_SCOPES {
        assert!(router.contains(tag), "{tag}");
        assert_eq!(router.is_stream(tag), Some(false));
    }
    // The scopes agree with the generated contract table.
    for (tag, scope) in METHOD_SCOPES {
        let spec = zc_contracts::METHODS.iter().find(|spec| spec.tag == tag).unwrap();
        assert_eq!(serde_json::to_value(spec.scope).unwrap(), json!(scope), "{tag}");
    }
}

#[tokio::test]
async fn payloads_are_checked_like_the_schemas() {
    let services = services();
    let search = |query: &str, limit: i64| ProjectSearchEntriesInput {
        cwd: "/w".into(),
        query: query.into(),
        limit,
        kind: None,
        image_only: None,
    };
    assert!(die(rpc::search_entries(&services, search("x", 0)).await).is_string());
    assert!(die(rpc::search_entries(&services, search("x", 201)).await).is_string());
    assert!(die(rpc::search_entries(&services, search(&"q".repeat(257), 5)).await).is_string());
    let blank_cwd = ProjectReadFileInput {
        cwd: "   ".into(),
        relative_path: "a".into(),
    };
    assert!(die(rpc::read_file(&services, blank_cwd).await).is_string());
    let long_path = ProjectReadFileInput {
        cwd: "/w".into(),
        relative_path: "a".repeat(513),
    };
    assert!(die(rpc::read_file(&services, long_path).await).is_string());
    let contents = ProjectSearchContentsInput {
        cwd: "/w".into(),
        query: "".into(),
        limit: 5,
        case_sensitive: false,
        whole_word: false,
        use_regex: false,
    };
    assert!(die(rpc::search_contents(&services, contents.clone()).await).is_string());
    let too_many = ProjectSearchContentsInput {
        query: "x".into(),
        limit: 501,
        ..contents
    };
    assert!(die(rpc::search_contents(&services, too_many).await).is_string());
    let browse = FilesystemBrowseInput {
        partial_path: "  ".into(),
        cwd: None,
    };
    assert!(die(rpc::browse(&services, browse).await).is_string());
}

#[tokio::test]
async fn trims_payload_strings_like_the_schemas() {
    let dir = temp_dir();
    let cwd = path_of(&dir);
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    let services = services();
    let read = rpc::read_file(
        &services,
        ProjectReadFileInput {
            cwd: format!("  {cwd} "),
            relative_path: " a.txt ".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(read.relative_path, "a.txt");
    let search = rpc::search_entries(
        &services,
        ProjectSearchEntriesInput {
            cwd: cwd.clone(),
            query: "  a.txt  ".into(),
            limit: 5,
            kind: None,
            image_only: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(search.entries[0].path, "a.txt");
}

/// Encoded wire values, with the schema id each must decode under.
async fn wire_samples() -> Vec<(String, Value)> {
    let dir = temp_dir();
    let cwd = path_of(&dir);
    let outside = temp_dir();
    std::fs::create_dir_all(dir.path().join("src/components")).unwrap();
    std::fs::write(dir.path().join("src/components/Composer.tsx"), "export const search = 'héllo wörld';\n").unwrap();
    std::fs::write(dir.path().join("bin.dat"), [0u8, 1, 2]).unwrap();
    std::fs::write(outside.path().join("secret.txt"), "s").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), dir.path().join("link.txt")).unwrap();
    let services = services();
    let mut samples: Vec<(String, Value)> = Vec::new();
    let mut ok = |tag: &str, value: Value| samples.push((format!("rpc:{tag}:success"), value));

    let search = |query: &str, kind| ProjectSearchEntriesInput {
        cwd: cwd.clone(),
        query: query.into(),
        limit: 10,
        kind,
        image_only: None,
    };
    ok(
        "projects.searchEntries",
        serde_json::to_value(rpc::search_entries(&services, search("compo", None)).await.unwrap()).unwrap(),
    );
    ok(
        "projects.searchEntries",
        serde_json::to_value(rpc::search_entries(&services, search("src", Some(ProjectEntryKind::Directory))).await.unwrap()).unwrap(),
    );
    let contents = ProjectSearchContentsInput {
        cwd: cwd.clone(),
        query: "wörld".into(),
        limit: 10,
        case_sensitive: false,
        whole_word: true,
        use_regex: false,
    };
    ok(
        "projects.searchContents",
        serde_json::to_value(rpc::search_contents(&services, contents).await.unwrap()).unwrap(),
    );
    let invalid_regex = ProjectSearchContentsInput {
        cwd: cwd.clone(),
        query: "a)(".into(),
        limit: 10,
        case_sensitive: false,
        whole_word: false,
        use_regex: true,
    };
    ok(
        "projects.searchContents",
        serde_json::to_value(rpc::search_contents(&services, invalid_regex).await.unwrap()).unwrap(),
    );
    let list = |directory_path: Option<&str>| ProjectListEntriesInput {
        cwd: cwd.clone(),
        directory_path: directory_path.map(str::to_owned),
    };
    ok(
        "projects.listEntries",
        serde_json::to_value(rpc::list_entries(&services, list(None)).await.unwrap()).unwrap(),
    );
    ok(
        "projects.listEntries",
        serde_json::to_value(rpc::list_entries(&services, list(Some("src"))).await.unwrap()).unwrap(),
    );
    let read = |path: &str| ProjectReadFileInput {
        cwd: cwd.clone(),
        relative_path: path.into(),
    };
    ok(
        "projects.readFile",
        serde_json::to_value(rpc::read_file(&services, read("src/components/Composer.tsx")).await.unwrap()).unwrap(),
    );
    let write = ProjectWriteFileInput {
        cwd: cwd.clone(),
        relative_path: "notes/a.md".into(),
        contents: "# A\n".into(),
    };
    ok(
        "projects.writeFile",
        serde_json::to_value(rpc::write_file(&services, write).await.unwrap()).unwrap(),
    );
    let browse = FilesystemBrowseInput {
        partial_path: format!("{cwd}/"),
        cwd: None,
    };
    ok(
        "filesystem.browse",
        serde_json::to_value(rpc::browse(&services, browse).await.unwrap()).unwrap(),
    );

    let mut err = |tag: &str, value: Value| samples.push((format!("rpc:{tag}:error"), value));
    err("projects.readFile", fail(rpc::read_file(&services, read("../x")).await));
    err("projects.readFile", fail(rpc::read_file(&services, read("link.txt")).await));
    err("projects.readFile", fail(rpc::read_file(&services, read("src")).await));
    err("projects.readFile", fail(rpc::read_file(&services, read("bin.dat")).await));
    err("projects.readFile", fail(rpc::read_file(&services, read("missing.txt")).await));
    let write_out = ProjectWriteFileInput {
        cwd: cwd.clone(),
        relative_path: "/etc/x".into(),
        contents: "".into(),
    };
    err("projects.writeFile", fail(rpc::write_file(&services, write_out).await));
    let missing_root = ProjectSearchEntriesInput {
        cwd: format!("{cwd}/missing"),
        query: "a".into(),
        limit: 3,
        kind: None,
        image_only: None,
    };
    err("projects.searchEntries", fail(rpc::search_entries(&services, missing_root).await));
    let file_root = ProjectSearchContentsInput {
        cwd: format!("{cwd}/bin.dat"),
        query: "a".into(),
        limit: 3,
        case_sensitive: false,
        whole_word: false,
        use_regex: false,
    };
    err("projects.searchContents", fail(rpc::search_contents(&services, file_root).await));
    err("projects.listEntries", fail(rpc::list_entries(&services, list(Some(".git/../.."))).await));
    let relative = FilesystemBrowseInput {
        partial_path: "./x".into(),
        cwd: None,
    };
    err("filesystem.browse", fail(rpc::browse(&services, relative).await));
    let missing_dir = FilesystemBrowseInput {
        partial_path: format!("{cwd}/missing/"),
        cwd: Some(cwd.clone()),
    };
    err("filesystem.browse", fail(rpc::browse(&services, missing_dir).await));
    let launch = LaunchEditorInput {
        cwd: cwd.clone(),
        editor: EditorId::Vscode,
        reveal: None,
    };
    err("shell.openInEditor", fail(rpc::open_in_editor(&services, launch).await));
    samples
}

#[tokio::test]
async fn folds_internal_errors_like_ws_ts() {
    let dir = temp_dir();
    let cwd = path_of(&dir);
    let services = services();
    let read = fail(
        rpc::read_file(
            &services,
            ProjectReadFileInput {
                cwd: cwd.clone(),
                relative_path: "../x".into(),
            },
        )
        .await,
    );
    assert_eq!(
        read,
        json!({
            "_tag": "ProjectReadFileError",
            "cwd": cwd,
            "relativePath": "../x",
            "failure": "workspace_path_outside_root",
            "message": format!("Failed to read workspace file '../x' in '{cwd}'."),
            "cause": {
                "name": "WorkspacePathOutsideRootError",
                "message": "Workspace file path must be relative to the project root: ../x",
            },
        })
    );
    let missing = format!("{cwd}/missing");
    let search = fail(
        rpc::search_entries(
            &services,
            ProjectSearchEntriesInput {
                cwd: missing.clone(),
                query: " ab ".into(),
                limit: 3,
                kind: None,
                image_only: None,
            },
        )
        .await,
    );
    assert_eq!(
        search,
        json!({
            "_tag": "ProjectSearchEntriesError",
            "cwd": missing,
            "queryLength": 2,
            "limit": 3,
            "failure": "workspace_root_not_found",
            "normalizedCwd": missing,
            "message": format!("Failed to search workspace entries in '{missing}'."),
            "cause": { "name": "WorkspaceRootNotExistsError", "message": format!("Workspace root does not exist: {missing}") },
        })
    );
    let browse = fail(
        rpc::browse(
            &services,
            FilesystemBrowseInput {
                partial_path: "./src".into(),
                cwd: None,
            },
        )
        .await,
    );
    assert_eq!(
        browse,
        json!({
            "_tag": "FilesystemBrowseError",
            "partialPath": "./src",
            "failure": "current_project_required",
            "message": "Failed to browse filesystem path './src'.",
            "cause": {
                "name": "WorkspaceEntriesCurrentProjectRequiredError",
                "message": "A current project is required to browse relative workspace path './src'.",
            },
        })
    );
    let listed = fail(
        rpc::list_entries(
            &services,
            ProjectListEntriesInput {
                cwd: cwd.clone(),
                directory_path: Some(".git".into()),
            },
        )
        .await,
    );
    assert_eq!(listed["failure"], json!("directory_list_failed"));
    assert_eq!(listed["normalizedCwd"], json!(cwd));
    assert_eq!(
        listed["detail"],
        json!(format!("Failed to read workspace directory '{cwd}/.git' while browsing '.git' from '{cwd}'."))
    );
    let launch = fail(
        rpc::open_in_editor(
            &services,
            LaunchEditorInput {
                cwd: "/w".into(),
                editor: EditorId::Vscode,
                reveal: None,
            },
        )
        .await,
    );
    assert_eq!(
        launch,
        json!({ "_tag": "ExternalLauncherCommandNotFoundError", "editor": "vscode", "command": "code" })
    );
}

#[tokio::test]
async fn wire_values_decode_with_the_typescript_contracts() {
    let samples = wire_samples().await;
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../code/scripts/contracts-oracle.ts");
    let effect = script.parent().unwrap().join("node_modules/effect/package.json");
    let node = Command::new("node").arg("--version").output();
    if node.is_err() || !effect.exists() {
        assert!(std::env::var("ZC_REQUIRE_GOLDEN").is_err(), "the contracts oracle is unavailable");
        eprintln!("skipping the contracts oracle: node or code/scripts/node_modules missing");
        return;
    }
    let mut child = Command::new("node").arg(&script).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let lines: Vec<String> = samples
        .iter()
        .map(|(schema, value)| json!({ "schema": schema, "value": value }).to_string())
        .collect();
    let writer = std::thread::spawn(move || {
        for line in lines {
            writeln!(stdin, "{line}").unwrap();
        }
    });
    let results: Vec<Value> = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
        .collect();
    writer.join().unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(results.len(), samples.len());
    let failures: Vec<String> = samples
        .iter()
        .zip(&results)
        .filter(|(_, result)| result["ok"] != json!(true))
        .map(|((schema, value), result)| format!("{schema}: {value} → {}", result["issue"]))
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}
