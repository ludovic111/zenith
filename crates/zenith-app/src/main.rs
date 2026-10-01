//! zenith.app: zenith in a window of its own, made with Tauri. The window shows the web app
//! the local zenith server serves (code/apps/web), with the traffic lights over the page's
//! own title bar and the system's light or dark appearance. The page knows it is in
//! zenith.app (code/apps/web/src/zenith/app.ts): it leaves room for the traffic lights, and a
//! press in its title bar moves the window (`shell_drag`, `shell_zoom`). It signs itself in
//! with a one-time token the app mints (`pairing_token`, see PAIRING).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod menu;
mod server;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use tauri::utils::config::WindowEffectsConfig;
use tauri::webview::{NewWindowResponse, PageLoadEvent};
use tauri::window::{Effect, EffectState};
use tauri::{AppHandle, LogicalPosition, Manager, RunEvent, Runtime, TitleBarStyle, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_opener::OpenerExt;

pub const MAIN: &str = "main";

/// Safari's (WKWebView's own lacks it), marked as zenith.app's.
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15 ZenithMac/3.0";

static FRENCH: OnceLock<bool> = OnceLock::new();

/// The French or the English string.
pub fn t(fr: &'static str, en: &'static str) -> &'static str {
    if *FRENCH.get_or_init(server::french) {
        fr
    } else {
        en
    }
}

/// On the pairing page without a token, the page asks the app for one and puts it in its
/// URL (`#token=`); the web app submits it (code/apps/web/src/zenith/useEmbeddedPairing.ts)
/// and gets its session cookie. The web app reaches /pair by a client-side redirect, hence
/// the watch on history. One try per page load; on failure the page's form stays.
const PAIRING: &str = r##"(() => {
  if (location.protocol !== "http:" || window.top !== window) return;
  let asked = false;
  const onPairPage = () =>
    location.pathname === "/pair" && !new URLSearchParams(location.search).has("host");
  const pair = () => {
    const ipc = window.__TAURI_INTERNALS__;
    if (asked || !ipc || !onPairPage() || /(^#|&)token=/.test(location.hash)) return;
    asked = true;
    ipc.invoke("pairing_token").then((token) => {
      if (onPairPage()) location.replace(location.pathname + location.search + "#token=" + encodeURIComponent(token));
    }, () => {});
  };
  for (const name of ["pushState", "replaceState"]) {
    const original = history[name];
    history[name] = function (...args) {
      const result = original.apply(this, args);
      pair();
      return result;
    };
  }
  addEventListener("popstate", pair);
  pair();
})();"##;

/// The last zenith page shown, to come back to it after the server restarts.
static LAST_URL: Mutex<Option<String>> = Mutex::new(None);
/// The web app is on screen (not the waiting page).
static CONNECTED: AtomicBool = AtomicBool::new(false);

fn is_local(url: &Url) -> bool {
    matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "tauri.localhost"))
}

/// A page's URL without its fragment, where pairing tokens travel.
fn without_fragment(url: &Url) -> String {
    let mut url = url.clone();
    url.set_fragment(None);
    url.to_string()
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![shell_drag, shell_zoom, pairing_token])
        .menu(menu::build)
        .on_menu_event(menu::on_event)
        .setup(|app| {
            let window = open_window(app.handle())?;
            connect(window);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("zenith.app could not start")
        .run(|app, event| {
            // Clicking the Dock icon brings the window back.
            if let RunEvent::Reopen { .. } = event {
                if let Some(window) = app.get_webview_window(MAIN) {
                    menu::show(&window);
                }
            }
        });
}

fn open_window<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<WebviewWindow<R>> {
    let opener = app.clone();
    WebviewWindowBuilder::new(app, MAIN, WebviewUrl::App("index.html".into()))
        .title("zenith")
        .inner_size(1440.0, 920.0)
        .min_inner_size(480.0, 520.0)
        .center()
        // The page's own top bar is the title bar: 52 pt, the traffic lights centered in it.
        .title_bar_style(TitleBarStyle::Overlay)
        .hidden_title(true)
        .traffic_light_position(LogicalPosition::new(20.0, 26.0))
        // The window's material, seen through the page wherever it is transparent (the sidebar).
        .transparent(true)
        .effects(WindowEffectsConfig {
            effects: vec![Effect::Sidebar],
            state: Some(EffectState::FollowsWindowActiveState),
            radius: None,
            color: None,
            interactive: false,
        })
        .user_agent(USER_AGENT)
        .initialization_script(PAIRING)
        // zenith takes files dropped on it.
        .disable_drag_drop_handler()
        // zenith stays here, everything else opens in the browser.
        .on_navigation(move |url| {
            if is_local(url) || matches!(url.scheme(), "tauri" | "about" | "data" | "blob") {
                return true;
            }
            let _ = opener.opener().open_url(url.as_str(), None::<&str>);
            false
        })
        .on_new_window({
            let opener = app.clone();
            move |url, _| {
                let _ = opener.opener().open_url(url.as_str(), None::<&str>);
                NewWindowResponse::Deny
            }
        })
        .on_page_load(|window, payload| {
            #[cfg(debug_assertions)]
            eprintln!("[zenith] {:?} {}", payload.event(), without_fragment(payload.url()));
            if payload.event() == PageLoadEvent::Finished && payload.url().scheme() == "http" {
                // Never the pairing page: coming back to it would sign in again.
                if payload.url().path() != "/pair" {
                    *LAST_URL.lock().unwrap() = Some(without_fragment(payload.url()));
                }
                set_fullscreen_flag(&window);
            }
        })
        .build()
        .inspect(|window| {
            let w = window.clone();
            window.on_window_event(move |event| match event {
                // Closing the window keeps zenith in the Dock; the icon reopens it.
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = w.hide();
                }
                WindowEvent::Resized(_) => set_fullscreen_flag(&w),
                _ => {}
            });
        })
}

