fn main() {
    // scripts/mac/install.sh bakes these in (option_env! in src/local.rs): rebuild when they change.
    for var in ["ZENITH_URL", "ZENITH_CODE_HOME", "ZENITH_AGENT_LABEL"] {
        println!("cargo:rerun-if-env-changed={var}");
    }
}
