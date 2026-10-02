//! The window's own preferences (`~/.zenith/app/window.json`): appearance, sidebar, the last
//! thread open, and what agents connecting through zenith-mcp may do. Server settings live
//! on the server (`server.updateSettings`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::theme::Appearance;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Prefs {
    pub appearance: Appearance,
    pub sidebar_visible: bool,
    /// The web's sidebar width (`chat_thread_sidebar_width`, 256 by default); a new key, so
    /// the width older versions saved (280) does not carry over.
    #[serde(rename = "threadSidebarWidth")]
    pub sidebar_width: f32,
    pub last_thread: Option<String>,
    /// Check GitHub Releases for a new zenith when the app starts.
    pub check_for_updates: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            appearance: Appearance::System,
            sidebar_visible: true,
            sidebar_width: 256.,
            last_thread: None,
            check_for_updates: true,
        }
    }
}

fn path() -> PathBuf {
    zenith_client::local::app_home().join("window.json")
}

impl Prefs {
    pub fn load() -> Self {
        std::fs::read(path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let path = path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(tmp, path);
            }
        }
    }
}
