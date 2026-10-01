//! The editor catalogue (`EDITORS` in `packages/contracts/src/editor.ts`) and the install-folder
//! lookup outside `PATH` (`resolveEditorCommand` in `packages/shared/src/editor.ts`).

use zc_contracts::{EditorId, EditorLaunchStyle};

use super::command::{CommandResolver, LauncherEnv};
use crate::paths::join;
use crate::platform::NodePlatform;

/// One `EDITORS` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorDefinition {
    pub id: EditorId,
    pub label: &'static str,
    /// `null` for the file manager.
    pub commands: Option<&'static [&'static str]>,
    pub base_args: &'static [&'static str],
    pub launch_style: EditorLaunchStyle,
    pub remote_scheme: Option<&'static str>,
}

const fn editor(id: EditorId, label: &'static str, commands: &'static [&'static str], launch_style: EditorLaunchStyle) -> EditorDefinition {
    EditorDefinition {
        id,
        label,
        commands: Some(commands),
        base_args: &[],
        launch_style,
        remote_scheme: None,
    }
}

const fn jetbrains(id: EditorId, label: &'static str, commands: &'static [&'static str]) -> EditorDefinition {
    editor(id, label, commands, EditorLaunchStyle::LineColumn)
}

/// `EDITORS`, in declaration order (discovery reports them in this order).
pub const EDITORS: &[EditorDefinition] = &[
    EditorDefinition {
        id: EditorId::Cursor,
        label: "Cursor",
        commands: Some(&["cursor"]),
        // File and workspace opens must target the IDE even when the Agents Window is active.
        base_args: &["--classic"],
        launch_style: EditorLaunchStyle::Goto,
        remote_scheme: Some("cursor"),
    },
    editor(EditorId::Trae, "Trae", &["trae"], EditorLaunchStyle::Goto),
    EditorDefinition {
        id: EditorId::Kiro,
        label: "Kiro",
        commands: Some(&["kiro"]),
        base_args: &["ide"],
        launch_style: EditorLaunchStyle::Goto,
        remote_scheme: None,
    },
    EditorDefinition {
        remote_scheme: Some("vscode"),
        ..editor(EditorId::Vscode, "VS Code", &["code"], EditorLaunchStyle::Goto)
    },
    EditorDefinition {
        remote_scheme: Some("vscode-insiders"),
        ..editor(EditorId::VscodeInsiders, "VS Code Insiders", &["code-insiders"], EditorLaunchStyle::Goto)
    },
    EditorDefinition {
        remote_scheme: Some("vscodium"),
        ..editor(EditorId::Vscodium, "VSCodium", &["codium"], EditorLaunchStyle::Goto)
    },
    EditorDefinition {
        remote_scheme: Some("zed"),
        ..editor(EditorId::Zed, "Zed", &["zed", "zeditor"], EditorLaunchStyle::DirectPath)
    },
    // `agy` is the standalone Antigravity CLI, not the IDE. The IDE bundle ships
    // `antigravity-ide`, so it comes first for install-folder lookups.
    editor(EditorId::Antigravity, "Antigravity", &["antigravity-ide", "agy-ide"], EditorLaunchStyle::Goto),
    jetbrains(EditorId::Idea, "IntelliJ IDEA", &["idea"]),
    jetbrains(EditorId::Aqua, "Aqua", &["aqua"]),
    jetbrains(EditorId::Clion, "CLion", &["clion"]),
    jetbrains(EditorId::Datagrip, "DataGrip", &["datagrip"]),
    jetbrains(EditorId::Dataspell, "DataSpell", &["dataspell"]),
    jetbrains(EditorId::Goland, "GoLand", &["goland"]),
    jetbrains(EditorId::Phpstorm, "PhpStorm", &["phpstorm"]),
    jetbrains(EditorId::Pycharm, "PyCharm", &["pycharm"]),
    jetbrains(EditorId::Rider, "Rider", &["rider"]),
    jetbrains(EditorId::Rubymine, "RubyMine", &["rubymine"]),
    jetbrains(EditorId::Rustrover, "RustRover", &["rustrover"]),
    jetbrains(EditorId::Webstorm, "WebStorm", &["webstorm"]),
    EditorDefinition {
        id: EditorId::FileManager,
        label: "File Manager",
        commands: None,
        base_args: &[],
        launch_style: EditorLaunchStyle::DirectPath,
        remote_scheme: None,
    },
];

/// The catalogue entry of an editor id (`EDITORS.find`).
pub fn find_editor(id: &str) -> Option<&'static EditorDefinition> {
    EDITORS.iter().find(|editor| editor.id.as_str() == id)
}

/// Application folder names that differ from the label.
fn install_names(id: EditorId) -> Option<&'static [&'static str]> {
    Some(match id {
        EditorId::Vscode => &["Visual Studio Code"],
        EditorId::VscodeInsiders => &["Visual Studio Code - Insiders"],
        // `Antigravity.app` is the separate Hub, not the IDE.
        EditorId::Antigravity => &["Antigravity IDE"],
        EditorId::Idea => &["IntelliJ IDEA", "IntelliJ IDEA CE", "IntelliJ IDEA Ultimate"],
        EditorId::Pycharm => &["PyCharm", "PyCharm CE"],
        EditorId::Rider => &["Rider", "JetBrains Rider"],
        _ => return None,
    })
}

