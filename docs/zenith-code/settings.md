# zenith code in Rust: settings, keybindings and themes (`zc-settings`)

`crates/zenith-code/crates/zc-settings` is WP-07 of [the porting plan](../zenith-code-rust-plan.md)
(§6.16). It owns the files the TS server keeps in `<baseDir>/userdata/`, shared with the TS server
and read by the dashboard, so they are read and written **byte for byte like TS does**:

| File | Owner | Ported from |
|---|---|---|
| `settings.json` | `settings::ServerSettingsService` | `apps/server/src/serverSettings.ts`, shared `serverSettings.ts`, `backgroundActivitySettings.ts` |
| `secrets/provider-env-*.bin`, `usage-limit-source-*.bin`, `bitbucket-*.bin` | the same | (through `zc_core::ServerSecretStore`) |
| `keybindings.json` | `keybindings::KeybindingsService` | `apps/server/src/keybindings.ts`, shared `keybindings.ts` |
| `themes/*.json` | `themes::EnvironmentThemeService` | `apps/server/src/environmentTheme.ts` |

It also serves `server.getSettings`, `server.updateSettings`, `server.upsertKeybinding`,
`server.removeKeybinding`, `server.getConfig` and `subscribeServerConfig` (`rpc::SettingsRpc`),
and implements the `zc_ports::SettingsService` port.

## Wiring

```rust
let paths = zc_core::derive_server_paths(&base_dir, dev_url, explicit);
let settings = ServerSettingsService::new(&paths.settings_path, Arc::new(secret_store), Arc::new(db));
let keybindings = KeybindingsService::new(&paths.keybindings_config_path);
let themes = EnvironmentThemeService::start(&paths.environment_themes_dir).await;
settings.start().await?;      // directory, watcher, first load (folds, migrations)
keybindings.start().await?;   // directory, watcher, default back-fill, first load

let config = ServerConfigService::new(settings.clone(), keybindings, ServerConfigParts {
        cwd: config.cwd,
        observability: zc_settings::config::observability(&logs_dir, traces, metrics, logs),
    })
    .with_themes(themes)
    .with_contributor(environment_and_auth)      // environment, auth (WP-33, WP-06)
    .with_contributor(providers_and_editors)     // providers, availableEditors, remoteOpenTargets, …
    .with_event_source(provider_statuses)        // providerStatuses (WP-12)
    .with_event_source(usage_limit_sources);     // usageLimitSourcesUpdated (WP-30)
let router = SettingsRpc::new(config).register(router_builder);
let port: Arc<dyn zc_ports::SettingsService> = Arc::new(settings);
```

- **`SnapshotContributor`** fills fields of the `ServerConfig` snapshot owned elsewhere; the
  builder starts from the settings/keybindings/observability parts and puts the keys in
  `ServerConfig` declaration order at the end. `environment` and `auth` are `null` until a
  contributor sets them (the client needs both).
