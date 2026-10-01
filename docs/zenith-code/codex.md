# zenith code in Rust: the Codex driver (WP-14)

Two crates under `crates/zenith-code/crates/`:

| Crate | What |
|---|---|
| `zc-codex-protocol` | serde types of the Codex app-server protocol, **generated** from its pinned JSON Schemas |
| `zc-provider-codex` | the driver: NDJSON JSON-RPC peer, session runtime, adapter (`ProviderAdapter`), status probe, home layout, managed mode |

The TypeScript they port: `packages/effect-codex-app-server` and, under `apps/server/src/provider/`,
`Layers/CodexAdapter.ts`, `Layers/CodexSessionRuntime.ts`, `Layers/CodexProvider.ts`,
`Layers/codexLaunchArgs.ts`, `Layers/codexUsageLimits.ts`, `CodexDeveloperInstructions.ts`,
`RuntimeInstructions.ts`, `codexModelOptions.ts`, `Drivers/CodexDriver.ts`,
`Drivers/CodexHomeLayout.ts`, `Drivers/CodexManagedProvider.ts`, `CodexManagedRuntime.ts`,
`CodexManagedErrors.ts`, `CodexManagedHome.ts`.

## `zc-codex-protocol`

```sh
cd code
node scripts/gen-rust-codex-protocol.ts                     # fetches openai/codex@fe74a77
node scripts/gen-rust-codex-protocol.ts --schema-dir <dir>  # or a local copy: <dir>/json, <dir>/typescript
```

**Which protocol.** The types follow `openai/codex@fe74a774532af67b5a4a3dec03ce9469e17f89af`, the
`UPSTREAM_REF` of `packages/effect-codex-app-server/scripts/generate.ts` (exported as
`methods::UPSTREAM_REF`). The CLI zenith code installs (0.156.1) and the one on the owner's Mac
(0.159.2) are newer; unknown values are tolerated (below), which is what makes the drift harmless.

**Input.** `schema/json/codex_app_server_protocol.schemas.json` (root definitions plus the v2 ones
under `definitions.v2`) holds every definition of the per-file schemas generate.ts downloads, once
each. Method lists come from the ts-rs `schema/typescript/{Client,Server}{Request,Notification}.ts`
with generate.ts's regexes (the authority, as in TS) and are cross-checked against the bundle's
method unions (the ts-rs files also list the legacy `getAuthStatus`/`getConversationSummary`/
`gitDiffToRemote` and the experimental `rawResponse*` notifications).

**Reproduced from generate.ts:** `PlanType` forced to `string`; the hand-added schemas
(`GetAuthStatus*`, `GetConversationSummary*`, `GitDiffToRemote*`, added only when missing);
`type: [X, "null"]` read as nullable X; `default: null` ignored; response types resolved by name
(`…Params` → `…Response`) with the same override table.

