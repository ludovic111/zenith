//! zenith.app: zenith, the app for coding with agents, in a native window (GPUI).
//!
//! The window is a client of the local zenith server (`crates/zenith-code`, a LaunchAgent on
//! 127.0.0.1:4747), which runs the agents, so closing the window never stops them. It talks
//! to the server through `zenith-client` (WebSocket RPC, a bearer session kept 0600 in
//! `~/.zenith/app`), and derives what it shows with `zenith-model`. Everything a person can do
//! here is also a command of `zenith-commands`, for `zenith-cli` and `zenith-mcp`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod assets;
mod composer;
mod native;
mod palette;
mod prefs;
mod runtime;
mod sessions;
mod settings;
mod sidebar;
mod store;
mod terminal;
mod theme;
mod thread_view;
mod ui;
mod update;
mod workspace;

use gpui::{point, px, size, App, AppContext, Application, Bounds, TitlebarOptions, WindowBackgroundAppearance, WindowBounds, WindowOptions};

use crate::theme::{ActiveTheme, Theme};
use crate::workspace::Workspace;

fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("zenith {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let app = Application::new().with_assets(assets::Assets);
    // Clicking the Dock icon brings the window back (closing it only hides zenith).
    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            open_window(prefs::Prefs::load(), cx);
        }
        cx.activate(true);
    });
    app.run(|cx: &mut App| {
        assets::load_fonts(cx);
        let prefs = prefs::Prefs::load();
        let reduce_transparency = theme::system_reduces_transparency();
        cx.set_global(Theme::new(theme::Mode::Dark, reduce_transparency));
        actions::bind_keys(cx);
        cx.set_menus(actions::menus());
        store::init(cx);
        register_app_actions(cx);
        open_window(prefs, cx);
        if std::env::var_os("ZENITH_FLOAT").is_none() {
            cx.activate(true);
        }
        ensure_server();
        lsuite_discovery();
        update::check_on_start(cx);
    });
}

fn register_app_actions(cx: &mut App) {
    cx.on_action(|_: &actions::Quit, cx| {
        // The window goes; the server (zenith's bridge) keeps running from login.
        let _ = zenith_commands::lsuite::write_discovery(env!("CARGO_PKG_VERSION"), false);
        cx.quit()
    });
    cx.on_action(|_: &actions::Hide, cx| cx.hide());
    cx.on_action(|_: &actions::HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &actions::ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &actions::ShowServerLog, cx| cx.reveal_path(&zenith_client::local::server_log()));
    cx.on_action(|_: &actions::CheckForUpdates, cx| update::check_now(cx));
    cx.on_action(|_: &actions::About, cx| {
        let version = env!("CARGO_PKG_VERSION");
        if let Some(window) = cx.active_window() {
            let _ = window.update(cx, |_, window, cx| {
                let _answer = window.prompt(
                    gpui::PromptLevel::Info,
                    &format!("zenith {version}"),
                    Some("The app for coding with agents. Part of lsuite, free and open source (MIT).\nlsuite.xyz/zenith"),
                    &["OK"],
                    cx,
                );
            });
        }
    });
}

fn open_window(prefs: prefs::Prefs, cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1440.), px(920.)), cx);
    let reduce_transparency = cx.theme().reduce_transparency;
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("zenith".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(18.), px(18.))),
        }),
        window_min_size: Some(size(px(560.), px(480.))),
        window_background: if reduce_transparency {
            WindowBackgroundAppearance::Opaque
        } else {
            WindowBackgroundAppearance::Transparent
        },
        app_id: Some("dev.zenith.app".into()),
        ..Default::default()
    };
    let opened = cx.open_window(options, move |window, cx| {
        if !reduce_transparency {
            native::add_vibrancy(window);
        }
        cx.new(|cx| Workspace::new(prefs, window, cx))
    });
    if let Err(error) = opened {
        eprintln!("zenith: could not open the window: {error:#}");
    }
}

/// The server's LaunchAgent, when this zenith.app carries the server (a release).
fn ensure_server() {
    std::thread::spawn(|| match zenith_commands::agent::ensure() {
        Ok(true) => eprintln!("zenith: the server's LaunchAgent now runs this app's server"),
        Ok(false) => {}
        Err(error) => eprintln!("zenith: could not set up the server's LaunchAgent: {error:#}"),
    });
}

/// `~/.lsuite/apps/zenith.json`: how the other lsuite apps (and agents) find zenith and drive
/// it (STANDARD.md §4, "Discovery"). Written at every start.
fn lsuite_discovery() {
    std::thread::spawn(|| {
        if let Err(error) = zenith_commands::lsuite::write_discovery(env!("CARGO_PKG_VERSION"), true) {
            eprintln!("zenith: could not write ~/.lsuite/apps/zenith.json: {error:#}");
        }
    });
}
