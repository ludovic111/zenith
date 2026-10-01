//! Ports of `process/externalLauncher.test.ts`: a recording spawner replaces the process
//! spawner, the platform and environment are fixed per test, and real temp directories hold
//! the stub executables.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Mutex;

use super::*;

#[derive(Clone)]
enum Probe {
    Done(i32, &'static str),
    Stall,
}

struct FakeSpawner {
    spawned: Mutex<Vec<DetachedLaunch>>,
    probes: Mutex<Vec<(String, Vec<String>)>>,
    probe: Mutex<Option<Probe>>,
}

impl FakeSpawner {
    fn new(probe: Option<Probe>) -> Arc<Self> {
        Arc::new(Self {
            spawned: Mutex::new(Vec::new()),
            probes: Mutex::new(Vec::new()),
            probe: Mutex::new(probe),
        })
    }

    fn spawned(&self) -> Vec<DetachedLaunch> {
        self.spawned.lock().unwrap().clone()
    }

    fn last(&self) -> DetachedLaunch {
        self.spawned().last().cloned().expect("something was spawned")
    }
}

impl LaunchSpawner for FakeSpawner {
    fn spawn_detached(&self, launch: &DetachedLaunch) -> std::io::Result<()> {
        self.spawned.lock().unwrap().push(launch.clone());
        Ok(())
    }

    fn run_probe(&self, command: &str, args: &[String]) -> BoxFuture<'static, Option<(i32, String)>> {
        self.probes.lock().unwrap().push((command.to_owned(), args.to_vec()));
        // A missing script answers like an empty successful run.
        let probe = self.probe.lock().unwrap().clone().unwrap_or(Probe::Done(0, ""));
        async move {
            match probe {
                Probe::Done(code, stdout) => Some((code, stdout.to_owned())),
                Probe::Stall => futures::future::pending().await,
            }
        }
        .boxed()
    }
}

/// The TS `testLayer`: platform, env, an identity (or mapped) spawn-executable resolver.
fn launcher(platform: NodePlatform, env: &[(&str, &str)], spawner: &Arc<FakeSpawner>) -> ExternalLauncher {
    ExternalLauncher::new(platform, EnvSource::Fixed(LauncherEnv::from_pairs(env.iter().copied())), spawner.clone())
        .with_spawn_resolver(Arc::new(|command, _, _| Some(command.to_owned())))
}

fn temp_dir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new().prefix(prefix).tempdir().unwrap()
}

fn path_of(dir: &tempfile::TempDir) -> String {
    dir.path().to_string_lossy().into_owned()
}