/// In full screen the traffic lights go away, and the page's room for them with them.
fn set_fullscreen_flag<R: Runtime>(window: &WebviewWindow<R>) {
    let js = if window.is_fullscreen().unwrap_or(false) {
        "document.documentElement.dataset.fullscreen='1'"
    } else {
        "delete document.documentElement.dataset.fullscreen"
    };
    let _ = window.eval(js);
}

/// Shows the web app once the server answers, and the waiting page while it doesn't —
/// at start, and whenever it restarts (an update, a crash).
fn connect<R: Runtime>(window: WebviewWindow<R>) {
    thread::spawn(move || {
        let mut attempts = 0u32;
        let mut misses = 0u32;
        loop {
            let up = server::healthy();
            if CONNECTED.load(Ordering::Relaxed) {
                misses = if up { 0 } else { misses + 1 };
                if misses >= 3 {
                    CONNECTED.store(false, Ordering::Relaxed);
                    attempts = 0;
                    if let Ok(url) = Url::parse("tauri://localhost/index.html") {
                        let _ = window.navigate(url);
                    }
                }
                thread::sleep(Duration::from_secs(5));
                continue;
            }
            if up {
                let target = LAST_URL.lock().unwrap().clone().unwrap_or_else(|| server::url("/"));
                if let Ok(url) = Url::parse(&target) {
                    let _ = window.navigate(url);
                }
                CONNECTED.store(true, Ordering::Relaxed);
                continue;
            }
            attempts += 1;
            if attempts == 1 {
                server::kickstart();
            }
            let message = if attempts > 40 {
                t(
                    "Le serveur ne répond pas. Journal : ~/Library/Logs/Zenith/server.log",
                    "The server isn't answering. Log: ~/Library/Logs/Zenith/server.log",
                )
            } else {
                t("Démarrage du serveur local", "Starting the local server")
            };
            let _ = window.eval(format!("var m=document.getElementById('message');if(m)m.textContent={message:?}"));
            thread::sleep(Duration::from_secs(1));
        }
    });
}

/// The page asks for a one-time pairing token (PAIRING); only the server's own pages may.
#[tauri::command]
async fn pairing_token<R: Runtime>(webview: tauri::Webview<R>) -> Result<String, String> {
    let page = webview.url().map_err(|e| e.to_string())?;
    let server = Url::parse(&server::base()).map_err(|e| e.to_string())?;
    if page.origin() != server.origin() {
        return Err("not zenith's server".into());
    }
    tauri::async_runtime::spawn_blocking(server::pairing_token).await.map_err(|e| e.to_string())?
}

/// A press on one of the page's title bars: the window follows the mouse.
#[tauri::command]
fn shell_drag<R: Runtime>(window: WebviewWindow<R>) {
    let _ = window.start_dragging();
}

/// A double-click there: what System Settings → Desktop & Dock says it does.
#[tauri::command]
fn shell_zoom<R: Runtime>(window: WebviewWindow<R>) {
    let action = std::process::Command::new("/usr/bin/defaults")
        .args(["read", "-g", "AppleActionOnDoubleClick"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    match action.as_str() {
        "Minimize" => {
            let _ = window.minimize();
        }
        "None" => {}
        _ => {
            if window.is_maximized().unwrap_or(false) {
                let _ = window.unmaximize();
            } else {
                let _ = window.maximize();
            }
        }
    }
}
