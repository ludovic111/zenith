//! What agents may do through zenith-mcp (`settings.agent.permissions` in the lsuite
//! standard), in `~/.zenith/app/agent-permissions.json`, checked by the registry for every
//! request: `off` (nothing), `read` (commands that change nothing), `full` (everything).
//! Only the window and the CLI can change it.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Off,
    Read,
    #[default]
    Full,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Permissions {
    pub mcp: Access,
}

fn path() -> std::path::PathBuf {
    zenith_client::local::app_home().join("agent-permissions.json")
}

impl Permissions {
    pub fn load() -> Self {
        std::fs::read(path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)
    }
}
