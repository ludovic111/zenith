//! Runs the settings, keybindings and theme services against a base directory and prints one
//! JSON line per operation, the same lines the TS oracle (`harness.ts` in the WP-07 notes,
//! docs/zenith-code/settings.md) prints, so the two backends can be diffed on a copy of real
//! user data.
//!
//! ```sh
//! cargo run -p zc-settings --example settings_oracle -- <baseDir> <ops.json>
//! ```
//!
//! `ops.json` is an array of `{"op": …}`: `start`, `file` (`name`), `getSettings` (redacted),
//! `getSettingsRaw`, `updateSettings` (`patch`), `keybindings`, `upsertKeybinding` /
//! `removeKeybinding` (`input`), `themes`. Never point it at the live `~/.zenith/code`.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use zc_settings::js::stringify;
use zc_settings::keybindings::KeybindingsService;
use zc_settings::settings::{redact_server_settings_for_client, ServerSettingsService};
use zc_settings::themes::EnvironmentThemeService;

fn settings_error(error: &zc_contracts::ServerSettingsError) -> Value {
    let mut value = serde_json::to_value(error).unwrap();
    value.as_object_mut().unwrap().remove("cause");
    value
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let base_dir = PathBuf::from(args.next().expect("base dir"));
    let ops: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(args.next().expect("ops file")).unwrap()).unwrap();
    let paths = zc_core::derive_server_paths(&base_dir, None, true);
    let db = zc_db::Db::open(&paths.db_path).expect("open the database");
    let secrets = zc_core::ServerSecretStore::open(&paths.secrets_dir).await.expect("open the secret store");
    let settings = ServerSettingsService::new(&paths.settings_path, Arc::new(secrets), Arc::new(db));
    let keybindings = KeybindingsService::new(&paths.keybindings_config_path);
    let themes = EnvironmentThemeService::start(&paths.environment_themes_dir).await;
    for op in ops {
        let name = op["op"].as_str().unwrap_or_default().to_owned();
        let result: Result<Value, Value> = match name.as_str() {
            "start" => {
                async {
                    settings.start().await.map_err(|e| settings_error(&e))?;
                    keybindings.start().await.map_err(|e| serde_json::to_value(e).unwrap())?;
                    Ok(Value::Null)
                }
                .await
            }
            "file" => Ok(std::fs::read_to_string(paths.state_dir.join(op["name"].as_str().unwrap()))
                .map(Value::String)
                .unwrap_or(Value::Null)),
            "getSettings" => settings
                .get_settings_value()
                .await
                .map(redact_server_settings_for_client)
                .map_err(|e| settings_error(&e)),
            "getSettingsRaw" => settings.get_settings_value().await.map_err(|e| settings_error(&e)),
            "updateSettings" => settings
                .update_settings_value(&op["patch"])
                .await
                .map(redact_server_settings_for_client)
                .map_err(|e| settings_error(&e)),
            "keybindings" => keybindings
                .load_config_state()
                .await
                .map(|state| zc_settings::config::keybindings_payload(&state))
                .map_err(|e| serde_json::to_value(e).unwrap()),
            "upsertKeybinding" => keybindings
                .upsert_keybinding_rule(&serde_json::from_value(op["input"].clone()).unwrap())
                .await
                .map(|rules| serde_json::to_value(rules).unwrap())
                .map_err(|e| serde_json::to_value(e).unwrap()),
            "removeKeybinding" => keybindings
                .remove_keybinding_rule(&serde_json::from_value(op["input"].clone()).unwrap())
                .await
                .map(|rules| serde_json::to_value(rules).unwrap())
                .map_err(|e| serde_json::to_value(e).unwrap()),
            "themes" => Ok(Value::Array(themes.current().await)),
            // A failed decode throws in the TS oracle: a defect.
            "decode" => zc_settings::settings::schema::decode_settings(&op["input"]).map_err(|_| json!("defect")),
            "decodePatch" => zc_settings::settings::schema::decode_patch(&op["input"]).map_err(|_| json!("defect")),
            other => panic!("unknown op {other}"),
        };
        let line = match result {
            Ok(value) => json!({"op": name, "ok": value}),
            Err(mut error) => {
                if let Some(map) = error.as_object_mut() {
                    map.remove("cause");
                }
                json!({"op": name, "error": error})
            }
        };
        println!("{}", stringify(&line));
    }
}
