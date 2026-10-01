# zenith

zenith is a Mac app for coding with agents. The interface is zenith code's web app (`code/apps/web`, React; a T3 Code fork, see `code/ZENITH.md`). Its server is being rewritten in Rust in `crates/zenith-code` (plan: `docs/zenith-code-rust-plan.md`; until it is done, `code/apps/server` runs). `crates/zenith-app` is the Tauri window. Rust code: `cargo clippy --workspace`, `cargo test --workspace`, rustfmt (root `rustfmt.toml`). Tests use made-up names; `npm run privacy` must pass.