fn stub(path: &str, mode: u32) {
    std::fs::create_dir_all(Path::new(path).parent().unwrap()).unwrap();
    std::fs::write(path, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn executables(dir: &str, names: &[&str]) {
    for name in names {
        stub(&format!("{dir}/{name}"), 0o755);
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn decode_encoded_command(launch: &DetachedLaunch) -> String {
    let encoded = launch.args.last().unwrap();
    let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).unwrap();
    let units: Vec<u16> = bytes.chunks(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
    String::from_utf16(&units).unwrap()
}

#[test]
fn launches_the_default_browser_through_the_platform_command() {
    let spawner = FakeSpawner::new(None);
    launcher(NodePlatform::Linux, &[], &spawner)
        .launch_browser("https://example.com/some path")
        .unwrap();
    assert_eq!(
        spawner.last(),
        DetachedLaunch {
            command: "xdg-open".into(),
            args: strings(&["https://example.com/some path"]),
            shell: false
        }
    );
    let darwin = launcher(NodePlatform::Darwin, &[], &spawner).browser_launch("https://example.com");
    assert_eq!(darwin.command, "open");
    let windows = launcher(NodePlatform::Win32, &[("SYSTEMROOT", "D:\\Win")], &spawner).browser_launch("https://example.com/'x'");
    assert_eq!(windows.command, "D:\\Win\\System32\\WindowsPowerShell\\v1.0\\powershell.exe");
    assert_eq!(
        decode_encoded_command(&windows),
        "$ProgressPreference = 'SilentlyContinue'; Start 'https://example.com/''x'''"
    );
}

#[tokio::test]
async fn launches_an_installed_editor_with_platform_safe_arguments() {
    let bin = temp_dir("zc-editors-");
    std::fs::write(bin.path().join("code.CMD"), "@echo off\r\n").unwrap();
    let spawner = FakeSpawner::new(None);
    let launcher = launcher(NodePlatform::Win32, &[("PATH", &path_of(&bin)), ("PATHEXT", ".COM;.EXE;.BAT;.CMD")], &spawner).with_spawn_resolver(Arc::new(
        |command, _, _| {
            Some(if command == "code" {
                "C:\\Program Files\\Microsoft VS Code\\bin\\code.CMD".into()
            } else {
                command.into()
            })
        },
    ));
    launcher
        .launch_editor_by_name("vscode", "C:\\workspace with spaces\\src\\index.ts:12:4", false)
        .await
        .unwrap();
    let spawned = spawner.last();
    assert_eq!(spawned.command, "^\"C:\\Program^ Files\\Microsoft^ VS^ Code\\bin\\code.CMD^\"");
    assert_eq!(
        spawned.args,
        strings(&["^\"--goto^\"", "^\"C:\\workspace^ with^ spaces\\src\\index.ts:12:4^\""])
    );
    assert!(spawned.shell);
}

#[tokio::test]
async fn launches_cursor_in_classic_ide_mode_on_darwin_and_linux() {
    for platform in [NodePlatform::Darwin, NodePlatform::Linux] {
        let bin = temp_dir("zc-editors-");
        executables(&path_of(&bin), &["cursor"]);
        let spawner = FakeSpawner::new(None);
        let launcher = launcher(platform, &[("PATH", &path_of(&bin))], &spawner);
        for cwd in [
            "/workspace with spaces",
            "/workspace with spaces/src/index.ts",
            "/workspace with spaces/src/index.ts:12",
            "/workspace with spaces/src/index.ts:12:4",
        ] {
            launcher.launch_editor_by_name("cursor", cwd, false).await.unwrap();
        }
        let spawned: Vec<(String, Vec<String>)> = spawner.spawned().into_iter().map(|s| (s.command, s.args)).collect();
        assert_eq!(
            spawned,
            vec![
                ("cursor".into(), strings(&["--classic", "/workspace with spaces"])),
                ("cursor".into(), strings(&["--classic", "/workspace with spaces/src/index.ts"])),
                ("cursor".into(), strings(&["--classic", "--goto", "/workspace with spaces/src/index.ts:12"])),
                ("cursor".into(), strings(&["--classic", "--goto", "/workspace with spaces/src/index.ts:12:4"])),
            ],
            "{platform:?}"
        );
    }
}

#[tokio::test]
async fn launches_cursor_in_classic_ide_mode_through_the_windows_command_shim() {
    let bin = temp_dir("zc-editors-");
    std::fs::write(bin.path().join("cursor.CMD"), "@echo off\r\n").unwrap();
    let spawner = FakeSpawner::new(None);
    let launcher = launcher(NodePlatform::Win32, &[("PATH", &path_of(&bin)), ("PATHEXT", ".COM;.EXE;.BAT;.CMD")], &spawner).with_spawn_resolver(Arc::new(
        |command, _, _| {
            Some(if command == "cursor" {
                "C:\\Program Files\\Cursor\\bin\\cursor.CMD".into()
            } else {
                command.into()
            })
        },
    ));
    launcher
        .launch_editor_by_name("cursor", "C:\\workspace with spaces\\src\\index.ts:12:4", false)
        .await
        .unwrap();
    let spawned = spawner.last();
    assert_eq!(spawned.command, "^\"C:\\Program^ Files\\Cursor\\bin\\cursor.CMD^\"");
    assert_eq!(
        spawned.args,
        strings(&["^\"--classic^\"", "^\"--goto^\"", "^\"C:\\workspace^ with^ spaces\\src\\index.ts:12:4^\""])
    );
    assert!(spawned.shell);
}

#[test]
fn line_column_style_splits_the_position() {
    let webstorm = find_editor("webstorm").unwrap();
    assert_eq!(
        resolve_command_editor_args(webstorm, "/w/file.ts:12:4"),
        strings(&["--line", "12", "--column", "4", "/w/file.ts"])
    );
    assert_eq!(resolve_command_editor_args(webstorm, "/w/file.ts:12"), strings(&["--line", "12", "/w/file.ts"]));
    assert_eq!(resolve_command_editor_args(webstorm, "/w/file.ts"), strings(&["/w/file.ts"]));
    assert_eq!(resolve_command_editor_args(webstorm, ":12"), strings(&[":12"]));
    let zed = find_editor("zed").unwrap();
    assert_eq!(resolve_command_editor_args(zed, "/w/file.ts:12:4"), strings(&["/w/file.ts:12:4"]));
}

#[tokio::test]
async fn reveals_a_file_in_finder_with_open_r_on_macos() {
    let bin = temp_dir("zc-editors-");
    executables(&path_of(&bin), &["open"]);
    let spawner = FakeSpawner::new(None);
    launcher(NodePlatform::Darwin, &[("PATH", &path_of(&bin))], &spawner)
        .launch_editor_by_name("file-manager", "/workspace/media/linux-mini-v2.mp4", true)
        .await
        .unwrap();
    let spawned = spawner.last();
    assert_eq!(spawned.command, "open");
    assert_eq!(spawned.args, strings(&["-R", "/workspace/media/linux-mini-v2.mp4"]));
}

#[tokio::test]
async fn reveals_a_file_in_file_explorer_through_powershell_on_windows() {
    let bin = temp_dir("zc-editors-");
    let bin_path = path_of(&bin);
    std::fs::write(bin.path().join("explorer.CMD"), "@echo off\r\n").unwrap();
    // `${SYSTEMROOT}\System32\...` uses Windows separators: on this file system it is one name.
    let system_root = format!("{bin_path}/system-root");
    let powershell = format!("{system_root}\\System32\\WindowsPowerShell\\v1.0\\powershell.exe");
    std::fs::create_dir_all(&system_root).unwrap();
    std::fs::write(&powershell, "").unwrap();
    let spawner = FakeSpawner::new(None);
    let launcher = launcher(
        NodePlatform::Win32,
        &[("PATH", &bin_path), ("PATHEXT", ".COM;.EXE;.BAT;.CMD"), ("SYSTEMROOT", &system_root)],
        &spawner,
    );
    launcher
        .launch_editor_by_name("file-manager", "C:/workspace with spaces/media/author's clip.mp4", true)
        .await
        .unwrap();
    assert_eq!(launcher.resolve_file_manager_reveal_kind().await, Some(FileManagerRevealKind::FileExplorer));
    let spawned = spawner.last();
    assert_eq!(spawned.command, powershell);
    assert_eq!(spawned.args[..5], strings(&POWERSHELL_ARGUMENTS_PREFIX));
    assert_eq!(
        decode_encoded_command(&spawned),
        "$ProgressPreference = 'SilentlyContinue'; Start-Process 'explorer.exe' -ArgumentList ('/select,\"' + 'C:\\workspace with spaces\\media\\author''s clip.mp4' + '\"')"
    );
    assert!(!spawned.shell);
}

#[tokio::test]
async fn does_not_advertise_reveal_on_windows_when_powershell_is_missing() {
    let bin = temp_dir("zc-editors-");
    let bin_path = path_of(&bin);
    std::fs::write(bin.path().join("explorer.CMD"), "@echo off\r\n").unwrap();
    let spawner = FakeSpawner::new(None);
    let launcher = launcher(
        NodePlatform::Win32,
        &[
            ("PATH", &bin_path),
            ("PATHEXT", ".COM;.EXE;.BAT;.CMD"),
            ("SYSTEMROOT", &format!("{bin_path}/missing-system-root")),
        ],
        &spawner,
    );
    assert_eq!(launcher.resolve_file_manager_reveal_kind().await, None);
    assert!(launcher.resolve_available_editors().await.contains(&EditorId::FileManager));
}

const WSL: [(&str, &str); 2] = [("WSL_DISTRO_NAME", "Ubuntu-24.04"), ("WSL_INTEROP", "/run/WSL/1_interop")];

fn wsl_env<'a>(bin: &'a str, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut env = vec![("PATH", bin)];
    env.extend(WSL);
    env.extend_from_slice(extra);
    env
}

#[tokio::test]
async fn reveals_a_wsl_file_in_windows_file_explorer_through_its_unc_path() {
    let bin = temp_dir("zc-editors-");
    let bin_path = path_of(&bin);
    executables(&bin_path, &["explorer.exe", "powershell.exe", "xdg-open"]);
    let spawner = FakeSpawner::new(None);
    let launcher = launcher(NodePlatform::Linux, &wsl_env(&bin_path, &[]), &spawner);
    assert_eq!(launcher.resolve_file_manager_reveal_kind().await, Some(FileManagerRevealKind::FileExplorer));
    assert!(launcher.resolve_available_editors().await.contains(&EditorId::FileManager));
    launcher
        .launch_editor_by_name("file-manager", "/home/t3/workspace/media/clip.mp4", true)
        .await
        .unwrap();
    let spawned = spawner.last();
    assert_eq!(spawned.command, "powershell.exe");
    assert_eq!(
        decode_encoded_command(&spawned),
        "$ProgressPreference = 'SilentlyContinue'; Start-Process 'explorer.exe' -ArgumentList ('/select,\"' + '\\\\wsl.localhost\\Ubuntu-24.04\\home\\t3\\workspace\\media\\clip.mp4' + '\"')"
    );
    // Plain open goes to Explorer with the UNC path.
    launcher.launch_editor_by_name("file-manager", "/home/t3/workspace", false).await.unwrap();
    assert_eq!(
        spawner.last(),
        DetachedLaunch {
            command: "explorer.exe".into(),
            args: strings(&["\\\\wsl.localhost\\Ubuntu-24.04\\home\\t3\\workspace"]),
            shell: false
        }
    );
}

#[tokio::test]
async fn does_not_advertise_reveal_from_wsl_when_interop_powershell_is_missing() {
    let bin = temp_dir("zc-editors-");
    let bin_path = path_of(&bin);
    stub(&format!("{bin_path}/explorer.exe"), 0o755);
    let spawner = FakeSpawner::new(None);
    let launcher = launcher(NodePlatform::Linux, &wsl_env(&bin_path, &[]), &spawner);
    assert!(launcher.resolve_available_editors().await.contains(&EditorId::FileManager));
    assert_eq!(launcher.resolve_file_manager_reveal_kind().await, None);
}

#[tokio::test]
async fn reveals_through_the_linux_file_manager_when_wsl_lacks_interop_powershell() {
    let bin = temp_dir("zc-editors-");
    let bin_path = path_of(&bin);
    executables(&bin_path, &["explorer.exe", "xdg-open", "xdg-mime"]);
    let spawner = FakeSpawner::new(Some(Probe::Done(0, "org.gnome.Nautilus.desktop\n")));
    let launcher = launcher(NodePlatform::Linux, &wsl_env(&bin_path, &[("DISPLAY", ":0")]), &spawner);
    assert_eq!(launcher.resolve_file_manager_reveal_kind().await, Some(FileManagerRevealKind::Files));
    launcher
        .launch_editor_by_name("file-manager", "/home/t3/workspace/media/clip.mp4", true)
        .await
        .unwrap();
    let spawned = spawner.spawned();
    let launch = spawned.iter().find(|launch| launch.command == "xdg-open").unwrap();
    assert_eq!(launch.args, strings(&["/home/t3/workspace/media"]));
    assert!(!spawned.iter().any(|launch| launch.command == "explorer.exe"));
}

#[tokio::test]
async fn falls_back_to_the_linux_file_manager_when_wsl_lacks_the_explorer_bridge() {
    let bin = temp_dir("zc-editors-");
    let bin_path = path_of(&bin);
    executables(&bin_path, &["xdg-open", "xdg-mime"]);
    let spawner = FakeSpawner::new(Some(Probe::Done(0, "org.gnome.Nautilus.desktop\n")));
    let launcher = launcher(NodePlatform::Linux, &wsl_env(&bin_path, &[("DISPLAY", ":0")]), &spawner);
    assert!(launcher.resolve_available_editors().await.contains(&EditorId::FileManager));
    assert_eq!(launcher.resolve_file_manager_reveal_kind().await, Some(FileManagerRevealKind::Files));
    launcher
        .launch_editor_by_name("file-manager", "/home/t3/workspace/media/clip.mp4", true)
        .await
        .unwrap();
    let spawned = spawner.spawned();
    let launch = spawned.iter().find(|launch| launch.command == "xdg-open").unwrap();
    assert_eq!(launch.args, strings(&["/home/t3/workspace/media"]));
}

#[tokio::test]
async fn falls_back_to_opening_the_containing_directory_for_wsl_paths_explorer_cannot_select() {
    let bin = temp_dir("zc-editors-");
    let bin_path = path_of(&bin);
    executables(&bin_path, &["explorer.exe", "powershell.exe"]);
    let spawner = FakeSpawner::new(None);
    launcher(NodePlatform::Linux, &wsl_env(&bin_path, &[]), &spawner)
        .launch_editor_by_name("file-manager", "/home/t3/work \"quoted\"/clip.mp4", true)
        .await
        .unwrap();
    let spawned = spawner.last();
    assert_eq!(spawned.command, "explorer.exe");
    assert_eq!(spawned.args, strings(&["\\\\wsl.localhost\\Ubuntu-24.04\\home\\t3\\work \"quoted\""]));
}

#[tokio::test]
async fn reveals_by_opening_the_containing_directory_on_linux() {
    let bin = temp_dir("zc-editors-");
    let bin_path = path_of(&bin);
    executables(&bin_path, &["xdg-open", "xdg-mime"]);
    let spawner = FakeSpawner::new(Some(Probe::Done(0, "org.gnome.Nautilus.desktop\n")));
    launcher(NodePlatform::Linux, &[("PATH", &bin_path), ("DISPLAY", ":0")], &spawner)
        .launch_editor_by_name("file-manager", "/workspace/media/linux-mini-v2.mp4", true)
        .await
        .unwrap();
    let spawned = spawner.spawned();
    let launch = spawned.iter().find(|launch| launch.command == "xdg-open").unwrap();
    assert_eq!(launch.args, strings(&["/workspace/media"]));
}

#[tokio::test]
async fn does_not_advertise_a_linux_file_manager_without_a_graphical_session() {
    let bin = temp_dir("zc-editors-");
    let bin_path = path_of(&bin);
    executables(&bin_path, &["xdg-open"]);
    let spawner = FakeSpawner::new(None);
    let editors = launcher(NodePlatform::Linux, &[("PATH", &bin_path)], &spawner)
        .resolve_available_editors()
        .await;
    assert!(!editors.contains(&EditorId::FileManager));
}

#[tokio::test]
async fn advertises_a_linux_file_manager_when_a_directory_handler_is_installed() {
    let bin = temp_dir("zc-editors-");
    let bin_path = path_of(&bin);
    executables(&bin_path, &["xdg-open", "xdg-mime"]);
    let spawner = FakeSpawner::new(Some(Probe::Done(0, "org.gnome.Nautilus.desktop\n")));
    let editors = launcher(NodePlatform::Linux, &[("PATH", &bin_path), ("DISPLAY", ":0")], &spawner)
        .resolve_available_editors()
        .await;
    assert!(editors.contains(&EditorId::FileManager));
    assert_eq!(
        spawner.probes.lock().unwrap().last().cloned(),
        Some(("xdg-mime".to_owned(), strings(&["query", "default", "inode/directory"])))
    );
}

#[tokio::test]
async fn does_not_advertise_a_linux_file_manager_without_a_directory_handler() {
    let bin = temp_dir("zc-editors-");
    let bin_path = path_of(&bin);
    executables(&bin_path, &["xdg-open", "xdg-mime"]);
    let spawner = FakeSpawner::new(Some(Probe::Done(0, "")));
    let editors = launcher(NodePlatform::Linux, &[("PATH", &bin_path), ("DISPLAY", ":0")], &spawner)
        .resolve_available_editors()
        .await;
    assert!(!editors.contains(&EditorId::FileManager));
}

#[tokio::test]
async fn does_not_advertise_a_linux_file_manager_when_the_handler_query_fails() {
    let bin = temp_dir("zc-editors-");
    let bin_path = path_of(&bin);
    executables(&bin_path, &["xdg-open", "xdg-mime"]);
    let spawner = FakeSpawner::new(Some(Probe::Done(47, "org.gnome.Nautilus.desktop\n")));
    let editors = launcher(NodePlatform::Linux, &[("PATH", &bin_path), ("DISPLAY", ":0")], &spawner)
        .resolve_available_editors()
        .await;
    assert!(!editors.contains(&EditorId::FileManager));
}

#[tokio::test(start_paused = true)]
async fn a_stalled_handler_probe_drops_only_the_file_manager() {
    let bin = temp_dir("zc-editors-");
    let bin_path = path_of(&bin);
    executables(&bin_path, &["xdg-open", "xdg-mime", "code"]);
    let spawner = FakeSpawner::new(Some(Probe::Stall));
    let editors = launcher(NodePlatform::Linux, &[("PATH", &bin_path), ("DISPLAY", ":0")], &spawner)
        .resolve_available_editors()
        .await;
    assert!(editors.contains(&EditorId::Vscode));
    assert!(!editors.contains(&EditorId::FileManager));
}

#[tokio::test]
async fn does_not_advertise_a_linux_file_manager_when_xdg_mime_is_missing() {
    let bin = temp_dir("zc-editors-");
    let bin_path = path_of(&bin);
    executables(&bin_path, &["xdg-open"]);
    let spawner = FakeSpawner::new(Some(Probe::Done(0, "org.gnome.Nautilus.desktop\n")));
    let editors = launcher(NodePlatform::Linux, &[("PATH", &bin_path), ("DISPLAY", ":0")], &spawner)
        .resolve_available_editors()
        .await;
    assert!(!editors.contains(&EditorId::FileManager));
    assert!(spawner.probes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn discovers_editors_through_the_service_api() {
    let bin = temp_dir("zc-editors-");
    std::fs::write(bin.path().join("code.CMD"), "@echo off\r\n").unwrap();
    std::fs::write(bin.path().join("explorer.CMD"), "@echo off\r\n").unwrap();
    let spawner = FakeSpawner::new(None);
    let editors = launcher(NodePlatform::Win32, &[("PATH", &path_of(&bin)), ("PATHEXT", ".COM;.EXE;.BAT;.CMD")], &spawner)
        .resolve_available_editors()
        .await;
    assert!(editors.contains(&EditorId::Vscode));
    assert!(editors.contains(&EditorId::FileManager));
}

#[tokio::test]
async fn discovers_and_launches_editors_outside_path() {
    let cases: [(NodePlatform, &str, &str, &[&str]); 13] = [
        (
            NodePlatform::Darwin,
            "Applications/Antigravity IDE.app/Contents/Resources/app/bin/antigravity-ide",
            "antigravity",
            &["--goto", "/workspace with spaces/file.ts:12:4"],
        ),
        (
            NodePlatform::Linux,
            ".local/bin/antigravity-ide",
            "antigravity",
            &["--goto", "/workspace with spaces/file.ts:12:4"],
        ),
        (
            NodePlatform::Darwin,
            "Applications/Cursor.app/Contents/Resources/app/bin/code",
            "cursor",
            &["--classic", "--goto", "/workspace with spaces/file.ts:12:4"],
        ),
        (
            NodePlatform::Darwin,
            "Applications/Kiro.app/Contents/Resources/app/bin/code",
            "kiro",
            &["--goto", "/workspace with spaces/file.ts:12:4"],
        ),
        (
            NodePlatform::Darwin,
            "Applications/WebStorm.app/Contents/MacOS/webstorm",
            "webstorm",
            &["--line", "12", "--column", "4", "/workspace with spaces/file.ts"],
        ),
        (
            NodePlatform::Darwin,
            "Applications/Zed.app/Contents/MacOS/cli",
            "zed",
            &["/workspace with spaces/file.ts:12:4"],
        ),
        (
            NodePlatform::Win32,
            "Programs/Cursor/resources/app/bin/cursor.cmd",
            "cursor",
            &["^\"--classic^\"", "^\"--goto^\"", "^\"/workspace^ with^ spaces/file.ts:12:4^\""],
        ),
        (
            NodePlatform::Win32,
            "Programs/Microsoft VS Code/bin/code.cmd",
            "vscode",
            &["^\"--goto^\"", "^\"/workspace^ with^ spaces/file.ts:12:4^\""],
        ),
        (
            NodePlatform::Win32,
            "Programs/JetBrains/WebStorm 2026.2/bin/webstorm64.exe",
            "webstorm",
            &["--line", "12", "--column", "4", "/workspace with spaces/file.ts"],
        ),
        (
            NodePlatform::Win32,
            "Programs/WebStorm/bin/webstorm64.exe",
            "webstorm",
            &["--line", "12", "--column", "4", "/workspace with spaces/file.ts"],
        ),
        (NodePlatform::Win32, "Programs/Zed/bin/zed.exe", "zed", &["/workspace with spaces/file.ts:12:4"]),
        (
            NodePlatform::Linux,
            ".local/share/JetBrains/Toolbox/scripts/idea",
            "idea",
            &["--line", "12", "--column", "4", "/workspace with spaces/file.ts"],
        ),
        (NodePlatform::Linux, ".local/bin/zed", "zed", &["/workspace with spaces/file.ts:12:4"]),
    ];
    for (platform, install_path, editor, args) in cases {
        let home = temp_dir("zc-editor installs-");
        let home_path = path_of(&home);
        let executable = format!("{home_path}/{install_path}");
        stub(&executable, 0o755);
        let spawner = FakeSpawner::new(None);
        let empty = format!("{home_path}/empty");
        let launcher = launcher(platform, &[("HOME", &home_path), ("LOCALAPPDATA", &home_path), ("PATH", &empty)], &spawner);
        let editors = launcher.resolve_available_editors().await;
        assert!(editors.iter().any(|id| id.as_str() == editor), "{editor} on {platform:?}: {editors:?}");
        launcher
            .launch_editor_by_name(editor, "/workspace with spaces/file.ts:12:4", false)
            .await
            .unwrap();
        let spawned = spawner.last();
        let is_cmd = executable.ends_with(".cmd");
        let expected_command = if is_cmd {
            format!("^\"{}^\"", executable.replace(' ', "^ "))
        } else {
            executable.clone()
        };
        assert_eq!(spawned.command, expected_command, "{editor} on {platform:?}");
        assert_eq!(spawned.args, strings(args), "{editor} on {platform:?}");
        assert_eq!(spawned.shell, is_cmd);
    }
}

#[tokio::test]
async fn does_not_report_the_agy_cli_as_the_antigravity_ide() {
    for (platform, install_path, on_path) in [
        (NodePlatform::Darwin, ".local/bin/agy", true),
        (NodePlatform::Linux, ".local/bin/agy", false),
        (NodePlatform::Win32, "agy/bin/agy.cmd", true),
    ] {
        let home = temp_dir("zc-agy-cli-");
        let home_path = path_of(&home);
        let executable = format!("{home_path}/{install_path}");
        stub(&executable, 0o755);
        let path = if on_path {
            crate::paths::dirname(&executable)
        } else {
            format!("{home_path}/empty")
        };
        let spawner = FakeSpawner::new(None);
        let editors = launcher(
            platform,
            &[
                ("HOME", &home_path),
                ("LOCALAPPDATA", &home_path),
                ("PATH", &path),
                ("PATHEXT", ".COM;.EXE;.BAT;.CMD"),
            ],
            &spawner,
        )
        .resolve_available_editors()
        .await;
        assert!(!editors.contains(&EditorId::Antigravity), "{platform:?}");
    }
}

#[tokio::test]
async fn ignores_unusable_app_bundles_and_keeps_path_launchers_first() {
    let home = temp_dir("zc-editor-priority-");
    let home_path = path_of(&home);
    let executable = format!("{home_path}/Applications/Cursor.app/Contents/Resources/app/bin/code");
    let path = format!("{home_path}/bin");
    let env = [("HOME", home_path.as_str()), ("PATH", path.as_str())];
    let spawner = FakeSpawner::new(None);
    let discover = || async { launcher(NodePlatform::Darwin, &env, &spawner).resolve_available_editors().await };
    let before = discover().await;
    std::fs::create_dir_all(&executable).unwrap();
    assert_eq!(discover().await, before, "a directory is not an executable");
    std::fs::remove_dir(&executable).unwrap();
    stub(&executable, 0o644);
    assert_eq!(discover().await, before, "a non-executable file is not an executable");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    executables(&path, &["cursor"]);
    let with_usr_bin = format!("{path}:/usr/bin");
    let launcher = launcher(NodePlatform::Darwin, &[("HOME", &home_path), ("PATH", &with_usr_bin)], &spawner);
    launcher.launch_editor_by_name("cursor", "/workspace", false).await.unwrap();
    assert_eq!(spawner.last().command, "cursor");
}

#[tokio::test(start_paused = true)]
async fn memoizes_editor_discovery_and_refreshes_after_the_cache_window() {
    let bin = temp_dir("zc-editors-memo-");
    let bin_path = path_of(&bin);
    executables(&bin_path, &["code"]);
    let spawner = FakeSpawner::new(None);
    let launcher = launcher(NodePlatform::Linux, &[("PATH", &bin_path)], &spawner);
    let first = launcher.resolve_available_editors().await;
    assert!(first.contains(&EditorId::Vscode));
    std::fs::remove_file(bin.path().join("code")).unwrap();
    // Past the command-resolution cache (30 s) but inside the discovery window: memoized.
    tokio::time::advance(Duration::from_secs(31)).await;
    assert_eq!(launcher.resolve_available_editors().await, first);
    // Past the discovery window: rescanned.
    tokio::time::advance(Duration::from_secs(30)).await;
    assert!(!launcher.resolve_available_editors().await.contains(&EditorId::Vscode));
}

#[tokio::test(start_paused = true)]
async fn rescans_after_an_interrupted_discovery_instead_of_caching_it() {
    let bin = temp_dir("zc-editors-interrupt-");
    let bin_path = path_of(&bin);
    executables(&bin_path, &["xdg-open", "xdg-mime", "code"]);
    let spawner = FakeSpawner::new(Some(Probe::Stall));
    let launcher = launcher(NodePlatform::Linux, &[("PATH", &bin_path), ("DISPLAY", ":0")], &spawner);
    // A client disconnecting mid-scan drops the discovery future.
    assert!(tokio::time::timeout(Duration::from_millis(500), launcher.resolve_available_editors())
        .await
        .is_err());
    *spawner.probe.lock().unwrap() = Some(Probe::Done(0, "org.gnome.Nautilus.desktop\n"));
    let probes_before = spawner.probes.lock().unwrap().len();
    let editors = launcher.resolve_available_editors().await;
    assert!(editors.contains(&EditorId::Vscode));
    assert!(editors.contains(&EditorId::FileManager));
    assert!(spawner.probes.lock().unwrap().len() > probes_before, "the next call scans again");
}

#[tokio::test]
async fn rejects_unknown_editors_through_the_service_api() {
    let spawner = FakeSpawner::new(None);
    let error = launcher(NodePlatform::Linux, &[("PATH", "")], &spawner)
        .launch_editor_by_name("missing-editor", "/tmp/workspace", false)
        .await
        .unwrap_err();
    match &error {
        ExternalLauncherError::ExternalLauncherUnknownEditorError(error) => assert_eq!(error.editor, "missing-editor"),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(external_launcher_error_message(&error), "Unknown editor: missing-editor");
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        json!({ "_tag": "ExternalLauncherUnknownEditorError", "editor": "missing-editor" })
    );
}

#[tokio::test]
async fn reports_missing_commands_and_unsupported_file_managers() {
    let spawner = FakeSpawner::new(None);
    let launcher = launcher(NodePlatform::Linux, &[("PATH", "/nonexistent")], &spawner);
    let error = launcher.launch_editor_by_name("vscode", "/w", false).await.unwrap_err();
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        json!({ "_tag": "ExternalLauncherCommandNotFoundError", "editor": "vscode", "command": "code" })
    );
    // No graphical session: no Linux file manager.
    let error = launcher.launch_editor_by_name("file-manager", "/w", false).await.unwrap_err();
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        json!({ "_tag": "ExternalLauncherUnsupportedEditorError", "editor": "file-manager" })
    );
}

#[test]
fn encodes_powershell_commands_as_utf16le_base64() {
    assert_eq!(encode_utf16le_base64("ab"), "YQBiAA==");
    assert_eq!(
        build_file_explorer_reveal_powershell_source("explorer.exe", "C:\\it's"),
        "$ProgressPreference = 'SilentlyContinue'; Start-Process 'explorer.exe' -ArgumentList ('/select,\"' + 'C:\\it''s' + '\"')"
    );
}
