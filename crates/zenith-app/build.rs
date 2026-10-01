fn main() {
    // install.sh bakes these in (option_env! in src/server.rs): rebuild when they change.
    for var in [
        "ZENITH_SERVER",
        "ZENITH_NODE",
        "ZENITH_CODE_HOME",
        "ZENITH_URL",
        "ZENITH_AGENT_LABEL",
        "ZENITH_LANG",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    // The page asks for these (its title bars, its sign-in); a capability lets it (capabilities/main.json).
    let manifest = tauri_build::AppManifest::new().commands(&["shell_drag", "shell_zoom", "pairing_token"]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest)).expect("tauri build");
}