**What is generated.** Every server notification (84) and server request (10) with its response
type, the client requests zenith code sends (17, `USED_CLIENT_REQUESTS` in the generator),
`initialized`, `ToolRequestUserInputResponse`, the experimental `CollaborationMode` and
`AdditionalContextEntry` of `turn/start`, and their closure: 504 types. Regeneration is
deterministic (rustfmt with the repository's `rustfmt.toml`).

**Mapping.**

| JSON Schema | Rust |
|---|---|
| object with properties | struct; extra keys ignored on read and not written back (Effect's Struct decode does the same) |
| required key | `T` |
| required, nullable | `Option<T>` that must be present (`serde_helpers::nullable`) |
| optional key | `Option<T>` |
| optional, nullable | `Option<Option<T>>`: absent / `null` / value, written back as read |
| optional key of any JSON | `Option<Value>`, a present `null` kept |
| string enum | enum + `Unknown(String)` (tolerated), `as_str`, `from_wire` |
| objects sharing a single-literal key (`type`, `method`, `mode`, …) | enum dispatching on the key + `Unknown(Value)` |
| any other union | untagged enum + `Other(Value)` |
| `integer` | `i64` |
| `number` | `Number` (an `f64` written like JS: `5`, not `5.0`) |
| `{}` / `true` | `serde_json::Value` |

`ServerNotification::decode(method, params)` / `ServerRequest::decode` return `None` for an
unknown method and `Some(Err)` when the params do not match (TS drops such a notification);
`params_value()` gives the decoded params back as JSON, which is what the native events carry.
`ClientRequest` markers (`client_requests::TurnStart`, …) give `METHOD`, `HAS_PARAMS`, `Params`,
`Response`.

## `zc-provider-codex`

| Module | API |
|---|---|
| `peer` | `CodexPeer::start(reader, writer, handler, termination)`, `request`, `notify`, `respond`, `respond_error`, `wait_terminated`, `shutdown`; `IncomingHandler` (`on_notification`, `on_request`, `on_termination`) |
| `client` | `CodexRequester` (`request_raw`), `request::<M: ClientRequest>`, `decode_response` |
| `errors` | `CodexAppServerError` (the TS `_tag`s and messages), `RequestError`, `CodexSessionRuntimeError` |
| `process` | `SpawnSpec` (argv, cwd, env, `extend_env`), `spawn`, `ChildHandle` (`wait`, `kill`: group SIGTERM, SIGKILL after 2 s) |
| `session_runtime` | `CodexSessionRuntime::spawn(options)` implementing `CodexRuntime` (`start`, `send_turn`, `interrupt_turn`, `compact_thread`, `read_thread`, `rollback_thread`, `upload_feedback`, `respond_to_request`, `respond_to_user_input`, `take_events`, `close`); `CodexSessionRuntimeOptions::spawn_spec`; pure helpers (`build_turn_start_params`, `rewrite_skill_mentions`, `route_codex_child_notification`, `MemoryConsolidationFilter`, `classify_codex_stderr_line`, …) |
| `thread_history` | `open_codex_thread` (resume with fresh-start fallback), `read_codex_thread` (legacy / paginated), `rollback_codex_thread`, the runtime-mode table |
| `mapping` | `map_to_runtime_events(event, thread)` (native → canonical JSON), `CodexEventMapper` (turn token usage, rate-limit merge, usage-limit and managed rewording), `to_runtime_event` |
| `adapter` | `CodexAdapter::new(settings, CodexAdapterOptions)` implementing `zc_ports::adapter::ProviderAdapter`; `subscribe()`; `runtime_options(input, resolved)`; seams `RuntimeFactory`, `ResolveRuntime`, `NativeEventSink`, `McpSessionLookup` (+ `with_agent_device_environment`), `AttachmentResolver` (+ `AttachmentsDir`) |
| `provider_status` | `check_codex_provider_status` (→ snapshot draft JSON), `make_pending_codex_provider`, `probe_codex_app_server_provider`, `probe_codex_skills_for_cwd`, `consume_reset_credit`, `CodexAppServerClient`, model/skill/plan helpers, `draft_into_server_provider` |
| `home_layout` | `resolve_codex_home_layout`, `materialize_codex_shadow_home`, `codex_continuation_identity` |
| `managed` | `classify_codex_managed_error`, `managed_codex_launch_args`, `resolve_managed_codex_home_layout`, `ManagedCodexRuntime` (`resolve`), `managed_effective_runtime`; seams `ManagedExecutableSource`, `ChatGptAccessSource` |
| `driver` | `create_codex_instance`, `create_managed_codex_instance` → `CodexInstance` (`pending_snapshot`, `check_provider`, `skills_for_cwd`, `consume_reset_credit`, `account_key`, `adapter`, `continuation_identity`, `maintenance`); `merge_provider_instance_environment` |
| `launch_args`, `instructions`, `usage_limits`, `model`, `elicitation` | the small TS modules of the same names |

The product name in prompts and `clientInfo` is `zenith code` (`BRAND_NAME`), as the server build
writes it (`scripts/lib/zenith-brand.ts`).

## Gates

```sh
cargo test -p zc-codex-protocol -p zc-provider-codex      # all of the below except the recorded logs
cargo clippy -p zc-codex-protocol -p zc-provider-codex --all-targets
```

1. **Ported tests.** `CodexAdapter.test.ts` (fake runtime: `src/adapter/tests.rs`),
   `CodexSessionRuntime.test.ts` and `CodexCollabWire.test.ts` (`src/session_runtime/tests.rs`,
   `src/thread_history.rs`, `src/elicitation.rs`, `src/instructions.rs`),
   `CodexCollabRuntime.integration.test.ts` (`tests/collab_runtime.rs`: the real runtime against the
   repo's `testFixtures/codexCollabMockPeer.mjs`, which replays `codexMultiAgentWire.json`; needs
   `node`), `protocol.test.ts` (`src/peer.rs`), `codexUsageLimits`, `codexLaunchArgs`,
   `codexModelOptions`, `CodexProvider`, `CodexHomeLayout`, `CodexManagedErrors`,
   `CodexManagedRuntime` tests.
2. **Recorded threads.** `tests/recorded_fixtures.rs` feeds the `NTIVE:` frames of every Codex
   thread in the provider logs to the mapper and compares with the `CANON:` frames (transient
   types are not logged; summarized frames are skipped; `providerInstanceId` is stamped by the
   provider service). The logs are personal: use a copy.
   ```sh
   cp -R ~/.zenith/code/userdata/logs/provider /tmp/provider-logs
   ZC_CODEX_RECORDED_LOGS=/tmp/provider-logs cargo test -p zc-provider-codex --test recorded_fixtures -- --nocapture
   ```
3. **Launch argv/env.** `tests/launch_oracle.rs` compares the Rust spawn (session and probe) with
   `tests/fixtures/launch_oracle.json`, produced by the real TS code. Regenerate (needs
   `node_modules`; a worktree can symlink the main checkout's, see verification.md):
   ```sh
   cd code && node apps/server/scripts/codex-launch-oracle.ts > ../crates/zenith-code/crates/zc-provider-codex/tests/fixtures/launch_oracle.json
   ```

`tests/live_smoke.rs` (ignored) runs the probe and one trivial read-only turn against a real
`codex`, with a COPY of a Codex home (`ZC_CODEX_LIVE_BINARY`, `ZC_CODEX_LIVE_HOME`).

## Deviations from the TS

- **Leniency.** Unknown enum values and union members decode (`Unknown`/`Other`), where the
  Effect schemas reject them and the client drops the notification. The adapter still maps an
  item of an unknown type the way TS does (no lifecycle event). Responses the runtime only reads a
  few fields of (`thread/start`, `thread/resume`, `thread/read`, `thread/turns/list`) are decoded
  to those fields only. Never stricter.
- **Event fan-out.** `subscribe_events` is a broadcast (every subscriber gets every event, from
  when it subscribed), not a single-consumer queue.
- **Closing.** `close()` lets the adapter's consumer drain what the runtime emitted while closing
  (the `session/closed` → `session.exited` event); TS shuts its queue down and may drop it.
- **Ordering of request events.** A server request's native event is emitted from its handler
  task, notifications from the notification task; as in TS the two can interleave either way.
- **No identifier-generation errors** (`CodexAppServerIdentifierGenerationError`): UUIDs cannot
  fail here. **No encode errors**: params are built as typed values.
- **`localeCompare`** (sorting an apply-patch approval's paths) is approximated (punctuation <
  digits < letters, case-insensitive, then lowercase first).
- **Managed status check.** The ChatGPT model filter (`CodexChatGptModels.ts`, an HTTP call) is not
  applied; the sign-in and installation are seams (WP-12b).

## What the provider core (WP-12) wires

- Per instance: `create_codex_instance(input, services)` (or `create_managed_codex_instance` with
  the WP-12b seams) and keep `CodexInstance`. Register `instance.adapter` in the adapter registry,
  subscribe to `adapter.subscribe_events()` **before** starting sessions, and log each canonical
  event (`CANON:`) with `providerInstanceId` stamped.
- `CodexAdapterOptions`: `native_event_sink` (the `NTIVE:` logger), `mcp_sessions` (WP-27a's
  registry), `attachments` (`AttachmentsDir(<stateDir>/attachments)` or the attachment store),
  `models` (the snapshot's model list, for display names), `default_cwd`.
- Snapshot management (`makeManagedServerProvider`): `pending_snapshot(checked_at)` as the initial
  snapshot, `check_provider(cwd, checked_at)` on refresh (then the model manifest, the version
  advisory and `withInstanceIdentity` stamping: `draft_into_server_provider` only adds
  `instanceId`/`driver`), `skills_for_cwd(cwd)` for `snapshotForCwd`.
- Maintenance: `instance.maintenance` (`@openai/codex`, `codex update` with `CODEX_HOME` = shared
  home for standalone installs).
- Reset credits: serialize on `instance.account_key()`, call `consume_reset_credit`, re-probe.
- Text generation (`codex exec`) is WP-17: `launch_args::codex_exec_launch_args` is ready.
