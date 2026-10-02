//! The window's actions, their shortcuts and the menu bar. Shortcuts follow the web
//! interface where it had one (⌘K palette, ⌘B sidebar, ⌘N new thread, ⌘⇧[ ⌘⇧] previous and
//! next thread, ⌘⇧S settle, ⌘⇧P pin, ⌘U sessions, ⌘J terminal) and macOS otherwise.

use gpui::{actions, App, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType};

use crate::ui::text_area;

actions!(
    zenith,
    [
        Quit,
        About,
        Hide,
        HideOthers,
        ShowAll,
        NewThread,
        AddProject,
        ToggleSidebar,
        OpenPalette,
        OpenSettings,
        OpenSessions,
        CloseWindow,
        Minimize,
        Zoom,
        ToggleFullScreen,
        NextThread,
        PreviousThread,
        OpenInBrowser,
        ShowServerLog,
        CheckForUpdates,
        FocusComposer,
        StopTurn,
        SettleThread,
        PinThread,
        ArchiveThread,
        ReloadConnection,
        ToggleTerminal,
        GitCommit,
        GitCommitPush,
        GitCommitPushPr,
        GitPush,
        GitPull,
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("alt-cmd-h", HideOthers, None),
        KeyBinding::new("cmd-n", NewThread, None),
        KeyBinding::new("cmd-o", AddProject, None),
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        KeyBinding::new("cmd-k", OpenPalette, None),
        KeyBinding::new("cmd-shift-p", OpenPalette, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-u", OpenSessions, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("ctrl-cmd-f", ToggleFullScreen, None),
        KeyBinding::new("cmd-shift-]", NextThread, None),
        KeyBinding::new("cmd-shift-[", PreviousThread, None),
        KeyBinding::new("alt-cmd-o", OpenInBrowser, None),
        KeyBinding::new("cmd-l", FocusComposer, None),
        KeyBinding::new("cmd-.", StopTurn, None),
        KeyBinding::new("cmd-shift-s", SettleThread, None),
        KeyBinding::new("cmd-shift-i", PinThread, None),
        KeyBinding::new("cmd-shift-r", ReloadConnection, None),
        KeyBinding::new("cmd-j", ToggleTerminal, None),
        KeyBinding::new("alt-cmd-c", GitCommit, None),
    ]);
    text_area::bind_keys(cx);
    crate::ui::menu::bind_keys(cx);
}

pub fn menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "zenith".into(),
            items: vec![
                MenuItem::action("About zenith", About),
                MenuItem::action("Check for Updates…", CheckForUpdates),
                MenuItem::separator(),
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Hide zenith", Hide),
                MenuItem::action("Hide Others", HideOthers),
                MenuItem::action("Show All", ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit zenith", Quit),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("New Thread", NewThread),
                MenuItem::action("Add Project…", AddProject),
                MenuItem::separator(),
                MenuItem::action("Open in Browser", OpenInBrowser),
                MenuItem::separator(),
                MenuItem::action("Close Window", CloseWindow),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::os_action("Undo", text_area::Undo, OsAction::Undo),
                MenuItem::os_action("Redo", text_area::Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action("Cut", text_area::Cut, OsAction::Cut),
                MenuItem::os_action("Copy", text_area::Copy, OsAction::Copy),
                MenuItem::os_action("Paste", text_area::Paste, OsAction::Paste),
                MenuItem::os_action("Select All", text_area::SelectAll, OsAction::SelectAll),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Command Palette…", OpenPalette),
                MenuItem::action("Toggle Sidebar", ToggleSidebar),
                MenuItem::separator(),
                MenuItem::action("Sessions & Costs", OpenSessions),
                MenuItem::separator(),
                MenuItem::action("Enter Full Screen", ToggleFullScreen),
            ],
        },
        Menu {
            name: "Thread".into(),
            items: vec![
                MenuItem::action("Focus Composer", FocusComposer),
                MenuItem::action("Stop the Agent", StopTurn),
                MenuItem::separator(),
                MenuItem::action("Previous Thread", PreviousThread),
                MenuItem::action("Next Thread", NextThread),
                MenuItem::separator(),
                MenuItem::action("Pin or Unpin", PinThread),
                MenuItem::action("Settle or Reopen", SettleThread),
                MenuItem::action("Archive", ArchiveThread),
                MenuItem::separator(),
                MenuItem::action("Show or Hide the Terminal", ToggleTerminal),
            ],
        },
        Menu {
            name: "Git".into(),
            items: vec![
                MenuItem::action("Commit", GitCommit),
                MenuItem::action("Commit and Push", GitCommitPush),
                MenuItem::action("Commit, Push and Open a Pull Request", GitCommitPushPr),
                MenuItem::separator(),
                MenuItem::action("Push", GitPush),
                MenuItem::action("Pull", GitPull),
            ],
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Minimize", Minimize),
                MenuItem::action("Zoom", Zoom),
                MenuItem::separator(),
                MenuItem::action("Reconnect to the Server", ReloadConnection),
                MenuItem::action("Show Server Log", ShowServerLog),
            ],
        },
    ]
}