/// A resolved editor command: the executable (a `PATH` name or an absolute path) and the
/// arguments that precede the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorCommand {
    pub command: String,
    pub base_args: Vec<String>,
}

fn list_directory(directory: &str) -> Vec<String> {
    std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// `resolveEditorCommand(editor, env)`: the first of the editor's commands on `PATH`, else the
/// first executable among the platform's install locations (app bundles, JetBrains Toolbox
/// scripts, Program Files, `~/.local/bin`, …).
pub fn resolve_editor_command(editor: &EditorDefinition, env: &LauncherEnv, platform: NodePlatform, resolver: &CommandResolver) -> Option<EditorCommand> {
    let commands = editor.commands?;
    let base_args: Vec<String> = editor.base_args.iter().map(|arg| (*arg).to_owned()).collect();
    for command in commands {
        if resolver.is_command_available(command, env, platform) {
            return Some(EditorCommand {
                command: (*command).to_owned(),
                base_args,
            });
        }
    }

    let home = env.truthy("HOME");
    let label = [editor.label];
    let names: &[&str] = install_names(editor.id).unwrap_or(&label);
    let command = commands[0];
    let is_jetbrains = editor.launch_style == EditorLaunchStyle::LineColumn;
    let mut candidates: Vec<String> = Vec::new();

    match platform {
        NodePlatform::Darwin => {
            let mut roots: Vec<String> = home.map(|home| join(home, "Applications")).into_iter().collect();
            roots.push("/Applications".into());
            for root in &roots {
                for name in names {
                    let contents = join(&join(root, &format!("{name}.app")), "Contents");
                    if is_jetbrains || editor.id == EditorId::Zed {
                        let binary = if editor.id == EditorId::Zed { "cli" } else { command };
                        candidates.push(join(&join(&contents, "MacOS"), binary));
                    } else {
                        candidates.push(join(&join(&contents, "Resources/app/bin"), command));
                        candidates.push(join(&contents, "Resources/app/bin/code"));
                    }
                }
            }
            if let (Some(home), true) = (home, is_jetbrains) {
                candidates.push(join(&join(home, "Library/Application Support/JetBrains/Toolbox/scripts"), command));
            }
        }
        NodePlatform::Win32 => {
            let mut roots: Vec<String> = env.truthy("LOCALAPPDATA").map(|dir| join(dir, "Programs")).into_iter().collect();
            for key in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
                if let Some(root) = env.truthy(key) {
                    roots.push(root.to_owned());
                }
            }
            if is_jetbrains {
                if let Some(local) = env.truthy("LOCALAPPDATA") {
                    candidates.push(join(&join(local, "JetBrains/Toolbox/scripts"), &format!("{command}.cmd")));
                }
                let directories: Vec<String> = roots.iter().flat_map(|root| [root.clone(), join(root, "JetBrains")]).collect();
                for directory in &directories {
                    for entry in list_directory(directory) {
                        if names.iter().any(|name| entry == *name || entry.starts_with(&format!("{name} "))) {
                            let bin = join(&join(directory, &entry), "bin");
                            candidates.push(join(&bin, &format!("{command}64.exe")));
                            candidates.push(join(&bin, &format!("{command}.exe")));
                        }
                    }
                }
            } else {
                let name = match editor.id {
                    EditorId::Vscode => "Microsoft VS Code",
                    EditorId::VscodeInsiders => "Microsoft VS Code Insiders",
                    _ => editor.label,
                };
                for root in &roots {
                    let base = join(root, name);
                    candidates.push(join(&join(&base, "resources/app/bin"), &format!("{command}.cmd")));
                    candidates.push(join(&base, "resources/app/bin/code.cmd"));
                    candidates.push(join(&join(&base, "bin"), &format!("{command}.cmd")));
                    candidates.push(join(&base, "bin/code.cmd"));
                    if editor.id == EditorId::Zed {
                        candidates.push(join(&join(&base, "bin"), "zed.exe"));
                        candidates.push(join(&base, "zed.exe"));
                    }
                }
            }
        }
        NodePlatform::Linux => {
            let mut directories: Vec<String> = home.map(|home| join(home, ".local/bin")).into_iter().collect();
            directories.extend(["/usr/local/bin".to_owned(), "/usr/bin".to_owned(), "/snap/bin".to_owned()]);
            if is_jetbrains {
                let data_home = env
                    .truthy("XDG_DATA_HOME")
                    .map(str::to_owned)
                    .or_else(|| home.map(|home| join(home, ".local/share")));
                if let Some(data_home) = data_home {
                    directories.push(join(&data_home, "JetBrains/Toolbox/scripts"));
                }
            }
            for directory in &directories {
                for name in commands {
                    candidates.push(join(directory, name));
                }
            }
        }
        NodePlatform::Other(_) => {}
    }

    for candidate in candidates {
        if resolver.is_command_available(&candidate, env, platform) {
            let base_args = if editor.id == EditorId::Kiro && matches!(platform, NodePlatform::Darwin | NodePlatform::Win32) {
                Vec::new()
            } else {
                base_args
            };
            return Some(EditorCommand { command: candidate, base_args });
        }
    }
    None
}