- **`ConfigEventSource`** adds encoded `ServerConfigStreamEvent`s to a subscription, chosen by the
  payload flags (`ConfigOptions`). Sources subscribe before the snapshot is read and can adjust
  their stream once it is known (`with_snapshot`: provider statuses drop a first value equal to
  the snapshot's, then debounce, in WP-12).
- The stream never completes (plan §1.3 rule 9).
- `SettingsRpc::with_patch_hook` lets the device package rewrite `deviceHosts` in a patch, as
  `ws.ts` does with `remoteSshDeviceHosts`.

`get_settings` (port and `get_settings_value`) returns **materialized** secrets: providers,
terminals and usage need the real values. RPCs redact (`redact_server_settings_for_client`), like
`ws.ts`.

## How the TS semantics are kept

The TS service works on plain objects, so its files depend on JS behaviour. The Rust service works
on encoded JSON (`serde_json::Value`, insertion-ordered) and reproduces it:

- **Decoding** (`settings::schema`): the generated `zc_contracts::ServerSettings` gives structure,
  top-level decoding defaults and declaration order. On top of it: the string transformations
  (`TrimmedString`, `TrimmedNonEmptyString`, the binary-path fallback `"" → "codex"`), the
  nested decoding defaults the generated types leave out (e.g. `providers.codex` when only
  `providers.cursor` is in the file), the legacy `ModelSelection` shapes, the
  `ForwardCompatibleNullable` members, and record key order (the generated `BTreeMap`s sort).
  The two tables are extracted from the Effect AST by `code/scripts/settings-schema-tables.ts`;
  rerun it after a contracts change and update `schema.rs`.
- **Writing**: `stripDefaultServerSettings` against the defaults (optional providers' `enabled`
  always kept), then `JSON.stringify(…, null, 2)` with JS number formatting (`js` module), plus
  a newline, through the atomic write.
- **Equality** is Effect's structural `Equal.equals` (key order ignored, numbers by value).
- **Caches** keep a failed load until the next invalidation, like Effect `Cache`.

## The gate: TS and Rust on the same data

`code/scripts/settings-oracle.ts` (TS services from source) and the
`settings_oracle` example (Rust) run the same operations on a base directory and print one JSON
line each. Run both on two copies of the same user data, then compare the lines and the files
left on disk:

```sh
cp -Rp ~/.zenith/code/userdata /tmp/gate/ts/userdata    # copies only: never the live directory
cp -Rp ~/.zenith/code/userdata /tmp/gate/rs/userdata
(cd code && node scripts/settings-oracle.ts /tmp/gate/ts ops.json | grep '^{"op"' > /tmp/gate/ts.out)
cargo run -q -p zc-settings --example settings_oracle -- /tmp/gate/rs ops.json > /tmp/gate/rs.out
cmp /tmp/gate/ts.out /tmp/gate/rs.out
for f in settings.json keybindings.json; do cmp /tmp/gate/{ts,rs}/userdata/$f; done
diff -r /tmp/gate/ts/userdata/secrets /tmp/gate/rs/userdata/secrets
```

Operations: `start`, `file` (`name`), `getSettings`, `getSettingsRaw`, `updateSettings`
(`patch`), `keybindings`, `upsertKeybinding` / `removeKeybinding` (`input`), `themes`, `decode` /
`decodePatch` (`input`). Paths in error lines differ by the run directory; nothing else should.

Results on a copy of the owner's `userdata` (2026-10-01), all identical, output lines and files
byte for byte, secrets directory included:

| Run | Operations |
|---|---|
| no-op | start, load, `getSettings`, keybindings, themes, `updateSettings({})` (which rewrites the file: TS drops a stored default) |
| patches (50 ops) | trims and binary-path fallbacks, model selection replace/merge, provider instances with sensitive env vars and the marker round-trip, usage hubs add/remove, Bitbucket tokens set/clear, project overrides (canonical and legacy maps), background activity profiles and intervals, worktree cleanup, prices, nullable fields, keybinding upsert/replace/remove |
| secrets | secrets left in place at the end (env vars, hub key, Bitbucket) |
| decode (41 inputs) | `ServerSettings` / `ServerSettingsPatch` edge cases: legacy shapes, trimming, invalid values, unknown keys, integer-like record keys, number formats |
| legacy file | comments and trailing commas, in-config `enabled` flags, legacy project maps folded with the real `projection_projects` rows, an inline Bitbucket token moved to the store, conflicting keybinding defaults skipped |
| invalid / broken | a file that does not decode (defaults, file untouched), broken JSON; keybindings with invalid entries (issue texts) and a malformed file |
| themes | trimmed names, extra keys, unsorted palettes, a rejected role |

## Deviations

- **Refinements**: like `zc-contracts`, most schema checks beyond the ones listed above are not
  enforced (integer ranges, slug patterns, URL shapes): a file TS would reject for one of them is
  accepted. Every check that changes a value (trims, fallbacks) or that a test covers is
  enforced. A patch failing one of the enforced checks is a per-request `Die`, as in TS.
- **`null` for a defaulted key** decodes to the default (TS rejects the whole file).
- **Issue texts**: keybinding issues reproduce the `Cause.pretty` texts of the cases TS produces
  for this schema (missing key, wrong type, lengths, unknown command, invalid rule, malformed
  file); other Effect error shapes are not reproduced.
- **Generated wire types sort records**: the typed port (`zc_ports::SettingsService`) and
  `EnvironmentTheme` values converted to `zc_contracts` types have record keys in sorted order;
  the RPC handlers send the encoded JSON with TS order. Key order on the wire is cosmetic.
- **Watching** uses `notify` (FSEvents on macOS). A change is applied 100 ms after the last event,
  like `Stream.debounce`; startup's own writes can produce one extra (identical) change event.
- **Subscriptions are eager**: `subscribeServerConfig` subscribes to settings and keybinding
  changes before reading the snapshot (TS subscribes after), so a change in between is delivered
  instead of lost.
