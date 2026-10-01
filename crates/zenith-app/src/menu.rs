//! The menu bar. The page owns most shortcuts (⌘K, ⌘B, ⌘N, ⌘W for its panels and
//! terminals…), so the items here keep clear of them: Close Window is ⌘⇧W, Reload ⌘⇧R,
//! Open in Browser ⌥⌘O.

use std::sync::Mutex;
use std::thread;

use tauri::menu::{AboutMetadata, Menu, MenuBuilder, MenuEvent, MenuItem, PredefinedMenuItem, Submenu, SubmenuBuilder};
use tauri::{AppHandle, Manager, Runtime, WebviewWindow};
use tauri_plugin_opener::OpenerExt;

use crate::{server, t, MAIN};

/// The page's zoom, from ⌘+ ⌘- ⌘0.
static ZOOM: Mutex<f64> = Mutex::new(1.0);

/// Where Settings… goes.
const SETTINGS: &str = "/settings/general";

fn item<R: Runtime>(app: &AppHandle<R>, id: &str, text: &str, key: Option<&str>) -> tauri::Result<MenuItem<R>> {
    MenuItem::with_id(app, id, text, true, key)
}

pub fn build<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    let about = AboutMetadata {
        name: Some("zenith".into()),
        comments: Some(
            t(
                "Un espace de travail pour Claude Code, Codex et les autres agents de code.",
                "A workspace for Claude Code, Codex and other coding agents.",
            )
            .into(),
        ),
        ..Default::default()
    };
    let zenith = SubmenuBuilder::new(app, "zenith")
        .item(&PredefinedMenuItem::about(app, Some(t("À propos de zenith", "About zenith")), Some(about))?)
        .separator()
        .item(&item(app, "settings", t("Réglages…", "Settings…"), Some("CmdOrCtrl+,"))?)
        .separator()
        .item(&PredefinedMenuItem::services(app, Some(t("Services", "Services")))?)
        .separator()
        .item(&PredefinedMenuItem::hide(app, Some(t("Masquer zenith", "Hide zenith")))?)
        .item(&PredefinedMenuItem::hide_others(app, Some(t("Masquer les autres", "Hide Others")))?)
        .item(&PredefinedMenuItem::show_all(app, Some(t("Tout afficher", "Show All")))?)
        .separator()
        .item(&PredefinedMenuItem::quit(app, Some(t("Quitter zenith", "Quit zenith")))?)
        .build()?;

    let file = SubmenuBuilder::new(app, t("Fichier", "File"))
        .item(&item(app, "close", t("Fermer la fenêtre", "Close Window"), Some("CmdOrCtrl+Shift+W"))?)
        .build()?;

    let edit = SubmenuBuilder::new(app, t("Édition", "Edit"))
        .item(&PredefinedMenuItem::undo(app, Some(t("Annuler", "Undo")))?)
        .item(&PredefinedMenuItem::redo(app, Some(t("Rétablir", "Redo")))?)
        .separator()
        .item(&PredefinedMenuItem::cut(app, Some(t("Couper", "Cut")))?)
        .item(&PredefinedMenuItem::copy(app, Some(t("Copier", "Copy")))?)
        .item(&PredefinedMenuItem::paste(app, Some(t("Coller", "Paste")))?)
        .item(&PredefinedMenuItem::select_all(app, Some(t("Tout sélectionner", "Select All")))?)
        .build()?;

    let view = SubmenuBuilder::new(app, t("Présentation", "View"))
        .item(&item(app, "reload", t("Recharger la page", "Reload Page"), Some("CmdOrCtrl+Shift+R"))?)
        .separator()
        .item(&item(app, "zoom-in", t("Agrandir", "Zoom In"), Some("CmdOrCtrl+Plus"))?)
        .item(&item(app, "zoom-out", t("Réduire", "Zoom Out"), Some("CmdOrCtrl+-"))?)
        .item(&item(app, "zoom-reset", t("Taille réelle", "Actual Size"), Some("CmdOrCtrl+0"))?)
        .separator()
        .item(&PredefinedMenuItem::fullscreen(app, Some(t("Plein écran", "Enter Full Screen")))?)
        .build()?;

    let go = SubmenuBuilder::new(app, t("Aller", "Go"))
        .item(&item(app, "back", t("Précédent", "Back"), Some("CmdOrCtrl+["))?)
        .item(&item(app, "forward", t("Suivant", "Forward"), Some("CmdOrCtrl+]"))?)
        .separator()
        .item(&item(
            app,
            "browser",
            t("Ouvrir dans le navigateur", "Open in Browser"),
            Some("CmdOrCtrl+Alt+O"),
        )?)
        .build()?;

    let window: Submenu<R> = SubmenuBuilder::new(app, t("Fenêtre", "Window"))
        .item(&PredefinedMenuItem::minimize(app, Some(t("Placer dans le Dock", "Minimize")))?)
        .item(&PredefinedMenuItem::maximize(app, Some(t("Réduire/agrandir", "Zoom")))?)
        .build()?;
    #[cfg(target_os = "macos")]
    window.set_as_windows_menu_for_nsapp()?;

    MenuBuilder::new(app).items(&[&zenith, &file, &edit, &view, &go, &window]).build()
}

pub fn on_event<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    let Some(window) = app.get_webview_window(MAIN) else {
        return;
    };
    match event.id().as_ref() {
        "settings" => {
            show(&window);
            // In the app's router (code/apps/web/src/zenith/app.ts), else as a page load.
            let _ = window.eval(format!(
                "if(location.protocol==='http:'&&window.dispatchEvent(new CustomEvent('zenith:navigate',{{detail:'{SETTINGS}',cancelable:true}})))location.assign('{SETTINGS}')"
            ));
        }
        "close" => {
            let _ = window.hide();
        }
        "reload" => {
            let _ = window.eval("location.reload()");
        }
        "back" => {
            let _ = window.eval("history.back()");
        }
        "forward" => {
            let _ = window.eval("history.forward()");
        }
        id @ ("zoom-in" | "zoom-out" | "zoom-reset") => {
            let mut zoom = ZOOM.lock().unwrap();
            *zoom = match id {
                "zoom-in" => (*zoom + 0.1).min(2.0),
                "zoom-out" => (*zoom - 0.1).max(0.5),
                _ => 1.0,
            };
            let _ = window.set_zoom(*zoom);
        }
        "browser" => {
            // The browser has no session of its own: it gets a one-time token too.
            let app = app.clone();
            thread::spawn(move || {
                let url = match server::pairing_token() {
                    Ok(token) => server::url(&format!("/pair#token={token}")),
                    Err(_) => server::url("/"),
                };
                let _ = app.opener().open_url(url, None::<&str>);
            });
        }
        _ => {}
    }
}

/// Brings the window back from the Dock.
pub fn show<R: Runtime>(window: &WebviewWindow<R>) {
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}
