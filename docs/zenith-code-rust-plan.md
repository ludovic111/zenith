# zenith code in Rust: the porting plan

This is the plan for rewriting **the whole zenith code server** (`code/apps/server`, about 181k lines of
TypeScript on Effect, excluding tests) in Rust, as the crate `crates/zenith-code` in the repository's Cargo
workspace. The existing web frontend (`code/apps/web`) must keep working **unchanged** against it, and so must
the CLI that the dashboard calls.

The plan is written so that 10 to 20 engineers can work in parallel without re-deriving the architecture.
Every claim points at the TypeScript source it comes from. Paths are relative to `code/` unless they start
with `src/` or `crates/` (repository root).

Status: research only. Nothing has been ported yet. Base: upstream T3 Code `451afcb2` (see `code/UPSTREAM`),
`effect@4.0.0-rc.115` (patched: `code/patches/effect@4.0.0-rc.115.patch`), server package version `0.0.43`.

---

## 0. What you need to know first

1. **The server is a WebSocket RPC server plus a small HTTP API.** The web app opens one WebSocket, `GET /ws`,
   and speaks **Effect RPC with JSON serialization**: one JSON object per text frame, tagged envelopes
   (`Request`, `Chunk`, `Ack`, `Exit`, `Ping`/`Pong`, `Interrupt`, `Defect`). It calls 148 RPC methods; 25 of
   them are streams, and every "push" is a long-lived stream. The full method table, with schema names and
   the required auth scope, is in [Appendix A](#appendix-a-all-148-ws-rpc-methods). The HTTP side has 24
   typed endpoints plus a handful of raw routes (§1.2).
2. **Wire fidelity is the main risk.** Payloads are Effect Schema *encoded* values: absent vs `null` keys,
   `{"_tag":"Some","value":…}` options, `"NaN"` strings, ISO dates with milliseconds, tagged errors inside an
   `Exit` cause. The client decodes everything strictly against `packages/contracts`, and a decode failure
   shows up as silently missing data, not a crash. So **the contracts are generated, not hand-written**
   (§7.2, WP-01), and **the TypeScript contracts package is kept as the oracle** for conformance tests (§9).
3. **State must stay compatible both ways.** The Rust server opens the existing `~/.zenith/code/userdata/state.sqlite`.
   That is SQLite in WAL mode, 54 migrations tracked in `effect_sql_migrations`, and an event store whose
   `sequence` numbers the clients cache in IndexedDB. It also reuses the existing secret files (a 30-day cookie
   must keep verifying) and the existing `settings.json` / `keybindings.json`. The TS server must be able to
   reopen the database afterwards, so rollback stays possible.
4. **Almost everything external is a subprocess.**
   - **git** is 100% `git` CLI. **Forges** go through `gh`, `glab`, `az`, `fj`/`tea`; Bitbucket is the only
     one reached by direct REST.
   - **Claude** runs as the `claude` CLI in stream-json mode with its control protocol. The server uses the
     Agent SDK, which just spawns the CLI, so Rust speaks that protocol directly.
   - **Codex** runs as `codex app-server` (NDJSON JSON-RPC without the `jsonrpc` field).
   - **Cursor, Grok and Antigravity** are ACP agents over stdio. **OpenCode** is `opencode serve` over HTTP+SSE.
   - Port scanning shells out to `lsof`, `ps` and `tailscale`.

   Keep shelling out: it gives behavioural parity for free, and the TS test fixtures stay valid.
5. **Some of it is already Rust:**
   - `code/native/resource-monitor` (crate `t3-resource-monitor`, sysinfo, 1,334 lines) is the process-sampling
     sidecar. Link it as a library.
   - `@ff-labs/fff-node` (workspace file search) is FFI over the Rust library **fff**. Depend on the fff crate
     directly.
   - The ACP schema is generated from Rust (crate `agent-client-protocol`).
   - Codex's app-server protocol is a Rust crate (`codex-app-server-protocol`).
6. **Part of the server is dead weight in zenith and can be stubbed:**
   - T3 Connect/cloud/relay is inert: it has no build keys.
   - The desktop update channel, the service launcher and PostHog (off by default) are unused.
   - SSH remote environments were Electron-only.
   - Device support (iOS/Android simulators) has low priority.

   The UI still calls a few of their RPCs, so each needs a minimal, correctly-shaped answer (§6.14).
7. **Two dashboards spawn the server today.** One is the TS dashboard: `src/lib/code/manager.ts`, `cli.ts`,
   `api.ts`. The other is the Rust rewrite of the dashboard happening in parallel in this worktree:
   `crates/zenith-server/src/code/{manager,cli,api,paths}.rs`. Both run `node code/apps/server/dist/bin.mjs …`.
   We keep `bin.mjs` as a 20-line Node shim that execs the Rust binary (§10). Later, both dashboards point
   `CODE_BIN` at the binary, and `zenith-server` can eventually link `zenith-code` in-process.

### 0.1 Size inventory (non-test lines / test lines)

| Area | Source | Tests | Notes |
|---|---:|---:|---|
| `apps/server/src/provider` | 51.9k | 64.9k | 6 drivers; Claude adapter alone 5.6k |
| `apps/server/src/orchestration` | 20.4k | 36.2k | event sourcing, projections, reactors |
| `apps/server/src/pullRequest` | 19.8k | 22.7k | 5 forges, GitHub ≈ 7k |
| `apps/server/src/sourceControl` | 8.0k | 6.7k | gh/glab/az/fj/tea/Bitbucket |
| `apps/server/src/persistence` | 7.6k | 4.2k | 54 migrations (2.0k) |
| `apps/server/src/vcs` | 6.5k | 6.4k | git driver, checkpoints, status broadcaster |
| `apps/server/src/cloud` | 5.8k | 4.8k | T3 Connect: stub |
| `apps/server/src/cli` | 5.7k | 3.2k | `bin.ts` commands |
| `apps/server/src/auth` | 4.3k | 2.7k | sessions, pairing, DPoP |
| `apps/server/src/device` | 4.2k | 2.3k | simulators: low priority |
| `apps/server/src/usage` | 4.0k | 3.7k | transcript scanning, pricing |
| `apps/server/src/project` | 4.0k | 6.5k | favicon, clone, worktree setup, agent-session import |
| `apps/server/src/resourceTelemetry` | 3.6k | 2.0k | sidecar client |
| `apps/server/src/terminal` | 3.6k | 3.2k | node-pty |
| `apps/server/src/git` | 3.4k | 6.4k | stacked actions |
| `apps/server/src/textGeneration` | 3.1k | 3.7k | one-shot agent CLI calls |
| `apps/server/src/mcp` | 3.1k | 3.5k | MCP HTTP server for agents |
| `apps/server/src/{assets,workspace,preview,process,diagnostics,environment,background,checkpointing,observability,telemetry,…}` | ≈ 10k | ≈ 10k | |
| root files (`ws.ts` 3.9k, `server.ts`, `http.ts`, `serverSettings.ts` 1.1k, `serverRuntimeStartup.ts` 1.1k, `keybindings.ts`, …) | ≈ 12k | ≈ 20k (`server.test.ts` 13.2k) | **`ws.ts` contains real logic**, not just glue |
| `packages/contracts` | 20.9k | 6.3k | the wire schemas |
| `packages/shared` (server-used parts) | ≈ 6k of 16.9k | | `hostProcess`, `model`, `shell`, `git`, `keybindings` `when` parser, `sourceControl` detection, `dpop`, `projectScripts` … |
| `packages/effect-acp` | 2.1k + 10.4k generated | | ACP client |
| `packages/effect-codex-app-server` | 1.0k + 57.8k generated | | Codex app-server client |
| `packages/ssh`, `packages/tailscale` | 2.7k, 0.4k | | only `ssh/command` and the tailscale CLI wrapper are used |

Expect roughly 110–140k lines of Rust. The 58k generated Codex lines collapse to a few thousand, since only about 30 methods are used.

---

## 1. Wire protocol

### 1.1 Transport

- One HTTP listener (`server.ts:224-243`, Node `http`). It binds `--host` (default `127.0.0.1`) and `--port`.
- **WebSocket** `GET /ws`, with `permessage-deflate` negotiated (context takeover on, no threshold). Text frames only.
- **Global middlewares** (`server.ts:598-628`, `http.ts`):
  - gzip/deflate/br compression (`HttpMiddleware.compression`);
  - CORS (§2.6);
  - **command readiness**: every HTTP request, including the WS upgrade, waits on `ServerRuntimeStartup.awaitCommandReady` until startup finishes (§6.18). If startup fails, they fail.
- HTML responses from the static handler carry `Content-Security-Policy: frame-ancestors 'self' <ZENITH_CODE_PARENT_ORIGINS…>` (§2.7).

### 1.2 HTTP routes (complete)

**Typed endpoints.** These use Effect `HttpApi`, defined in `packages/contracts/src/environmentHttp.ts:411-623`. Handlers are in `apps/server/src/auth/http.ts`, `cloud/http.ts`, `orchestration/http.ts`, `pullRequest/http.ts`, and `http.ts` (`serverEnvironmentHttpApiLayer`). Bodies are JSON, encoded with the same `toCodecJson` as RPC (§1.5).

Errors are JSON tagged errors with the status from the `httpApiStatus` annotation:

```http
HTTP/1.1 401
{"_tag":"EnvironmentAuthInvalidError","code":"auth_invalid","reason":"missing_credential","traceId":"…"}
```

`EnvironmentAuthInvalidError` may carry `dpopFailureReason`. The other errors are `EnvironmentRequestInvalidError` (400, `code:"invalid_request"`, reason `invalid_scope|scope_not_granted|invalid_command`), `EnvironmentScopeRequiredError` (403, `insufficient_scope`, `requiredScope`), `EnvironmentOperationForbiddenError` (403), `EnvironmentResourceNotFoundError` (404) and `EnvironmentInternalError` (500, reason from a fixed enum, `environmentHttp.ts:86-101`). The cloud group adds `EnvironmentHttp{BadRequest,Unauthorized,Forbidden,InternalServer,Conflict}Error` and `EnvironmentCloudEndpointUnavailableError` (503).

| Method | Path | Auth | Request | Success schema | Used by web? |
|---|---|---|---|---|---|
| GET | `/.well-known/t3/environment` | none | — | `ExecutionEnvironmentDescriptor` (environment.ts) | yes, on **every** connect; `orchestrationProtocolVersion` must be 1 |
| GET | `/api/auth/session` | optional | headers `authorization?`, `dpop?` | `AuthSessionState` (auth.ts). **Always 200** | yes |
| POST | `/api/auth/browser-session` | none | `AuthBrowserSessionRequest` `{credential}` | `AuthBrowserSessionResult` + `Set-Cookie` | yes (pairing) |
| POST | `/oauth/token` | none (optional `dpop`) | **form-urlencoded** `AuthTokenExchangeRequest` (token-exchange grant) | `AuthAccessTokenResult` `{access_token, issued_token_type, token_type, expires_in, scope}` | remote clients |
| POST | `/api/auth/websocket-ticket` | session | — | `AuthWebSocketTicketResult` `{ticket, expiresAt}` | bearer/relay clients |
| POST | `/api/auth/pairing-token` | `access:write` | `AuthCreatePairingCredentialInput` `{label?, scopes?}` | `AuthPairingCredentialResult` | Settings › Connections |
| GET | `/api/auth/pairing-links` | `access:read` | — | `AuthPairingLink[]` | no (stream used instead) |
| POST | `/api/auth/pairing-links/revoke` | `access:write` | `{id}` | `{revoked}` | yes |
| GET | `/api/auth/clients` | `access:read` | — | `AuthClientSession[]` | no |
| POST | `/api/auth/clients/revoke` | `access:write` | `{sessionId}` | `{revoked}`; 403 on own session | yes |
| POST | `/api/auth/clients/revoke-others` | `access:write` | — | `{revokedCount}` | yes |
| GET | `/api/orchestration/snapshot` | `orchestration:read` | — | `OrchestrationReadModel` (command model, lightweight) | CLI `project …` |
| GET | `/api/orchestration/shell` | `orchestration:read` | — | `OrchestrationShellSnapshot` | yes; also the dashboard (`src/lib/code/api.ts`) |
| GET | `/api/orchestration/threads/:threadId` | `orchestration:read` | query `reasoningMessages?="true"`, `turnLimit?` (int ≥1, string-encoded), `beforeCursor?` | `OrchestrationThreadDetailSnapshot` | yes; dashboard |
| POST | `/api/orchestration/dispatch` | `orchestration:operate` | `ClientOrchestrationCommand` | `DispatchResult` `{sequence}` | dashboard, CLI |
| POST | `/api/pull-requests/diff` | `orchestration:read` | `PullRequestDiffInput` | `PullRequestDiffResult` | yes (large diffs kept off the socket) |
| POST | `/api/connect/link-proof` | `relay:write` | `RelayLinkProofRequest` | `RelayEnvironmentLinkProof` | cloud (stub) |
| POST | `/api/connect/relay-config` | `relay:write` | `RelayEnvironmentConfigRequest` | `EnvironmentCloudRelayConfigResult` | cloud (stub) |
| GET | `/api/connect/link-state` | `relay:read` | — | `EnvironmentCloudLinkStateResult` | **may be probed**: answer "not linked" |
| POST | `/api/connect/unlink` | `relay:write` | — | `EnvironmentCloudRelayConfigResult` | cloud (stub) |
| POST | `/api/connect/preferences` | `relay:write` | `{publishAgentActivity}` | `EnvironmentCloudLinkStateResult` | cloud (stub) |
| POST | `/api/t3-connect/health` | relay-signed JWT | `RelayCloudEnvironmentHealthRequest` | `RelayEnvironmentHealthResponse` | cloud (404 is fine) |
| POST | `/api/connect/mint-credential`, `/api/t3-connect/mint-credential` | relay-signed JWT | `RelayCloudMintCredentialRequest` | `RelayEnvironmentMintResponse` | cloud (404 is fine) |

**Raw routes** (`HttpRouter.add`):

| Method | Path | Source | Behaviour |
|---|---|---|---|
| GET | `/ws` | `ws.ts:3803-3918` | WebSocket upgrade. Auth: `?wsTicket=` if present, otherwise cookie/bearer (§2.4). Records the client connection (`markConnected`/`markDisconnected`). |
| POST | `/api/observability/v1/traces` | `http.ts:321` | Browser OTLP JSON spans. Auth `orchestration:operate`. Forwards to the configured OTLP URL and the local trace collector. Answers 204. |
| GET, HEAD | `/api/assets/<token>/<name>` | `http.ts:390`, `assets/AssetAccess.ts` | HMAC-signed capability URLs (§6.11). Range requests for audio/video. Per-type CSP (`sandbox` for HTML and SVG). |
| POST | `/api/attachments/upload/<token>` | `http.ts:437`, `assets/AttachmentUpload.ts` | Signed upload; `Content-Length` must equal the size in the token. Answers 204. |
| * (+WS) | `/api/device-hub/*` | `device/DeviceHubProxy.ts:233` | Proxy to the local device hub (§6.13). |
| GET | `/zenith/embed.json` | `zenith/embed.ts` | Public. `{"parentOrigins":[…]}`, `cache-control: no-store`. **Also the dashboard's liveness probe.** |
| POST, DELETE (others 405) | `/mcp` | `mcp/McpHttpServer.ts:663` | MCP Streamable HTTP for the agents (§6.9). Own bearer tokens. |
| GET | `/callback` | `cloud/CliTokenManager.ts:464` | Only during a CLI T3 Connect login: skip. |
| GET | `*` | `http.ts:563-669` | Static SPA from `apps/server/dist/client` (or `apps/web/dist`). Rules: `/` maps to `index.html`; an extensionless path resolves to `<dir>/index.html`; anything missing falls back to `index.html`; traversal is rejected. Immutable cache for hashed `assets/*-xxxxxxxx.*` files listed in `.vite/manifest.json`, `no-cache` otherwise. Weak ETag and Last-Modified on non-HTML; HTML never gets 304. HTML gets the frame-ancestors CSP. |

### 1.3 RPC envelope (Effect RPC, `RpcSerialization.layerJson`)

Sources:
- `effect/dist/unstable/rpc/RpcMessage.js`, `RpcServer.js`, `RpcClient.js`, `RpcSerialization.js` in `effect@4.0.0-rc.115`;
- client: `packages/client-runtime/src/rpc/session.ts:190-206`;
- server: `ws.ts:3858-3873`.

A frame holds one JSON value. That value may also be an array of messages: the client accepts arrays, but the TS server never sends them.

```text
client → server
  {"_tag":"Request","id":<number|string>,"tag":"<method>","payload":<encoded payload>,"headers":[[k,v]…],
   "traceId"?:…,"spanId"?:…,"sampled"?:…,"isNotification"?:true}
  {"_tag":"Ack","requestId":<id>}
  {"_tag":"Interrupt","requestId":<id>}
  {"_tag":"Ping"}
  {"_tag":"Eof"}                                   (never sent by the socket client)
server → client
  {"_tag":"Chunk","requestId":<id>,"values":[<encoded item>, …]}      (non-empty array)
  {"_tag":"Exit","requestId":<id>,"exit":<ExitEncoded>}
  {"_tag":"Defect","defect":<encoded defect>}      (kills every pending request on that socket)
  {"_tag":"Pong"}
ExitEncoded = {"_tag":"Success","value":<encoded success | null for streams>}
            | {"_tag":"Failure","cause":[ {"_tag":"Fail","error":<tagged error>}
                                        | {"_tag":"Die","defect":<defect>}
                                        | {"_tag":"Interrupt","fiberId":<number|null>} ]}
```

Frames captured from the real client and server encoders (rc.115):

```text
C→S {"_tag":"Request","id":0,"tag":"subscribeServerConfig","payload":{"environmentThemes":true,"usageLimitSources":true,"usageLimitsCommand":true},"traceId":"5479…","spanId":"72e2…","sampled":true,"headers":[]}
S→C {"_tag":"Chunk","requestId":0,"values":[{"version":1,"type":"snapshot","config":{…ServerConfig…}}]}
C→S {"_tag":"Ack","requestId":0}
C→S {"_tag":"Request","id":1,"tag":"orchestration.subscribeShell","payload":{"afterSequence":8084,"requestCompletionMarker":true},"headers":[]}
S→C {"_tag":"Chunk","requestId":1,"values":[{"kind":"thread-upserted",…},{"kind":"synchronized"}]}
C→S {"_tag":"Ack","requestId":1}
C→S {"_tag":"Request","id":2,"tag":"orchestration.dispatchCommand","payload":{"type":"thread.turn.start","commandId":"…",…},"headers":[]}
S→C {"_tag":"Exit","requestId":2,"exit":{"_tag":"Success","value":{"sequence":8090}}}
C→S {"_tag":"Request","id":3,"tag":"server.probe","payload":{},"headers":[]}
S→C {"_tag":"Exit","requestId":3,"exit":{"_tag":"Failure","cause":[{"_tag":"Fail","error":{"_tag":"EnvironmentAuthorizationError","message":"…","requiredScope":"orchestration:read"}}]}}
S→C {"_tag":"Exit","requestId":7,"exit":{"_tag":"Failure","cause":[{"_tag":"Die","defect":"Unknown request tag: foo"}]}}
S→C {"_tag":"Exit","requestId":9,"exit":{"_tag":"Failure","cause":[{"_tag":"Die","defect":{"name":"Error","message":"boom"}}]}}
S→C {"_tag":"Exit","requestId":1,"exit":{"_tag":"Success","value":null}}     (end of a stream)
C→S {"_tag":"Ping"}     S→C {"_tag":"Pong"}
```

Rules the Rust implementation must follow:

1. **Request ids.**
   - Ids are JSON **numbers** from a module-global counter in the client. They keep counting across reconnects.
   - Echo `requestId` with **the same JSON type**: the client keys a `Map` by the raw value.
   - Accept string or number. Anything else gets a `Defect`.
2. **Ignore** `traceId`, `spanId`, `sampled` and `headers`. The TS server runs with `disableTracing: true`.
3. **Unary call** → exactly one `Exit`.
4. **Stream call** → zero or more `Chunk`s, then one `Exit`. The success Exit value is `null`. Failure uses `Fail` with an error from the RPC's error union.
5. **Backpressure.**
   - After each `Chunk` the server **waits for the client's `Ack`** before sending the next chunk for that request id (`RpcServer.js` latch, lines 270-300).
   - The server may batch many items into one chunk; batch whatever accumulated while waiting.
   - **Exception:** `terminal.attach` and `subscribeTerminalEvents` use a window of up to **8 unacknowledged chunks or 64 KiB** (`terminal/OutputProtocol.ts`). The TS server synthesizes Acks while the window is not full.
6. **`Interrupt`.** Cancel the handler. The server may then send `Exit` with an interrupt cause; the client has already forgotten the id. An unknown id gets an `Exit(interrupt)`.
7. **Bad requests get a per-request `Exit(Die)`.** That covers an unknown tag or a payload that fails to decode, e.g. `defect: "Unknown request tag: foo"` or the schema issue text. **Avoid `Defect` frames**: the client fails every pending request when it gets one.
8. **Ping/Pong.**
   - The client pings every **5 s** (patched `makePinger`). After **3 consecutive missed Pongs** it declares the socket dead.
   - Answer `Pong` immediately, independently of data traffic.
9. **Never complete `subscribeServerConfig`.** The client treats the end of that stream as a lost session and reconnects (`client-runtime/src/rpc/session.ts`).
10. **Closing the socket**, even with code 1000, ends the client session. The client supervisor reconnects with backoff 3 s → 4 s → 8 s → 16 s (`client-runtime/src/connection/supervisor.ts`). Every reconnect refetches the descriptor, opens a new socket and resubscribes everything.
11. **WS upgrade query parameters** (all optional, read leniently, `ws.ts:433-505`):
    - `wsTicket`
    - `clientSurface` (`web|desktop|mobile|cli`), `clientAppVersion`, `clientDeviceType`, `clientOs`
    - `clientWebDeployment`, `clientBrowser`, `clientOsMajorVersion`, `clientDeviceModel`
    - `connectionMethod`
    - `orchestrationProtocol=1` (ignored)

    `clientSurface` and `clientAppVersion` become `metadata.origin` on dispatched events and update `auth_sessions.client_surface` and `client_app_version`.
12. **Per-RPC authorization.** Before the handler runs, check the session's scopes against `RPC_REQUIRED_SCOPES` (`apps/server/src/auth/RpcAuthorization.ts`, one scope per method, reproduced in Appendix A). A missing scope fails with `EnvironmentAuthorizationError{message, requiredScope}`. One method uses a finer rule: `device.list` requires operate when `retryHostId` or `updateTool` is set.

### 1.4 Streams the client keeps open (all push goes through these)

| Stream | First item | Then |
|---|---|---|
| `subscribeServerConfig` | `{version:1,type:"snapshot",config:ServerConfig}`. The session is "ready" only after this item **and** `config.environment.environmentId` matches the descriptor. | `keybindingsUpdated`, `providerStatuses` (debounced), `settingsUpdated`. Also `environmentThemesUpdated` and `usageLimitSourcesUpdated`, **only if** the payload flag asked for them (`rpc.ts:1380-1400`). |
| `subscribeServerLifecycle` | snapshot events sorted by `sequence` (latest `welcome`, latest `ready`) | live events with a higher sequence |
| `subscribeAuthAccess` | `{version:1,revision:1,type:"snapshot",payload:{pairingLinks,clientSessions}}` | `pairingLinkUpserted`, `pairingLinkRemoved`, `clientUpserted`, `clientRemoved`, each with `revision+1` (`ws.ts:384-421`) |
| `orchestration.subscribeShell {afterSequence?, requestCompletionMarker?}` | `{kind:"snapshot",snapshot}`, **or** a replay of shell events after `afterSequence` (only if the gap ≤ 1000) | `{kind:"synchronized"}` if requested, then live `project-upserted/removed`, `thread-upserted/removed`, each with `sequence`. **Live delivery is attached before the snapshot is read** (`ws.ts:2013-2168`). |
| `orchestration.subscribeThread {threadId, reasoningMessages?, afterSequence?, requestCompletionMarker?, turnLimit?}` | `snapshot` or replay (≤ 1000 events and ≤ 8 MiB) | `synchronized`, then `{kind:"event",event:OrchestrationEvent}` filtered to that thread's detail events (`ws.ts:2186-2350`) |
| `subscribeTerminalEvents`, `terminal.attach`, `subscribeTerminalMetadata` | snapshot | events (windowed acks) |
| `subscribeVcsStatus` | `snapshot{local,remote}` | `localUpdated`, `remoteUpdated` |
| `subscribeBackgroundPolicy`, `subscribeResourceTelemetry`, `subscribeDeviceState` | latest snapshot | changes |
| `subscribePreviewEvents`, `subscribeDiscoveredLocalServers`, `subscribeProjectClones`, `subscribeWorktreeSetup`, `pullRequests.subscribeRefreshes`, `previewAutomation.connect` | — | events |
| Stream-shaped commands | `git.runStackedAction`, `server.updateServerWithProgress`, `cloud.installRelayClient`, `provider.chatgpt.handoff.subscribe`, `provider.codex.auth-callback.subscribe`, `provider.auth.subscribe`, `provider.install.subscribe` | progress, then end |

**Sequences must stay stable and monotonic across restarts.** The client caches shell and thread snapshots in IndexedDB (`t3code:connection-runtime` v4) and resumes with `afterSequence`. This is why the Rust server keeps `orchestration_events.sequence` exactly as it is.

### 1.5 How Effect Schema encodes on the wire (verified with rc.115)

The codec is `Schema.toCodecJson` (`RpcSerialization.js:9`). Each row below was verified by encoding with rc.115:

| Schema | JSON | Rust/serde |
|---|---|---|
| `Schema.optionalKey(X)` (388 uses) | key absent, or X; **`null` is rejected** | `Option<T>` + `skip_serializing_if = "Option::is_none"`; never emit null |
| `Schema.optional(X)` (= `optionalKey(UndefinedOr(X))`, 893 uses) | absent; decode also accepts `null` | same as above, `#[serde(default)]` |
| `Schema.NullOr(X)` (356 uses) | X or `null`, **key required** | `Option<T>` without skip |
| `withDecodingDefault` (194 uses) | decode tolerates a missing key; encode always writes it | `#[serde(default = …)]` and always serialize |
| `Schema.Number` unrefined (110) | finite → number; `NaN`/`Infinity`/`-Infinity` → **strings** | custom `JsNumber(f64)` serializer |
| `Schema.Int` (82), `Finite` (14) | number | `i64` / `f64` |
| `Schema.DateTimeUtc` (41) | `"2026-10-01T12:00:00.000Z"` (ms precision, `Z`) | newtype over `jiff::Timestamp` or `chrono::DateTime<Utc>` with a fixed formatter |
| `Schema.Option(X)` (21: resourceTelemetry, sourceControl, vcs, server.ts) | `{"_tag":"Some","value":…}` / `{"_tag":"None"}` | generated `enum EOption<T>` with `#[serde(tag="_tag")]` |
| `Schema.OptionFromNullOr` | value or `null` | `Option<T>` |
| `Schema.Uint8Array` (1, `ipc.ts`) | base64 | `base64` serde helper |
| `Schema.BigInt`, `Duration` | `"12"`, `{"_tag":"Millis","value":3000}` | not used in contracts |
| Branded strings/numbers (`ThreadId`, `ProjectId`, `CommandId`, `EventId`, `MessageId`, `TurnId`, `ApprovalRequestId`, `ProviderInstanceId`, …, `baseSchemas.ts:133-194`) | the plain value | `#[serde(transparent)]` newtypes |
| `TrimmedString` / `TrimmedNonEmptyString` | trimmed on decode **and** encode | newtype that trims in `Deserialize` |
| `Schema.TaggedError` (125 classes) / `TaggedStruct` | `{"_tag":"Name", …declared fields}`; no `message`/`stack` unless declared | struct with `_tag` literal |
| `Schema.Literal(s)` unions | the literal | `#[serde(rename = "…")]` unit enums |
| Discriminated unions | discriminator is **`type`** for orchestration commands/events, `ServerConfigStreamEvent` and auth events; **`kind`** for stream items; **`_tag`** for errors, `VcsStatusStreamEvent`, Exit/Cause | `#[serde(tag = "type")]` etc. |
| Forward-compatible helpers `ForwardCompatibleOptional`, `ForwardCompatibleNullable`, `OmittedWhenNull`, `ForwardCompatibleArray` (`baseSchemas.ts:51-131`) | decode drops unknown members instead of failing | hand-written serde helpers |
| `Schema.Defect()` and every `Die` | `Error` → `{"name","message"}`; other values pass through | `serde_json::Value` |
| `Schema.Unknown` | passthrough | `serde_json::Value` |
| Struct key order | declaration order (cosmetic) | `serde_json` with `preserve_order` |

Extra keys are ignored on decode and the client tolerates unknown union members where forward-compatible helpers are used. **Exact key absence still matters**: `optionalKey` with `null` is a decode error.

---

## 2. Auth and pairing

Source: `apps/server/src/auth/**` (4.3k lines), `packages/contracts/src/auth.ts`.

### 2.1 Model

- **Scopes, not roles.** There are eight: `orchestration:read`, `orchestration:operate`, `terminal:operate`, `review:write`, `access:read`, `access:write`, `relay:read`, `relay:write`.
  - *Standard client* = the first four plus `relay:read`.
  - *Administrative* ("owner") = standard plus `access:read`, `access:write`, `relay:write` (`auth.ts:81-115`).
- **Policy** (`auth/EnvironmentAuthPolicy.ts`): `desktop-managed-local | loopback-browser | remote-reachable | unsafe-no-auth`. zenith runs `serve --host 127.0.0.1`, so its policy is `loopback-browser` with bootstrap method `one-time-token`.
- **Pairing is required even on loopback.**

### 2.2 Session tokens (custom, not JWT)

`auth/SessionStore.ts` (1,053 lines), `auth/utils.ts`.

```
token     = base64url(JSON(claims)) + "." + base64url(HMAC-SHA256(key, encodedPayload))
claims    = {v:1, kind:"session", sid:<uuidv4>, sub, scopes:[…], method, jkt?, iat:<epoch ms>, exp:<epoch ms>}
ws ticket = {v:1, kind:"websocket", sid, iat, exp}    (5 min, stateless, reusable within its window)
```

- **Key:** 32 random bytes in `<stateDir>/secrets/server-signing-key.bin`. The directory is 0700 and the file 0600. It is created with `O_EXCL`; if another process wins the race, the loser re-reads the file.
- **Verification:** split, then constant-time HMAC check, then strict claims decode, then `exp > now`, then a DB row exists for `sid` and is not revoked.
- **Lifetimes:** session 30 days, DPoP access token 1 h, ws ticket 5 min, CLI `session issue` 30 days by default.
- **"Connected"** is an in-memory refcount per `sid`, kept around each `/ws` connection. It feeds `connected`, `last_connected_at`, and the rule that a connected session is listed even after it expires.

### 2.3 Pairing credentials

`auth/PairingGrantStore.ts`.

- **Format:** 12 characters from `23456789ABCDEFGHJKLMNPQRSTUVWXYZ`, generated by rejection sampling.
- **Lifetime:** TTL 5 min by default, one-time use, stored in plaintext in `auth_pairing_links.credential` (UNIQUE).
- **Consumption** is one atomic statement:
  ```sql
  UPDATE auth_pairing_links SET consumed_at=? WHERE credential=? AND revoked_at IS NULL AND consumed_at IS NULL
    AND expires_at > ? AND (proof_key_thumbprint IS NULL OR proof_key_thumbprint = ?) RETURNING …
  ```
- **Startup credential** (`serve`): subject `administrative-bootstrap`, administrative scopes, printed in the banner, hidden from lists.
- **Not needed in zenith:** the desktop bootstrap token (fd) and the reusable dev token (`T3CODE_DEV_AUTH_TOKEN`).

### 2.4 Cookie and credential selection

- **Cookie name** (`auth/utils.ts:25-53`). zenith's case is web mode on loopback: `t3_session_<port>_<sha256(stateDir path)[0..12]>`. Desktop mode uses `t3_session_<port>`; a remote-reachable host uses `t3_session_<sha256(environmentId)[0..12]>`, plus the legacy `t3_session`. The client reads the name from `AuthSessionState.auth.sessionCookieName`. The name must still be derived identically, so cookies that browsers already hold keep matching.
- **Cookie attributes:** `HttpOnly; Path=/; SameSite=Lax; Expires=<exp>`, no `Secure`. Credential responses carry `cache-control: no-store` and `pragma: no-cache`.
- **Credential selection order:** session cookie → `Authorization: Bearer` → `Authorization: DPoP` (requires a valid `DPoP` proof when the token has `jkt`) → legacy cookie.
- **`/ws` upgrade:** `?wsTicket=` first, then the same selection (`EnvironmentAuth.ts:1075-1095`).

### 2.5 DPoP

Only T3 Connect relay clients use DPoP (`auth/dpop.ts`, `packages/shared/src/dpop.ts`).

- The proof is ES256 with a P-256 JWK. The server checks `htm`, `htu` (rebuilt from `Host` and `x-forwarded-proto`), `iat` within +5/−300 s, and `ath`.
- Replay markers are `secrets/dpop-proof-<b64url(sha256(jkt:jti))>.bin`, created with `O_EXCL` and swept after one day (`auth/replayMarkers.ts`).
- **Port this last.** Keep `/oauth/token` with Bearer: `client-runtime` uses it to pair remote environments.

### 2.6 CSRF, Origin, CORS

- **There is no Origin check or CSRF token anywhere**, including `/ws`. Protection relies on `SameSite=Lax` and same-site loopback.
  - Recommendation: a Rust-only hardening that changes nothing for valid clients. On `/ws` and on cookie-authenticated POSTs, accept `Origin` ∈ {self, `ZENITH_CODE_PARENT_ORIGINS`} or no Origin.
- **CORS** (`http.ts:240-265`, `httpCors.ts`): methods `GET, POST, OPTIONS`; headers `authorization, b3, traceparent, content-type, dpop`; max-age 600; `Access-Control-Allow-Origin: *` without credentials. In dev (`--dev-url`) it switches to an allowlist with credentials. `server.test.ts` asserts these headers (`assertBrowserApiCorsResponseHeaders`).

### 2.7 zenith specifics (`apps/server/src/zenith/embed.ts`, 50 lines)

- `GET /zenith/embed.json` → `{"parentOrigins":["http://127.0.0.1:4747","http://127.0.0.1:4748"]}`, public, `cache-control: no-store`.
- The list comes from env `ZENITH_CODE_PARENT_ORIGINS`, comma-separated. An entry is kept only if it is an http(s) URL whose `origin` equals the entry exactly; duplicates are removed.
- `Content-Security-Policy: frame-ancestors 'self' <origins>` goes on **HTML from the static handler only** (SPA `index.html` and fallbacks). It does not go on JSON, `/ws`, assets or `embed.json`.
- Pairing inside the dashboard:
  1. The iframe at `/pair` posts `zenith-code:pair-request`.
  2. The parent calls its own `POST /api/code/pair`, which runs `bin.mjs auth pairing create --ttl 2m --admin --label zenith --json`.
  3. The parent answers `zenith-code:pair-token`.
  4. The app calls `POST /api/auth/browser-session`, which sets the 30-day cookie.

  No server change is involved beyond the CLI output contract (§6.17).

### 2.8 `subscribeAuthAccess` and CLI-made credentials

Changes come from in-process pub/sub on `PairingGrantStore` and `SessionStore`. Credentials made by the CLI process do not appear live today; the TS server has the same limitation, so the port does not need to fix it.

---

## 3. Persistence and file layout

### 3.1 Database

- **Engine:** SQLite through **`node:sqlite` (`DatabaseSync`)**, wrapped by `packages/shared/src/nodeSqliteClient.ts` (285 lines). That is one synchronous connection, serialized by a 1-permit semaphore, with a statement cache of 200 entries. Transactions use `BEGIN`/`COMMIT`; nested transactions use `SAVEPOINT`.
- **File:** `<baseDir>/userdata/state.sqlite`. With `--dev-url` and no explicit `--base-dir`, it is `<baseDir>/dev/state.sqlite`.
- **Pragmas** (`persistence/Layers/Sqlite.ts:14-27`):
  ```sql
  PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA journal_size_limit = 33554432;
  ```
- **Value conventions:**
  - Timestamps are TEXT `YYYY-MM-DDTHH:mm:ss.sssZ`. They are compared lexicographically and with `julianday()`.
  - Booleans are INTEGER 0/1. JSON goes in TEXT columns (`json_extract`, `json_set`). IDs are TEXT.
- **The CLI process writes the same DB concurrently** (`auth …`, `project …`). That is why WAL and `busy_timeout` are used.

**Migrations.** They are tracked in `effect_sql_migrations(migration_id INTEGER PK, created_at DATETIME DEFAULT current_timestamp, name VARCHAR(255))`. The protocol runs in one transaction: read `max(migration_id)` → insert the rows of all pending migrations (a PK conflict means another process is migrating, so do nothing) → run the migrations in order.

There are **54 migrations**: `persistence/Migrations/001_OrchestrationEvents.ts` … `054_ProjectionThreadsAutoSettleDisabledAt.ts`. A live database (`~/.zenith/code`, checked 2026-10-01) has all 54 applied, about 8k events, and is 50 MB.

**Rust rule:**
- Implement this exact protocol yourself (≈150 lines). Do not use refinery, sqlx-migrate or rusqlite_migration.
- On a fresh DB, apply the consolidated DDL, then insert rows 1–54 with the original names.
- On an existing DB at 54, do nothing.
- If the max id is above the highest the Rust build knows, refuse to start.
- **Add no new migration until the TS server is retired.** If one becomes unavoidable, number it 55+ and land the same migration in TS.

**Final schema** (verified against the live DB's `.schema`):
- Event store: `orchestration_events`, `orchestration_command_receipts`.
- Projections: `projection_projects`, `projection_threads` (31 columns), `projection_thread_messages`, `projection_thread_activities`, `projection_thread_sessions`, `projection_turns`, `projection_pending_approvals`, `projection_thread_proposed_plans`, `projection_thread_pull_requests`, `projection_state`.
- Provider: `provider_session_runtime`.
- Auth: `auth_pairing_links`, `auth_sessions`.
- Pull requests: `pull_request_files_viewed` (WITHOUT ROWID).
- `checkpoint_diff_blobs` (dead), `sqlite_sequence`.

The reconstructed DDL with every column and index, and the migration-by-migration table, are in [Appendix B](#appendix-b-final-sqlite-schema). The migration files themselves are the source of truth. CI must diff `sqlite_master` between a TS-created and a Rust-created DB.

**Repositories.** These are the persistence services that need porting:

| Repository | Methods |
|---|---|
| `OrchestrationEventStore` | `append` (INSERT…RETURNING, `stream_version` via `COALESCE(max+1,0)`, `actor_kind` inferred from the `commandId` prefix `provider:`/`server:`), `readFromSequence` (pages of 500), `readAggregateRange`, `getAggregateReplayStats` (count and payload bytes in SQL), `readAll`, `hasEventAfter` |
| `OrchestrationCommandReceiptRepository` | `upsert`, `getByCommandId` |
| `ProjectionProjectRepository` | `upsert`, `getById` |
| `ProjectionThreadRepository` | `upsert`, `getById` |
| `ProjectionThreadMessageRepository` | `upsert`, `appendStreaming` (SQL-side text concatenation), `getByMessageId`, `hasAssistantMessageForTurn`, `listByThreadId`, `getLatestUserMessageAt`, `deleteByThreadId` |
| `ProjectionThreadActivityRepository` | `upsert`, `listByThreadId`, `listUserInputLifecycleByThreadId`, `getLatestTaskActivity`, `deleteByThreadId` |
| `ProjectionThreadSessionRepository` | `upsert`, `getByThreadId`, `deleteByThreadId` |
| `ProjectionTurnRepository` | `upsertByTurnId`, `replacePendingTurnStart`, `getPendingTurnStartByThreadId`, `deletePendingTurnStartByThreadId`, `listByThreadId`, `getByTurnId`, `clearCheckpointTurnConflict`, `deleteByThreadId` |
| `ProjectionPendingApprovalRepository` | `upsert`, `listByThreadId`, `countPendingByThreadId`, `getByRequestId`, `deleteByThreadId` |
| `ProjectionThreadProposedPlanRepository` | `upsert`, `getByPlanId`, `listByThreadId`, `hasActionableByThreadId`, `deleteByThreadId` |
| `ProjectionStateRepository` | `upsert`, `upsertMany`, `getByProjector`, `listAll` |
| `ProjectionThreadPullRequestRepository` | `upsert`, `listByThreadId`, `listByPullRequest`, `delete`, `deleteByThreadId`, `deleteByThreadIdAndSource` |
| `ProviderSessionRuntimeRepository` | `upsert`, `recordImportedTranscript`, `getByThreadId`, `list`, `deleteByThreadId` |
| `AuthSessionRepository` | `create`, `createReplacingActive`, `createIfAbsent`, `getById`, `listActive`, `revoke`, `revokeAllExcept`, `setLastConnectedAt`, `setClientConnection` |
| `AuthPairingLinkRepository` | `create`, `consumeAvailable`, `listActive`, `revoke`, `getByCredential` |
| `PullRequestFilesViewedRepository` | `list`, `set` (max 500 rows) |

`ProjectionSnapshotQuery` (3,870 lines of hand-written SQL: keyset pagination, search, shell and thread snapshots) belongs to orchestration (§5). **Port its SQL verbatim.**

**JSON columns are wire-encoded values.** That covers `payload_json`, `metadata_json` and the `*_json` projection columns. Rust must write exactly what the TS encoder writes (absent vs null, encoded `ProjectIconOverride`, …), because the TS server, the CLI and a rollback will read it.

### 3.2 Files under the base dir (`~/.zenith/code`, `--base-dir`, env `T3CODE_HOME`)

Paths come from `config.ts:132-165` (`deriveServerPaths`), `os-jank.ts:105` and the modules.

```
<baseDir>/
  userdata/                         stateDir ("dev/" when --dev-url and baseDir implicit)
    state.sqlite (+ -wal, -shm)
    settings.json                   ServerSettings, sparse (defaults stripped); read directly by the dashboard
    client-settings.json            present on disk but nothing in apps/server writes it (the web keeps client settings in
                                    localStorage `t3code:client-settings:v1`); leave it alone
    keybindings.json                [{key, command, when?}] ≤ 256 rules
    environment-id                  UUID; read directly by the dashboard (thread URLs)
    anonymous-id
    server-runtime.json             {version:1,pid,host?,port,origin,devUrl?,startedAt,serviceManaged?}; dashboard adopts orphans with it
    themes/*.json                   environment themes (≤32 files, 32 KiB each)
    attachments/                    <threadSegment>-<uuid>[-ext].<ext>; "pending-…" uploads; *.part
    browser-artifacts/              MCP preview screenshots
    secrets/<name>.bin              0700/0600: server-signing-key, asset-access-signing-key, cloud-link-ed25519-key-pair,
                                    provider-env-<b64url(instance)>-<b64url(name)>, provider-auth-<sha256>, dpop-proof-*, cloud-* …
    logs/server.trace.ndjson(.1…10) span trace, 10 MiB × 10 (read by `trace summary` and server.getTraceDiagnostics)
    logs/provider/events.<threadId>.log(.N), events._global.log    "[ISO] NTIVE|CANON|ORCH: <json>" (the typo NTIVE is real)
    logs/terminals/terminal_<b64url(threadId)>[_<b64url(terminalId)>].log
    model-manifest.json             cached remote provider manifest
    usage-model-rates.json          LiteLLM price table cache (24 h)
    usage-scan-cache.json           transcript scan resume positions
    providers/codex/<instanceId>/shadow, providers/antigravity/…, antigravity-tmp/
  worktrees/<repoName>/<branch with / → ->/      thread worktrees
  caches/<instanceId>.json, caches/pull-requests/  provider status cache, PR cache
  tools/codex/<version>/ (+ active.json), tools/antigravity-acp/…, tools/<npm tool>/<version>   managed installs
  runtime/…                         upstream service launcher only: not used by zenith
  zenith-projects.json              written by the dashboard, not the server
```

The same files hold settings, keybindings and secrets in both TS and Rust. Secret values are raw bytes, or JSON for the ed25519 pair.

---

## 4. Providers

Source: `apps/server/src/provider/**` (53k lines), `packages/effect-acp`, `packages/effect-codex-app-server`, contracts `providerRuntime.ts`, `providerInstance.ts`, `providerSetup.ts`, `model.ts`, `providerUsageLimits.ts`.

### 4.1 Drivers

There are six built-in drivers (`provider/builtInDrivers.ts`). The driver-kind strings are persisted and must not change. Every driver supports multiple instances: `ServerSettings.providerInstances: Record<instanceId, {driver, displayName?, accentColor?, environment?, enabled?, config?}>`, and the default instance id is the kind itself.

| driverKind | Transport | Binary / argv |
|---|---|---|
| `claudeAgent` | `claude` CLI, stream-json + control protocol (via `@anthropic-ai/claude-agent-sdk` 0.3.276, pinning CLI 2.1.276) | see 4.2 |
| `codex` | NDJSON JSON-RPC **without the `"jsonrpc"` field** | `codex app-server [launchArgs] [-c mcp_servers.t3-code.url=… -c mcp_servers.t3-code.bearer_token_env_var="T3_MCP_BEARER_TOKEN"]`; env `CODEX_HOME`, `T3_MCP_BEARER_TOKEN` |
| `cursor` | ACP (JSON-RPC 2.0, NDJSON stdio) | `cursor-agent [-e endpoint] [--auto-review\|--force] acp` |
| `grok` | ACP | `grok --permission-mode default\|acceptEdits\|auto agent stdio`; full-access: `grok agent --always-approve stdio` |
| `antigravity` | ACP | managed `agy-acp-server` from `dl.google.com` under `<baseDir>/tools/antigravity-acp/…` |
| `opencode` | HTTP REST + SSE (`@opencode-ai/sdk/v2`) | `opencode serve --hostname=127.0.0.1 --port=<free>`, `OPENCODE_SERVER_PASSWORD`, Basic auth; or an external URL |

### 4.2 Claude, without the Node SDK

The server never uses the SDK's bundled binary. It sets `pathToClaudeCodeExecutable` to the instance's `binaryPath` (default `claude`), and per-instance isolation sets `CLAUDE_CONFIG_DIR`. `HOME` is left untouched because changing it breaks the keychain.

Rust spawns the CLI exactly as the SDK does (`sdk.mjs initialize()`):

```
<claude> --output-format stream-json --verbose --input-format stream-json \
  [--thinking adaptive --thinking-display summarized] [--effort <e>] [--model <id>] \
  --permission-prompt-tool stdio [--resume=<uuid> | --session-id=<uuid>] \
  [--mcp-config '{"mcpServers":{"t3-code":{"type":"http","url":"http://127.0.0.1:<port>/mcp","headers":{"Authorization":"Bearer …"}}}}'] \
  --setting-sources=user,project,local [--permission-mode acceptEdits|auto|bypassPermissions] [--allow-dangerously-skip-permissions] \
  --include-partial-messages --add-dir <cwd> --add-dir <attachmentsDir> [--settings '<json>'] [<user launchArgs>]
env: CLAUDE_CODE_ENTRYPOINT=sdk-ts (unless set), NODE_OPTIONS and DEBUG removed, CLAUDE_CONFIG_DIR=<homePath>
```

**Control protocol** (NDJSON on stdin and stdout):

| Direction | Messages |
|---|---|
| Out | `{"type":"control_request","request_id","request":{subtype:…}}` with subtypes `initialize` (sent first: `appendSystemPrompt`, `supportedDialogKinds:["resume_return"]`, `hooks:null`), `set_model`, `set_permission_mode` (plan ↔ base), `interrupt`, `get_usage` (probe); `control_cancel_request` |
| In | `control_response`; `can_use_tool` → `{"behavior":"allow","updatedInput","updatedPermissions"?,"toolUseID"}` or `{"behavior":"deny","message","toolUseID"}`; `request_user_dialog` (answer only declared kinds); `elicitation` → `{action:"decline"}`; `control_cancel_request`; `keep_alive` |
| User turns | `{"type":"user","session_id":"","parent_tool_use_id":null,"message":{"role":"user","content":[…image base64…, {type:"text"}]}}`. A send during a running turn is a **steer** (no new turn boundary). |

Special cases:
- `AskUserQuestion` becomes a user-input request; the answer key is **the question text**.
- `ExitPlanMode` becomes a proposed plan plus a deny reply.
- The skill mention `$name` is rewritten to `/name` in the last text block.
- Compaction is the turn `/compact`.

Stream message types: `system` (about 60 subtypes), `stream_event`, `assistant`, `user`, `result`, `rate_limit_event`, `auth_status`, `tool_progress`, `tool_use_summary`. Map only what `ClaudeAdapter.ts` maps, with `#[serde(other)]`/`Value` passthrough for the rest.

**Session history** (`claudeHistoryWorker.ts`) uses `getSessionMessages` and `forkSession`. These are pure JSONL operations on `$CLAUDE_CONFIG_DIR/projects/<encoded-cwd>/<sessionId>.jsonl`; in Rust they are file I/O with no worker process.

The resume cursor is `{threadId, resume, resumeSessionAt?, turnCount, turnStartMessageIds?}`.

### 4.3 Codex

Runtime mode mapping (`CodexSessionRuntime.ts:515`):

| Runtime mode | approvalPolicy | sandbox | approvalsReviewer |
|---|---|---|---|
| approval-required | `untrusted` | `read-only` | `user` |
| auto-accept-edits | `on-request` | `workspace-write` | `user` |
| auto | `on-request` | `workspace-write` | `auto_review` |
| full-access | `never` | `danger-full-access` | `user` |

Methods used:
- Handshake: `initialize` (clientInfo name "T3 Code"; the brand build plugin turns it into "zenith code"), `initialized`.
- Threads: `thread/start|resume|read|turns/list|revert|inject_items|compact/start`.
- Turns and config: `turn/start|interrupt`, `config/mcpServer/reload`, `feedback/upload`.
- Probes and credits: `account/read`, `account/rateLimits/read`, `model/list`, `skills/list`, `account/rateLimitResetCredit/consume`.

Server requests handled:
- `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`, `item/permissions/requestApproval`
- `mcpServer/elicitation/request`, `item/tool/requestUserInput`
- legacy approvals

All 84 notifications are subscribed; the adapter maps about 30 of them. That includes synthetic `collabAgent/*` events for sub-agent threads.

Fixtures: `provider/testFixtures/codexMultiAgentWire.json` (a real capture, codex-cli 0.145) and `codexCollabMockPeer.mjs`, a standalone stdio replayer that a Rust test can spawn.

Managed mode (`CodexManagedRuntime.ts`) adds `-c model_provider="openai_token_sharing" …` and passes the token as `ACCESS_TOKEN`. Non-default instances get a shadow `CODEX_HOME` materialized by `Drivers/CodexHomeLayout.ts` (422 lines).

### 4.4 ACP (Cursor, Grok, Antigravity)

All three share `acp/AcpSessionRuntime.ts` (1,354 lines).

Sequence:
1. `initialize {protocolVersion:1, clientCapabilities, clientInfo}`
2. `authenticate {methodId}`
3. `session/new` or `session/load`. For `load`, wait until the replayed `session/update`s go idle. Use `session/resume` instead only if the agent advertises it.
4. `session/set_config_option`, `session/set_model`, `session/set_mode`
5. `session/prompt`
6. `session/cancel` (a notification) to interrupt

The client handles `session/update`, `session/request_permission` (`{outcome:{outcome:"selected",optionId}}`) and the extensions:
- `cursor/ask_question`, `cursor/create_plan`, `cursor/update_todos`, `cursor/list_available_models`
- `_x.ai/ask_user_question`, `_x.ai/exit_plan_mode`, `_x.ai/session/prompt_complete`
- Antigravity's `interaction_*` permission requests, which are really questions

MCP is passed as `mcpServers:[{type:"http",name:"t3-code",url,headers:[{name:"Authorization",value}]}]`.

### 4.5 OpenCode

`opencodeRuntime.ts` (1.1k) and `OpenCodeAdapter.ts` (4.1k). There is one shared server per instance, closed after 30 s idle. Readiness is detected from stdout, then `GET /global/health`; the minimum version is 1.14.19.

Calls:
- `session.create|get|update|fork|children|messages|message|status|summarize|command|promptAsync|abort`
- `permission.list|reply` (always `once`), `question.list|reply`
- `mcp.add`, `provider.list`, `app.agents|skills`, `command.list`
- SSE `/event`

Types: write them by hand with serde, or generate a client from OpenCode's OpenAPI with `progenitor`.

### 4.6 Internal event model

`ProviderRuntimeEvent` (`contracts/providerRuntime.ts`, 1,235 lines) is a union of 48 `type`s:
- `session.*`, `thread.*`, `turn.*`
- `item.started|updated|completed`, `content.delta`
- `request.opened|resolved`, `user-input.*`
- `task.*`, `hook.*`, `tool.*`
- `auth.status`, `account.*`, `mcp.*`
- `model.rerouted`, `runtime.warning|error`, …

**The web app never sees it.** It is consumed by `orchestration/Layers/ProviderRuntimeIngestion.ts`, so the Rust port is free to model it as it likes. In practice, keep the same JSON so the `CANON:` lines in the existing `logs/provider/events.*.log` files work as fixtures.

Other subsystems:

| Subsystem | Details |
|---|---|
| Approvals | `accept \| acceptForSession \| acceptAlways \| decline \| cancel`; pending approvals settle as `cancel` on close |
| Plan mode | `interactionMode: "default" \| "plan"` |
| Interrupt | per transport (4.2–4.5) |
| Resume | `ProviderSessionDirectory` over `provider_session_runtime`, `ProviderService.recoverSessionForThread` |
| Reaper | stops sessions idle 30 min, sweeping every 5 min |
| Attachments | adds `[Attached image "x" is saved at: …]` lines, plus native image parts |
| Registries | `ProviderInstanceRegistry` (instances from settings), `ProviderAdapterRegistry`, `ProviderRegistry` (status snapshots that feed `subscribeServerConfig.providerStatuses`, cached in `caches/<instanceId>.json`) |
| Model manifest | `provider/model-manifest.json`, refreshed from `raw.githubusercontent.com/pingdotgg/t3code/main/apps/server/src/provider/model-manifest.json`, cached in `userdata/model-manifest.json`; CLI version-compatibility ranges |
| Auth flows | Codex managed ChatGPT OAuth (OIDC discovery, dynamic registration, PKCE loopback at `127.0.0.1:<p>/auth/callback`, JWKS check, tokens in `secrets/provider-auth-*`, `proper-lockfile` lock); Antigravity Google sign-in callback forwarding. Other providers only detect the CLI's own login. |
| Installers | Codex from GitHub releases (`rust-v0.156.1`, sha256-pinned tarballs, 6 targets); Antigravity zip (`yauzl`); self-update via npm/pnpm/bun/brew or `<bin> update` |
| Usage limits | Claude `get_usage` + `rate_limit_event` + `api.anthropic.com/api/oauth/usage`; Codex `account/rateLimits/*`; Cursor `api2.cursor.sh` with a keychain token (`keyring` service `cursor-access-token`, account `cursor-user`); Grok billing endpoint; OpenCode zen usage |
| Event logs | `EventNdjsonLogger.ts` (805 lines): 64 KiB/record cap, 10 MiB × 10 rotation, 512 MiB total, 14 days |

`ProviderService` is the public interface. Its methods are `startSession`, `sendTurn`, `compactThread`, `interruptTurn`, `respondToRequest`, `respondToUserInput`, `stopSession`, `listSessions`, `getCapabilities`, `getInstanceInfo`, `assertConversationRollbackSupported`, `rollbackConversation`, `uploadFeedback`, and the `streamEvents` fan-out. The only callers are the orchestration reactors and startup.

### 4.7 The generated packages, and how to get Rust types

| Package | Generator | Input | Rust strategy |
|---|---|---|---|
| `effect-acp` (`src/_generated/schema.gen.ts` 10.4k, `meta.gen.ts`) | `packages/effect-acp/scripts/generate.ts` (`pnpm --filter effect-acp generate`) | `schema.unstable.json` + `meta.unstable.json` from `github.com/agentclientprotocol/agent-client-protocol/releases/download/v0.11.3/`, through `@effect/openapi-generator` `JsonSchemaGenerator` | Use the **official `agent-client-protocol` / `agent-client-protocol-schema` crates** at the release matching v0.11.3, with the `unstable` features (`session/resume|fork|list`, `set_model`, `set_config_option`, elicitation). Extensions go through the crate's ext hooks. Fallback: `typify` on the same JSON. |
| `effect-codex-app-server` (`schema.gen.ts` 56.7k, `meta.gen.ts` 889, `namespaces.gen.ts` 295) | `packages/effect-codex-app-server/scripts/generate.ts` | every JSON Schema under `codex-rs/app-server-protocol/schema/json{,/v1,/v2}` at **`openai/codex@fe74a774532af67b5a4a3dec03ce9469e17f89af`** (GitHub contents API), with `$ref` rewrites, `V1`/`V2` prefixes, three hand-added schemas, and `PlanType` forced to `string`; method maps regex-parsed from the ts-rs `schema/typescript/*.ts` | Codex itself is Rust (`codex-app-server-protocol`: serde + schemars + ts-rs). **Recommended:** `typify` over the same pinned schema tree, keeping only the ~30 methods and notifications used, with `#[serde(other)]` and `String` fallbacks (the `PlanType` lesson). The alternative, a git dependency on `openai/codex` at the pinned rev, gives exact types but drags in its dependency tree and toolchain. Keep the no-`jsonrpc` quirk. The pin (fe74a77) and the installed CLI (0.156.1) drift; record which one the Rust types follow. |
| Claude | none | the SDK's `sdk.d.ts` | hand-written serde for the subset in 4.2, `Value` passthrough for the rest |

---

## 5. Orchestration

Source: `apps/server/src/orchestration/**` (20.4k lines), `checkpointing/`, `background/`, `review/`, `attachmentStore.ts`, `storageCleanup.ts`, and **the logic inside `ws.ts`**: the turn-start bootstrap at about lines 1054–1640 and the subscriptions at 836–1060 and 2013–2400. Contracts: `packages/contracts/src/orchestration.ts` (2,441 lines).

### 5.1 Model

**Event-sourced CQRS.** There are two aggregate kinds, `project` and `thread`, and **one global serialized writer**: a single queue and worker fiber (`Layers/OrchestrationEngine.ts`).

- `decider.ts` (2,226 lines) is pure: `(command, commandReadModel) → event | event[]`. It throws typed invariant errors (`commandInvariants.ts`, `Errors.ts`).
- `projector.ts` (1,099 lines) keeps the in-memory *command read model* (an `OrchestrationReadModel` with caps: 2,000 messages and 500 checkpoints per thread). At boot this model is loaded from the projection tables, not rebuilt from the log.
- `Layers/ProjectionPipeline.ts` (2,222 lines) runs nine SQL projectors, each with its own cursor in `projection_state`:
  1. projects
  2. thread-messages
  3. thread-proposed-plans
  4. thread-activities
  5. thread-sessions
  6. thread-turns
  7. checkpoints
  8. pending-approvals
  9. threads (last; computes the shell summary counts)

  A tenth cursor, `projection.attachment-cleanup`, drives attachment deletion.
- **`snapshotSequence`** is the *minimum* `last_applied_sequence` over the required projectors (`ProjectionSnapshotQuery.ts:319`).

### 5.2 Dispatch transaction

`processEnvelope` must be reproduced exactly:

1. **Receipt check.** Look up the receipt by `commandId`.
   - Same aggregate and `accepted` → return `{sequence: result_sequence}` (idempotent).
   - `rejected` → `OrchestrationCommandPreviouslyRejectedError`.
   - Different aggregate → `OrchestrationCommandIdConflictError`.
2. **Guards.**
   - `thread.auto-settle` is rejected if an event exists after `snapshotSequence`, or if background work is live.
   - `thread.pull-request.sync` is rejected if the thread was recreated.
   - `thread.user-input.respond|dismiss` loads the request activity from SQL first.
3. **Decide.** `decide`, then stamp `metadata.origin` (client surface, appVersion) on every event.
4. **One SQL transaction:** for each event, `eventStore.append` → `projectEvent` (in-memory copy) → all nine projectors (nested SAVEPOINT) → `upsertMany` of the cursors. Then upsert the `accepted` receipt with the last sequence.
5. **After commit:** swap in the new command model → delete attachment files → publish each event on the in-process pub/sub (every subscriber gets everything, unbounded) → resolve `{sequence}`.
6. **On failure:** re-read the events persisted since the start, re-project and publish them. On a domain rejection, upsert a `rejected` receipt.

**Command-id conventions** (persisted; migration 046 depends on them): `server:<tag>:<uuid>`, `server:auto-settle:<threadId>:<uuid>`, `server:pr-sync:…`, `provider:<runtimeEventId>:<tag>:<uuid>`, `session-stop-for-archive:<cmdId>`, …

### 5.3 Commands and events (discriminator `type`)

- **Client commands** (`ClientOrchestrationCommand`, `orchestration.ts:1427-1492`):
  - Projects: `project.create|meta.update|delete`
  - Thread lifecycle: `thread.create|delete|archive|unarchive|settle|unsettle|snooze|unsnooze|pin|unpin|pin.reorder|auto-settle.set|active.reorder|meta.update`
  - Pull requests: `thread.pull-request.link|unlink`
  - Modes: `thread.runtime-mode.set`, `thread.interaction-mode.set`
  - Turns: `thread.turn.start` (with `bootstrap{createThread?,prepareWorktree?,runSetupScript?}`, attachments, `sourceProposedPlan`), `thread.turn.interrupt`
  - Requests: `thread.approval.respond`, `thread.user-input.respond|dismiss`
  - Revert and stop: `thread.checkpoint.revert`, `thread.conversation.revert`, `thread.session.stop`
- **Internal commands:**
  - `thread.auto-settle`, `thread.session.set`
  - `thread.message.{assistant,reasoning}.{delta,complete}`, `thread.message.user.append`, `thread.history.import`
  - `thread.proposed-plan.upsert`, `thread.turn.diff.complete`, `thread.activity.append`, `thread.revert.complete`
  - `thread.title.{generate.complete,refine,regeneration.complete}`
  - `thread.pull-request.sync`, `thread.pull-request-link.sync`
- **Events** (33 types). In the live DB the most frequent are `thread.activity-appended`, `thread.message-sent`, `thread.session-set`, `thread.turn-start-requested`, `thread.meta-updated` and `thread.turn-diff-completed`. The full list is in `OrchestrationEventType`.
  - Envelope: `{sequence, eventId, aggregateKind, aggregateId, occurredAt, commandId|null, causationEventId|null, correlationId|null, metadata, type, payload}`.
  - **Deltas are `message-sent{streaming:true}` events that the projector appends.**

### 5.4 Reactors (started by `OrchestrationReactor.start`)

| Reactor | Lines | Does |
|---|---:|---|
| `ProviderRuntimeIngestion` | 2,743 | provider events → `thread.session.set`, message delta/complete, `activity.append`, `proposed-plan.upsert`, `turn.diff.complete`, titles. Assistant-text buffering: ≥400 ms cadence, 24k-char cap, markdown-aware split points. Liveness and plan-progress registries. **The streaming cadence is user-visible.** |
| `ProviderCommandReactor` | 1,965 | domain events → `ProviderService` calls (start, send, interrupt, respond, stop, compact, model/instance switch); title and branch-name generation through `textGeneration`; worktree recreation |
| `CheckpointReactor` | 1,063 | git checkpoints at turn start and end, numstat diff, revert (restore + provider rollback + ref deletion); `RuntimeReceiptBus` (test sync points) |
| `ThreadDeletionReactor` | | stops the session and closes terminals on `thread.deleted` |
| `ThreadSettlementReactor` | 385 | periodic auto-settle (`ThreadSettlementPolicy.ts`) |
| `ThreadPullRequestReactor` | 424 | every minute, `thread.pull-request.sync` |
| `PullRequestSyncReactor` | 341 | every minute / 15 minutes, link sync |
| `AgentAwarenessRelay` | | T3 Connect publishing: stub |
| `StorageCleanup` | 481 | worktree, log and artifact retention |

### 5.5 Checkpoints (git hidden refs)

- **Refs:** `refs/t3/checkpoints/<base64url(threadId)>/turn/<n>`.
- **Capture:**
  1. Build a private index `GIT_INDEX_FILE=<commonDir>/t3-checkpoint-index-<uuid>` with `read-tree HEAD` and `add -A`. Sparse checkout is respected.
  2. `-c core.fsync=objects,reference write-tree` → `commit-tree -m "t3 checkpoint ref=<ref>"` (author "T3 Code") → `update-ref`.
  3. Repair 0-byte refs. Retry `index.lock` errors twice at 75 ms.
- **Restore:** `restore --source <oid> --worktree --staged -- .`, then `clean -fd`, then `reset`.
- **Diffs:** computed live (`CheckpointDiffQuery.getTurnDiff`, `getFullThreadDiff`).

### 5.6 Worktree bootstrap

This lives in `ws.ts`, inside the `orchestration.dispatchCommand` handler for `thread.turn.start` with `bootstrap`:
1. Create the thread, then append the user message (`deferredTurn`).
2. Optionally fetch origin and resolve the base.
3. `thread.session.set` (preparing).
4. `git -c checkout.workers=N worktree add [-b branch] <baseDir>/worktrees/<repo>/<branch-with-dashes> <base>`, with `GIT_PROGRESS_DELAY=0 LC_ALL=C`. Progress is parsed into `WorktreeSetupTracker`.
5. Submodules, best effort.
6. Setup script in a PTY (`ProjectSetupScriptRunner`).
7. Turn start.

On failure, the thread is deleted or marked `not-created` (`bootstrapThreadDisposition`) and created worktrees are removed with retries.

The same handler also does more around dispatch:
- rejects commands during a project clone;
- on `thread.archive`, stops the session (`session-stop-for-archive:`) and closes terminals;
- cleans up uploaded attachments when dispatch fails;
- records analytics.

---

## 6. Other modules

Each module below lists: purpose, external dependencies with their Rust replacement, size, and interface to the rest of the server. "RPC" lists the methods it backs (scopes in Appendix A).

### 6.1 `vcs/` (6.5k): git driver
- **Purpose:**
  - status (porcelain v2), refs (cached), worktrees, commit and push, fetch, diffs, checkpoints;
  - ignore filtering with `check-ignore -z --stdin` (chunked at 256 KB) and `ls-files`;
  - repo detection, and `.t3code/vcs.json` lenient config.
- **Processes:**
  - `VcsProcess.ts` is the single spawn point for git, gh, glab and az: semaphores of **8 overall and 4 for gh**, default timeout 30 s, output cap 1 MB (truncate or error), invalid-UTF-8 flag, stderr classification `authentication|rate-limited|not-found|command-failed`.
  - Network calls run with a non-interactive env: `GIT_TERMINAL_PROMPT=0 GCM_INTERACTIVE=never GIT_ASKPASS= SSH_ASKPASS= SSH_ASKPASS_REQUIRE=never`.
- **trace2 hook progress:** `GIT_TRACE2_EVENT=<file>`, tailed. In Rust, prefer `GIT_TRACE2_EVENT=af_unix:<sock>`.
- **`VcsStatusBroadcaster`:**
  - Refcounted remote pollers per realpath cwd, gated by `BackgroundPolicy`.
  - Interval `automaticGitFetchInterval` (30 s), backoff from 30 s to 15 min, optional auto-pull.
  - No FS watcher; local refreshes are triggered by RPCs and reactors.
- **Interface:** `GitVcsDriver` (≈35 methods), `VcsDriverRegistry`, `CheckpointStore`.
- **RPC:** `subscribeVcsStatus`, `vcs.*`; `review.*` through `ReviewService`.
- **Rust:** `tokio::process` on `git`. **Do not use git2 or gix for behaviour**: hooks, worktree progress, sparse checkout, credential helpers and trace2 need the CLI. `gix` is optional later for hot read-only paths.

### 6.2 `git/` (3.4k): stacked actions
- `GitManager.ts` (2,862 lines) covers status assembly with PR lookup, `resolvePullRequest`, `preparePullRequestThread`, and `runStackedAction`.
- `runStackedAction` is a stream:
  - actions `commit|push|create_pr|commit_push|commit_push_pr`;
  - phases `branch → commit → push → pr`;
  - events `action_started`, `phase_started`, `hook_started|output|finished`, `action_finished`, `action_failed`.
- **Dependencies:** textGeneration (commit and PR text), PR template detection (`ls-tree` + `cat-file`), the source-control providers, `ProjectSetupScriptRunner`. After creating a PR it dispatches `thread.pull-request.link` (`linkCreatedPullRequest.ts`).
- **RPC:** `git.runStackedAction`, `git.resolvePullRequest`, `git.preparePullRequestThread`.

### 6.3 `sourceControl/` (8.0k)
- Detects the forge from the remote URL (`packages/shared/src/sourceControl.ts`) and offers a common provider API: list, get, create change requests; clone URLs; create repo; default branch; checkout; resolve link.
- **Mechanisms:**
  - CLIs: `gh` (`pr …`, `repo …`, `api`, `api graphql --input -`), `glab`, `az … --only-show-errors --output json`, `fj`/`tea`.
  - **Bitbucket:** direct REST to `api.bitbucket.org/2.0` (credentials from settings or `T3CODE_BITBUCKET_*`; honours `Retry-After`).
- **Rate control:** a GitHub GraphQL budget that keeps a 10% reserve for interactive calls, plus a per-host rate-limit pause (in memory).
- **Repository service:** lookup, clone (`git clone --progress`, progress parsed from `\r`-split stderr), publish.
- **RPC:** `server.discoverSourceControl`, `sourceControl.*`, and `projectClone.*` through `ProjectCloneTracker`.
- **Rust:** keep shelling out to the CLIs, and port the GraphQL documents verbatim. Use `reqwest` for Bitbucket.

### 6.4 `pullRequest/` (19.8k, the largest feature)
- **Coverage:** a PR inbox and review UI over 5 forges: list/stats/summary/stack/detail/preview/activity, review threads, paged diffs, file contents, viewed files, actions, edit, comment, review, reply, resolve, react, reviewers, labels, refresh stream.
- **`PullRequestService.ts`** (3.2k):
  - routing, 12-way concurrency;
  - TTL caches: list 30 s, detail 15 s, diff 60 s, commit diff 10 min, viewer 10 min, stale-while-revalidate 10 min;
  - invalidation epochs and single-flight (`PullRequestReadCache`).
- **Providers:**
  - GitHub ≈ 7k, all through `gh` (≈29 GraphQL documents, stacks API, routing identity via `gh auth token` + `api user`).
  - GitLab 2.9k (`glab`), Forgejo 0.9k, Azure 2.2k (`az devops invoke`; **diffs synthesized locally** with jsdiff `structuredPatch`, so use `similar` in Rust and golden-test the hunks), Bitbucket 2.1k (REST).
- **Persistence:** `pull_request_files_viewed` for hosts without native viewed state; reads `projection_thread_pull_requests`.
- **RPC:** all `pullRequests.*`, plus HTTP `POST /api/pull-requests/diff`.
- **Port order:** freeze the `PullRequestProvider` trait (`PullRequestProvider.ts`, 663 lines) first. The forges are then independent work items.

### 6.5 `project/` (4.0k)
| Component | Behaviour |
|---|---|
| `ProjectFaviconResolver` | `t3.json iconPath` → candidate list → regex scan of `index.html` etc.; LRU 512 entries |
| `T3ProjectFileLoader` | `t3.json`, schema in `contracts/t3ProjectFile.ts` |
| `ProjectSetupScriptRunner` | runs `runOnWorktreeCreate` scripts inside a terminal PTY, with an exit sentinel |
| `WorktreeSetupTracker` | in memory, PubSub, `subscribeWorktreeSetup` |
| `ProjectCloneTracker` | in memory, PubSub, `projectClone.*` / `subscribeProjectClones` |
| `RepositoryIdentityResolver` | `rev-parse --show-toplevel` + `remote -v`; TTL 15 min / 1 min |
| `AgentSessionScanner` / `AgentSessionImporter` (1.8k) | import external Claude (`~/.claude/projects/**/*.jsonl`) and Codex (`~/.codex/sessions/…/rollout-*.jsonl`) sessions; RPC `agentSessions.scan|import`, which becomes `thread.history.import` |

### 6.6 `workspace/` (1.5k)
- **Search:** `WorkspaceSearchIndex.ts` wraps `@ff-labs/fff-node`, which is FFI to the Rust **fff** library: fuzzy file and directory search, mixed search, grep with cursor paging and a 250 ms budget, its own watcher, a 25k cap.
  - There are two indexes per workspace (paths, and content on demand), destroyed after 15 min idle.
  - **Depend on the fff crate directly** (git dependency `dmtrKovalenko/fff`, matching the version behind fff-node 0.9.4). Rebuilding on `nucleo` + `ignore` + `grep-*` would visibly change ranking.
- **Entries:** `WorkspaceEntries` handles browse (directories only), list (from the index or immediate children; excludes `.git`; marks `ignored` via `check-ignore`) and search.
- **File system:** `WorkspaceFileSystem` covers:
  - `readFile`: relative paths are realpath-contained; **absolute paths may read any file** (deliberate); 1 MB cap with a `truncated` flag.
  - `writeFile`: relative only; mkdir -p; refreshes the index.
- **RPC:** `projects.searchEntries|searchContents|listEntries|readFile|writeFile`, `filesystem.browse`.

### 6.7 `terminal/` (3.6k)
- **Sessions:** keyed by `(threadId, terminalId)`, default `term-1`.
- **Shell:** `$SHELL`, falling back to bash/sh (Windows: ComSpec/PowerShell/cmd). Default size 120×30.
- **Environment:** the server env minus `T3CODE_*`, `VITE_*`, `PORT` and the Electron variables, plus the per-instance provider homes, `TERM=xterm-256color` and `COLORTERM=truecolor`.
- **Scrollback:** 5,000 lines / 8 MiB, in 16 KiB chunks.
  - **Escape sanitizer:** query/response sequences are stripped before storage, so replaying history does not trigger fresh replies: CSI `n R c`, DECRQM, XTVERSION, kitty `?u`, DCS `$q +q`, OSC 10/11/12.
  - History is persisted to `logs/terminals/…` (40 ms debounce) and its tail is restored on open. PTYs do not survive a restart.
- **Child detection:** every 1 s, with backoff up to 60 s, via the resource monitor's process table or `ps -eo pid=,ppid=,comm=`. Detected pids feed `PortDiscovery`.
- **Flow control:** windowed acks (§1.3 rule 5).
- **RPC:** `terminal.*`, `subscribeTerminalEvents|Metadata`. Also used by `ProjectSetupScriptRunner` and `closeIdle` on settle.
- **Rust:** `portable-pty`, plus `vte` (or a hand state machine) for the sanitizer.

### 6.8 `textGeneration/` (3.1k)
- **Operations:** commit message, PR content, branch name, thread title (with link resolution).
- **Routing:** the model selection's `instanceId` picks the instance, whose own text generator is used. Each one is a one-shot agent CLI call with the prompt on stdin:
  - Claude: `claude -p --output-format json --json-schema <s> --model … --tools "" --disable-slash-commands --strict-mcp-config --permission-mode dontAsk`.
  - Codex: `codex exec --ephemeral --skip-git-repo-check -s read-only --model M --config model_reasoning_effort="low" --output-schema S --output-last-message O [--image P] -`.
  - OpenCode: SDK session.
  - Cursor, Grok, Antigravity: ACP, with `clientInfo` `t3-code-git-text`.
- **Dependency:** the provider layer (binaries, homes, env). Port it with the providers.

### 6.9 `mcp/` (3.1k): MCP server for the agents
- **Transport:** Streamable HTTP at `POST /mcp`, protocol `2025-06-18`, server name "T3 Code" (brand-swapped to "zenith code").
  - The effect patch adds `DELETE /mcp`: 400 without an `mcp-session-id`, 404 if unknown, 204 otherwise.
  - An empty 200 response is rewritten to 202.
- **Auth:** per provider session, `Authorization: Bearer <32 random bytes b64url>`.
  - Only the SHA-256 is kept, in memory. Expiry is 24 h after the last touch.
  - Capabilities per session: `pull-requests`, `preview`, `device`.
  - Failure: 401 `{error:"invalid_mcp_credential"}` with `WWW-Authenticate: Bearer`.
- **Tools:**
  - `preview_status|open|navigate|resize|set_appearance|snapshot|click|type|press|scroll|evaluate|wait_for|recording_start|recording_stop`
  - `device_list|open|screenshot|close`
  - `link_pull_request`, `unlink_pull_request`, `list_thread_pull_requests`
- **`PreviewAutomationBroker`** (660 lines) forwards preview tool calls to the browser client over the `previewAutomation.connect` stream and gets answers through `previewAutomation.respond`.
- **Rust:** `rmcp` (official SDK, streamable-HTTP server) mounted in axum. Verify the session-header behaviour against Claude and Codex.

### 6.10 `preview/` (1.1k)
- **Manager:** in-memory tabs `(threadId, tabId)` with revision and epoch.
- **`PortScanner`:**
  - Polls `lsof -iTCP -sTCP:LISTEN -P -n -F pcn` every 3 s while subscribed (Windows: `Get-NetTCPConnection`), falling back to common dev ports.
  - HTTP-probes candidates, merges terminal pids.
- **RPC:** `preview.*`, `subscribePreviewEvents`, `subscribeDiscoveredLocalServers`.
- **Rust:** the `listeners` or `netstat2` crate.

### 6.11 `assets/` (1.7k)
- **Signed capability URLs:** HMAC over a base64url JSON payload, key `secrets/asset-access-signing-key.bin`, TTL 1 h. Favicon tokens are bucketed per 30 min.
  - Claim kinds: `workspace-file(-exact)`, `media-file-exact` (device and inode checked), `attachment`, `project-favicon(-external)`, `native-app-icon`, `github-media`.
- **Uploads:** signed, valid 10 min; files go to `attachments/` as pending, swept every 15 min.
- **GitHub media:** proxied with a `gh auth token`.
- **macOS app icons:** `plutil`, `mdfind`, `mdls`, `sips`. Rust: the `plist` and `icns` crates.
- **RPC:** `assets.createUrl`, `attachments.createUploadUrl|delete`.

### 6.12 `usage/` (4.0k)
- **Purpose:** token and cost dashboards built from the provider CLIs' local transcripts:
  - Claude `projects/**.jsonl`, Codex `sessions/`, Grok `~/.grok/sessions`;
  - OpenCode SQLite (read-only), Antigravity SQLite;
  - Cursor through `cursor.com/api/dashboard/get-filtered-usage-events` with a keychain token.
- **Pricing:** the LiteLLM JSON (24 h cache) plus overrides from settings.
- **Cache:** scan resume positions in `usage-scan-cache.json`.
- **`UsageLimitSources`:** polls CLIProxyAPI hubs from settings.
- **Performance:** a cold 30-day scan covers about 1.4 GB in 2–3 s in TS, so use a streaming line parser in Rust.
- **RPC:** `server.getUsageSummary`, `server.refreshUsageRates`, and the `usageLimitSourcesUpdated` config event.

### 6.13 `device/` (4.2k): low priority
- iOS Simulator and Android Emulator control through pinned npm tools (`expo-device-hub@0.12.0`, `agent-device@0.21.12`) installed into `tools/`.
- An HTTP and WS proxy `/api/device-hub/*` with an allowlist, authenticated like `/ws`.
- SSH device hosts (`ssh -N -L …`, shelling out to `ssh`).
- **Phase 1 can return an empty `DeviceServiceState`** (`hubBasePath:"/api/device-hub"`). Check the `DeviceHostStatus` literals in `contracts/device.ts` before choosing the status value.

### 6.14 Cloud / relay / desktop update / service launcher: stubs
- **Why it is inert:** `hasCloudPublicConfig` is false in zenith (no `T3CODE_RELAY_URL` or Clerk keys).
- **Minimum to serve:**
  - `GET /api/connect/link-state` → `{"linked":false,"cloudUserId":null,"relayUrl":null,"relayIssuer":null,"managedTunnelActive":false,"publishAgentActivity":false}`
  - `cloud.getRelayClientStatus` → `{"status":"missing","version":"2026.5.2"}`
  - `cloud.installRelayClient` → a failed stream
  - descriptor capability `agentActivityPublishing:false`
- **Self-update:** `server.updateServer*` and `server.commitDesktopUpdate` answer through `ServerSelfUpdate` with `available=false` semantics. Check the `serverSelfUpdate` capability literal in `ExecutionEnvironmentDescriptor.capabilities`, and advertise the value that hides the UI.
- **Not ported:** `serviceLauncher.ts`, `cloud/selfUpdate.ts`, `pinnedRuntime.ts`, `bootService.ts`, `AgentAwarenessRelay`, `CliTokenManager`, `cloudflared`.

### 6.15 `environment/`, tailscale, ssh
- **`ServerEnvironment`:**
  - `environmentId` UUID in `userdata/environment-id` (temp file + `link()`);
  - label from `scutil --get ComputerName` or `hostnamectl`;
  - machine kind from `ioreg` or DMI;
  - the descriptor (≈30 capability flags: **advertise only what is implemented**, since flags gate UI features).
- **`RemoteOpenTargets`:** tailscale MagicDNS and mDNS hosts, only if sshd listens on loopback:22.
- **Tailscale:** `tailscale status --json` and `tailscale serve --bg --https=<p> http://127.0.0.1:<port>`, only with `--tailscale-serve`.
- **SSH remote environments** are client-side or Electron-only, and dead here. The server only uses `@t3tools/ssh/command` for device hosts. Skip `__ssh-helper`.

### 6.16 Settings, keybindings, themes
- **`serverSettings.ts`** (1.1k):
  - `settings.json` written **sparse** (defaults stripped) with an atomic write;
  - schema `ServerSettings` (`contracts/settings.ts:1103`), patch `ServerSettingsPatch` (`:1464`), defaults `DEFAULT_SERVER_SETTINGS` (`:1313`);
  - **secret fields** (provider env vars marked sensitive, usage-hub keys, Bitbucket tokens) live in `secrets/`. They appear as a redaction marker on disk and on the wire; sending the marker back means "keep the current value";
  - load-time folding migrations (legacy `enabled`, per-project fields → `projectSettingsOverrides`), which read SQLite;
  - a directory watch with 100 ms debounce feeds `settingsUpdated`. An invalid file falls back to defaults and is left untouched.
- **`keybindings.ts`** (685 lines):
  - `keybindings.json`; defaults are the 56 rules in `packages/shared/src/keybindings.ts`, back-filled without conflicts;
  - limits: 256 rules, key ≤ 64 chars, `when` ≤ 256 chars, nesting ≤ 64;
  - the `when`-expression parser is ported from shared;
  - watched; changes emit `keybindingsUpdated {keybindings, issues}`.
- **`environmentTheme.ts`:** watches `themes/`.
- **RPC:** `server.getSettings|updateSettings|getConfig|upsertKeybinding|removeKeybinding`, `subscribeServerConfig`.

### 6.17 CLI (`bin.ts`, `cli/` 5.7k): the external contract
The parser is Effect CLI. **Flags may come after positionals**: the dashboard appends `--base-dir X` last. Errors go to stderr, exit 1.

| Command | Contract |
|---|---|
| `serve [cwd] --host --port --base-dir [--mode web] [--dev-url] [--no-browser] [--log-ws-events] [--tailscale-serve[-port]] [--bootstrap-fd] [--auto-bootstrap-project-from-cwd]` | Env fallbacks `T3CODE_HOST|PORT|HOME|MODE|NO_BROWSER|…`. When ready, prints `T3 Code server is ready.` / `Connection string:` / `Token:` / `Pairing URL: …/pair#token=…` / QR code (the dashboard redacts the token and drops QR lines). |
| `auth pairing create [--ttl 2m] [--admin] [--label L] [--base-url U] [--json]` | stdout: `JSON.stringify({id, credential, label?, scopes, expiresAt(ISO), pairUrl?}, null, 2)`. The dashboard reads `.credential`. |
| `auth session issue [--ttl 12h] [--label] [--subject] [--token-only] [--json]` | `{sessionId, token, method:"bearer-access-token", scopes, subject, client:{label,deviceType:"bot"}, expiresAt}`. The dashboard parses from the first `{`. |
| `auth pairing list [--json]`, `auth pairing revoke <id>`, `auth session list [--json]`, `auth session revoke <id>` | list JSON: `[{id,label?,scopes,createdAt,expiresAt}]` and `[{sessionId,method,scopes,subject,client,connected,issuedAt,expiresAt,lastConnectedAt\|null}]`. Revoke prints `Revoked pairing credential <id>.` / `No active pairing credential found for <id>.` (sessions: `Revoked session …` / `No active session found …`). Non-JSON `pairing create` prints `Issued client pairing token <id>.` / `Token: …` / `[Pair URL: …]` / `Expires at: …`. |
| `project add <path> --title T` | `Added project <id> (<title>) at <root>.`. Failure: `An active project already exists for '<root>'.` (the dashboard matches `/already exists/i`). Goes through the running server's HTTP API (a temporary admin session, `GET /api/orchestration/snapshot`, `POST /api/orchestration/dispatch`) if `server-runtime.json` points at a live server; otherwise opens SQLite and the engine offline. |
| `project remove|rename`, `pair`, `theme set|clear|show`, `trace summary` | secondary |
| `start`, `__claude-history`, `__service-*`, `__ssh-helper`, `connect` | not needed |

Durations accept `^\d+(ms|s|m|h|d|w)$` or Effect strings like `15 minutes`.

### 6.18 Startup and lifecycle (`serverRuntimeStartup.ts`, 1.1k)
Startup steps, in order:
1. `fixPath`: run the login shell `-ilc` with markers and a 5 s timeout to recover `PATH` (macOS fallback: `launchctl getenv PATH`).
2. DB and migrations.
3. HTTP listener.
4. Keybindings start, then settings start.
5. Reactors and session reaper.
6. Reconcile provider sessions that did not survive the restart: mark them `error`, or continue them if `continueThreadsAfterServerUpdate` is set.
7. Reconcile stuck worktree setups.
8. Auto-pull projects.
9. Welcome lifecycle event, then the startup pairing banner.
10. Write `server-runtime.json`.
11. Open the command gate (HTTP requests waited on it until now).
12. `ready` lifecycle event.

Shutdown: delete `server-runtime.json`, disable tailscale serve, graceful HTTP shutdown. The dashboard sends SIGTERM, then SIGKILL after 5 s.

### 6.19 Observability, telemetry, diagnostics, resource telemetry, background
- **Logs:** pretty logs on stdout and stderr; the dashboard captures them into `.data/code.log`.
- **Traces:** NDJSON spans in `logs/server.trace.ndjson`, 10 MiB × 10. The shape is `{type:"effect-span",name,traceId,spanId,…}`; `trace summary` and `server.getTraceDiagnostics` parse it, so keep it.
- **Optional OTLP:** `T3CODE_OTLP_*`, `OTEL_*`.
- **Not ported:** PostHog (no-op), the event-loop monitor, heap snapshots.
- **`resourceTelemetry`:** spawns `t3-resource-monitor` (NDJSON v3: `configure`, `setExternalProcesses`, `readHistory`, `processTable`). **In Rust, link `native/resource-monitor` as a library.**
- **`HostResources`:** CPU and memory, cached 5 s.
- **`ProcessDiagnostics.signal`:** only reaches server descendants, checked by `(pid, startTimeMs)` identity.
- **`BackgroundPolicy`:** per-client activity leases (≤ 16 each) plus host power. It gates pollers through `shouldRunScopeWork` for the scopes `server-config|provider-status|vcs-status|git-refs|diagnostics|thread`.
- **RPC:** `server.getTraceDiagnostics|getProcessDiagnostics|getHostResources|getProcessResourceHistory|getResourceTelemetryHistory|retryResourceTelemetry|signalProcess|reportClientActivity|reportHostPowerState|getBackgroundPolicy`, `subscribeResourceTelemetry`, `subscribeBackgroundPolicy`.

### 6.20 `process/externalLauncher.ts` (802) and `processRunner.ts` (424)
- **`processRunner.ts`:** `run({command,args,cwd,timeout,env,stdin,maxOutputBytes=8MiB,outputMode})` → `{stdout,stderr,code,timedOut,truncated,invalidUtf8}`. This is the base of every subprocess.
- **`externalLauncher.ts`:** browser open (`open`, `xdg-open`), ≈25 editors from `contracts/editor.ts` (`goto`/`line-column`/`direct-path` styles), PATH probing.
- **RPC:** `shell.openInEditor`, and `availableEditors` in `server.getConfig`.
- **Rust:** `which`, `open`.

---

## 7. Rust architecture

### 7.1 Crate layout

All of it lives under `crates/zenith-code/`. The root `Cargo.toml` adds `"crates/zenith-code"` and `"crates/zenith-code/crates/*"` to `members`. Splitting into sub-crates gives engineers clear ownership, parallel compilation, and enforced dependency direction.

```
crates/zenith-code/                    package `zenith-code`: lib (server assembly) + bin `zenith-code` (CLI)
  src/main.rs, src/cli/…               clap CLI (§6.17), output contracts
  src/server/…                         axum app: route table, startup sequence, readiness gate, shutdown
  src/rpc_handlers/…                   the port of ws.ts: one module per method group (orchestration, server, vcs,
                                       pullRequests, terminal, preview, device, provider, auth access, …)
  crates/
    zc-contracts/      generated serde types for packages/contracts + RPC method table + scope table (WP-01)
    zc-rpc/            Effect-RPC-over-WebSocket server runtime: envelopes, Exit/Cause encoding, Ack backpressure,
                       windowed acks, ping/pong, interrupt, per-method scope check, stream plumbing (WP-02)
    zc-http/           shared HTTP bits: typed JSON error responses, CORS, compression, static SPA, CSP, cookies (WP-03)
    zc-core/           config + paths (deriveServerPaths), process runner, login-shell PATH fix, secret store,
                       atomic write, caches, pubsub helpers, clock/ids, lenient JSON (WP-04)
    zc-db/             rusqlite actor, pragmas, migration protocol + consolidated DDL, all repositories (WP-05)
    zc-auth/           sessions, pairing, tickets, cookies, DPoP, RPC scope map, auth HTTP handlers (WP-06)
    zc-settings/       server settings (sparse, redaction, secrets, folding), keybindings (+ `when` parser), themes (WP-07)
    zc-orchestration/  decider, projector, engine, projection pipeline, snapshot queries, subscriptions, reactors,
                       checkpoints, attachments, storage cleanup, background policy (WP-08..WP-11)
    zc-providers/      provider core (service, registries, session directory, reaper, logger, manifest, auth flows,
                       installers, usage limits) (WP-12)
    zc-provider-claude/   (WP-13)   zc-provider-codex/ (WP-14)   zc-acp/ + zc-provider-acp/ (WP-15)
    zc-provider-opencode/ (WP-16)   zc-textgen/ (WP-17)
    zc-vcs/            git driver, VcsProcess semantics, status broadcaster, git workflow/stacked actions, review (WP-18, WP-19)
    zc-sourcecontrol/  forge CLIs + Bitbucket REST + discovery + repository service (WP-20)
    zc-pullrequest/    service + caches + provider trait; forges as modules (WP-21..WP-23)
    zc-workspace/      fff-based search, entries, file system, browse (WP-24)
    zc-project/        favicon, t3.json, clone tracker, worktree setup tracker, setup scripts, agent-session import (WP-25)
    zc-terminal/       PTY manager, sanitizer, history, flow control (WP-26)
    zc-mcp/            /mcp server, session registry, toolkits, preview automation broker (WP-27)
    zc-preview/        preview manager, port discovery (WP-28)
    zc-assets/         signed asset URLs, uploads, media ranges, GitHub media, app icons (WP-29)
    zc-usage/          transcript scanners, pricing, limit sources (WP-30)
    zc-telemetry/      resource telemetry (links native/resource-monitor), host resources, diagnostics,
                       trace NDJSON writer + reader, OTLP (WP-31)
    zc-device/         device service + hub proxy (WP-32, later)
    zc-environment/    descriptor, environment id, remote open targets, tailscale, cloud stubs (WP-33)
```

Notes on the layout:
- `code/native/resource-monitor` either becomes a workspace member, or `zc-telemetry` path-depends on a library target extracted from it.
- `zc-contracts` is consumed by everything, so keep it to types only. Each crate owns its service trait; `zenith-code` wires concrete implementations together, the way `server.ts` composes layers.

### 7.2 WP-01 in detail: generating the contracts

Do not hand-write about 1,500 schemas, and do not trust JSON Schema alone. We ran `Schema.toJsonSchemaDocument` over every payload, success and error schema of the 148 RPCs: all 444 convert, but the result is lossy.
- Only 99 named definitions come out; the rest is inlined anonymously.
- Some fields degrade to `{}` (for example `ModelSelection.model`).
- Transformations such as `TrimmedString`, `ForwardCompatible*` and NaN-as-string numbers are not represented.

So `typify` on that output is a starting point at best.

**Build a TS generator that walks Effect `SchemaAST` and emits Rust.** It lives at `code/scripts/gen-rust-contracts.ts` and runs with Node against the installed `effect` (the main checkout has `node_modules`):

1. **Naming.** Iterate every export of `packages/contracts/src/index.ts`. Map schema identity to export name, so that shared sub-schemas get stable names (`ThreadId`, `OrchestrationThreadShell`, …). Fall back to `identifier`/`title` annotations, then to parent-field names for anonymous structs.
2. **Mapping.** Map each AST node with the table in §1.5:
   - `Objects` → struct, with `Option` plus the right `skip`/`default` per property signature (`isOptional`, `NullOr` vs `UndefinedOr`);
   - `Union` of literals → enum;
   - `Union` of structs sharing a literal key → internally-tagged enum (detect `type`, `kind` or `_tag`);
   - `Declaration` (DateTimeUtc, Option, Uint8Array, Defect) → the hand-written types in `zc-contracts::prim`;
   - `Suspend` → `Box`;
   - refinements: keep the base type, and emit `validate()` for `NonNegativeInt`, `PortSchema`, max lengths…;
   - **encoded side only**: use the encoded AST, because that is what crosses the wire.
3. **RPC table.** Emit one `enum RpcMethod` plus `pub const METHODS: &[MethodSpec{tag, stream, scope}]` from `WsRpcGroup.requests` and `RPC_REQUIRED_SCOPES`. Also emit per-method associated types (`type Payload; type Success; type Error;`) through a trait, so handlers are type-checked.
4. **Same for `EnvironmentHttpApi`.** Emit endpoints with their status codes.
5. **Fixtures.** Write `fixtures/<schema>/<n>.json` from curated samples and from recordings (§9), for round-trip tests.

Commit the generated code (like `_generated` today) and check in CI that regenerating it is a no-op. Run the generator at every upstream sync (`npm run code:sync`).

### 7.3 Dependency graph

```
zc-contracts ─┬─> zc-rpc ──────────────┐
              ├─> zc-http ─────────────┤
              └─> zc-core ─┬─> zc-db ──┼─> zc-auth ─────────────────────────────┐
                           │           ├─> zc-settings ─────────────────────────┤
                           │           └─> zc-orchestration(core: decider/projector/engine/projections/queries)
                           ├─> zc-vcs ─┬─> zc-sourcecontrol ─> zc-pullrequest    │
                           │           └─> (checkpoints) ─> zc-orchestration     │
                           ├─> zc-terminal ─> zc-project(setup scripts)          │
                           ├─> zc-workspace                                      │
                           ├─> zc-providers ─> zc-provider-{claude,codex,acp,opencode} ─> zc-textgen
                           │        └─ needs: zc-mcp (session registry: token + endpoint)
                           ├─> zc-telemetry, zc-assets, zc-preview, zc-usage, zc-environment, zc-device
                           └───────────────────────────────────────────────> zenith-code (wiring, rpc_handlers, CLI)
reactors (in zc-orchestration::reactors) depend on traits: ProviderService, TerminalManager, GitWorkflow,
TextGeneration, PullRequestService, ServerSettings → define these traits in zc-core (or a small zc-ports crate)
in week 1 so both sides can proceed.
```

The cycles in the TS layer graph (orchestration ↔ providers ↔ git ↔ textGeneration) are broken by **traits defined early**. Concretely: `ProviderService`, `OrchestrationDispatch`, `ProjectionReads`, `TerminalManager`, `GitWorkflow`, `TextGeneration`, `PullRequests`, `SettingsService`, `BackgroundPolicy`. They all go in one `zc-ports` module, owned by the tech lead and frozen at the end of Phase 0.

### 7.4 Work packages, order, staffing

The sizes below are TS source lines to port. "Gate" means the WP must reach that state before dependents start real (non-stub) integration.

**Phase 0: foundation (weeks 0–3, 5–6 engineers).** Everything else depends on these.

| WP | Content | TS sources | Size | Deps | Gate |
|---|---|---|---:|---|---|
| WP-01 | Contracts codegen + `zc-contracts` + fixtures | `packages/contracts/**`, `auth/RpcAuthorization.ts` | 21k (generated) | — | all 148 RPCs + 24 endpoints round-trip against the TS oracle |
| WP-02 | `zc-rpc`: WS protocol runtime | `effect/unstable/rpc/*` semantics, `terminal/OutputProtocol.ts`, `ws.ts:3803-3918` | — | WP-01 | `server.test.ts` WS transport tests pass against a dummy handler set |
| WP-03 | `zc-http` + axum skeleton: static SPA, CSP, `/zenith/embed.json`, CORS, compression, readiness gate, typed errors | `http.ts`, `zenith/embed.ts`, `httpCors.ts`, `server.ts` | 1.5k | WP-01 | dashboard iframe loads the SPA from the Rust server |
| WP-04 | `zc-core` + `zc-ports` traits; config, paths, process runner, PATH fix, secret store, atomic write | `config.ts`, `os-jank.ts`, `processRunner.ts`, `vcs/VcsProcess.ts`, `auth/ServerSecretStore.ts`, `atomicWrite.ts`, shared `hostProcess`/`shell` | 2.5k | — | traits frozen |
| WP-05 | `zc-db`: actor, pragmas, migration protocol, DDL, all repositories | `persistence/**`, `shared/nodeSqliteClient.ts` | 7.6k | WP-04 | opens the live DB copy; `sqlite_master` diff clean; repository tests ported |
| WP-34 | Verification harness: validating recording proxy, Node oracle, black-box runner (§9) | new | — | WP-01 | proxy runs between apps/web and the TS server |

**Phase 1: vertical slice, "chat works in the dashboard" (weeks 3–10, 10–12 engineers).**

| WP | Content | TS sources | Size | Deps |
|---|---|---|---:|---|
| WP-06 | `zc-auth` + auth HTTP + CLI `auth …` (+ `--admin`) | `auth/**`, `cli/auth.ts`, `cliAuthFormat.ts` | 4.5k | WP-05 |
| WP-07 | `zc-settings`: settings, keybindings, themes, `server.getConfig`, `subscribeServerConfig` (without provider statuses at first) | `serverSettings.ts`, `keybindings.ts`, `environmentTheme.ts`, shared `keybindings` | 2.5k | WP-05 |
| WP-08 | Orchestration core: decider, projector, engine, receipts, normalizer, attachments | `decider.ts`, `projector.ts`, `Layers/OrchestrationEngine.ts`, `Normalizer.ts`, `attachmentStore.ts` | 5k | WP-05 |
| WP-09 | Projections + queries + subscriptions + orchestration HTTP: `subscribeShell/Thread`, snapshots, search | `Layers/ProjectionPipeline.ts`, `Layers/ProjectionSnapshotQuery.ts`, `ActivityPayloadProjection.ts`, `ws.ts:836-1060,2013-2400`, `orchestration/http.ts` | 8k | WP-05, WP-08 |
| WP-10 | Reactors: runtime ingestion + provider command reactor + deletion/settlement | `Layers/ProviderRuntimeIngestion.ts`, `Layers/ProviderCommandReactor.ts`, `Thread*Reactor.ts` | 5.5k | WP-08, WP-12 traits |
| WP-11 | Checkpoints + checkpoint reactor + worktree bootstrap from `ws.ts` + storage cleanup + background policy + review | `checkpointing/**`, `CheckpointReactor.ts`, `ws.ts:1054-1640`, `storageCleanup.ts`, `background/**`, `review/**` | 4k | WP-18 (git driver) |
| WP-12 | Provider core: service, instance/adapter/provider registries, session directory, reaper, event logger, manifest, status cache, compatibility | `provider/Layers/Provider*.ts`, `EventNdjsonLogger.ts`, `ModelManifest.ts`, `providerSnapshot.ts`, … | 7k | WP-05, WP-07 |
| WP-13 | Claude driver (control protocol, adapter mapping, skills, history, probe, usage limits) | `ClaudeAdapter.ts`, `ClaudeProvider.ts`, `Drivers/Claude*`, `claudeHistoryWorker.ts`, `claude*Limits/Credits` | 8k | WP-12 |
| WP-14 | Codex driver (app-server peer, typify types, adapter, collab, home layout, managed mode) | `Codex*`, `effect-codex-app-server` client | 10k | WP-12 |
| WP-26 | Terminal | `terminal/**` | 3.6k | WP-02, WP-04 |
| WP-18 | VCS core: git driver, refs, status, worktrees, checkpoints primitives, status broadcaster, `vcs.*` | `vcs/**`, `git/GitWorkflowService.ts` | 7k | WP-04 |
| WP-24 | Workspace search and files | `workspace/**` | 1.5k | WP-04 |
| WP-27a | MCP session registry + endpoint (needed by providers), toolkits later | `mcp/McpSessionRegistry.ts`, `McpProviderSession.ts`, `McpHttpServer.ts` | 1k | WP-03 |
| WP-33 | Environment descriptor, environment id, lifecycle events, startup sequence, `server-runtime.json`, cloud and device stubs, `serve` CLI + banner, `project add` | `environment/**`, `serverRuntimeStartup.ts`, `serverRuntimeState.ts`, `serverLifecycleEvents.ts`, `startupAccess.ts`, `cli/project.ts`, `cloud/**` (stubs) | 3k | WP-03, WP-06, WP-08 |

**Milestone M1, end of Phase 1.** The dashboard (TS or Rust) spawns `bin.mjs serve`, which is the shim to the Rust binary. On a copy of a real `~/.zenith/code`:
- the iframe pairs and the sidebar fills;
- old threads open with the same sequences;
- a Claude turn and a Codex turn stream, approvals work, interrupt works;
- the terminal works and file search works;
- reopening the DB with the TS server works.

**Phase 2: breadth (weeks 8–18, 12–16 engineers).**

| WP | Content | Size | Deps |
|---|---|---:|---|
| WP-15 | ACP runtime + Cursor, Grok, Antigravity drivers (+ Antigravity installer/auth) | 14k | WP-12 |
| WP-16 | OpenCode driver | 6.3k | WP-12 |
| WP-17 | Text generation (all providers) | 3.1k | WP-13..16 |
| WP-12b | Provider auth flows (Codex ChatGPT OAuth, Antigravity), installers, maintenance/self-update, usage limits and reset credits, `provider.*` RPCs | 5k | WP-12 |
| WP-19 | Git stacked actions, PR resolve/prepare, `GitManager` | 3.4k | WP-18, WP-17, WP-20 |
| WP-20 | Source control (gh/glab/az/fj/tea, Bitbucket REST, discovery, repo service, clone, publish) | 8k | WP-18 |
| WP-21 | PR core + GitHub provider + `/api/pull-requests/diff` + viewed files | 11k | WP-20 |
| WP-22 | PR GitLab + Forgejo | 3.7k | WP-21 trait frozen |
| WP-23 | PR Azure DevOps + Bitbucket (`similar` diff synthesis) | 3.5k | WP-21 trait frozen |
| WP-25 | Project: favicon, t3.json, clone tracker, worktree setup tracker, setup script runner, agent-session scan/import | 4k | WP-26, WP-20, WP-08 |
| WP-27b | MCP toolkits + preview automation broker | 2k | WP-27a, WP-28, WP-21 |
| WP-28 | Preview manager + port discovery | 1.1k | WP-26 |
| WP-29 | Assets + uploads + media ranges | 1.7k | WP-03, WP-04 |
| WP-30 | Usage | 4k | WP-07 |
| WP-31 | Resource telemetry (link the resource monitor), diagnostics, trace writer/reader + `trace summary`, OTLP | 5k | WP-04 |
| WP-32 | Device (real implementation) | 4.2k | WP-26, WP-29; optional |
| WP-06b | DPoP, `/oauth/token` DPoP mode, remote-reachable cookie variants, tailscale serve | 1k | WP-06 |

**Phase 3: hardening and cut-over (weeks 16–22).** Run the black-box suite for both backends in CI, run Playwright smoke flows, fix the divergences, have the owner dogfood behind the shim, then flip `CODE_BIN` in both dashboards (§10).

**Staffing sketch for 14 engineers.**

| Engineer | Phase 0 | Then |
|---|---|---|
| E1 (tech lead) | WP-01 + `zc-ports` | rpc_handlers integration, reviews |
| E2 | WP-02 | WP-26, WP-28 |
| E3 | WP-03 | WP-33, then WP-29 |
| E4 | WP-04 | WP-24, then WP-31 |
| E5 | WP-05 | WP-09 |
| E6 | WP-34 | owns verification throughout |
| E7 | — | WP-06, then WP-06b, WP-30 |
| E8 | — | WP-08, then WP-10 |
| E9 | — | WP-07, then WP-11 |
| E10 | — | WP-12, then WP-12b |
| E11 | — | WP-13, then WP-17 |
| E12 | — | WP-14, then WP-15 |
| E13 | — | WP-18, then WP-19, WP-25 |
| E14 | — | WP-20, then WP-21 |

With more people, split WP-21/22/23 across 3 engineers, and give WP-15 and WP-16 their own owners. Below 10 engineers, merge WP-28/29/31 into one owner and postpone WP-32 and WP-06b.

---

## 8. Recommended crates and the hard parts

### 8.1 Crates

| Concern | Crate | Why / notes |
|---|---|---|
| Runtime | `tokio` (full), `tokio-util` (`CancellationToken`), `futures`, `tokio-stream`, `async-stream` | |
| HTTP + WS | `axum` 0.8 (`ws` feature, built on `tokio-tungstenite`), `tower-http` (cors, compression-full, fs), `axum-extra` (cookies) | The workspace already uses axum 0.8 in `zenith-server`. **permessage-deflate:** tungstenite has no stable deflate. Either accept uncompressed frames (clients don't require it; costs bandwidth on large snapshots) or use `fastwebsockets`/a deflate-capable fork. Measure snapshot sizes first. |
| JSON | `serde`, `serde_json` (`preserve_order`, `arbitrary_precision` off), `serde_with`, `serde_path_to_error` | |
| SQLite | **`rusqlite`** (`bundled`, ≥ 3.35 for DROP COLUMN, JSON1, RETURNING) on a dedicated writer thread fed by `mpsc`, plus a read-only WAL connection pool for snapshot queries | Matches the TS single-connection semantics, gives explicit SAVEPOINTs and dynamic SQL. **Not sqlx**: its migrator, async pool contention and compile-time checks don't fit. |
| Time | `jiff` (already in the workspace) or `chrono`, with a fixed `%Y-%m-%dT%H:%M:%S%.3fZ` formatter | |
| Crypto | `hmac`, `sha2`, `subtle`, `rand` (OsRng), `base64` (URL_SAFE_NO_PAD), `p256` (DPoP), `ed25519-dalek` + `pkcs8` (cloud link key, if ever), `jsonwebtoken` or `josekit` (OpenAI/Google id_token JWKS) | |
| Subprocesses | `tokio::process`, `which`, `shell-words` (launchArgs tokenizing) | Keep spawning `git`, `gh`, `glab`, `az`, `fj`/`tea`, `ssh`, `tailscale`, `lsof`, `ps`. |
| PTY | `portable-pty` (+ `vte` for the sanitizer) | |
| File watching | `notify` + `notify-debouncer-full` | settings, keybindings, themes, trace2 fallback |
| Caches | `moka` (future cache, TTL, `get_with` single-flight) | PR caches, repo identity, favicons, refs |
| Search | the **fff** crate (git dependency); fallback `ignore` + `nucleo` + `grep-searcher`/`grep-regex` | |
| Diff | `similar` | Azure diff synthesis; review previews only if git's output is not used |
| ACP | `agent-client-protocol` (+ `-schema`), unstable features | |
| Codex types | `typify` (build script or committed output) over the pinned schema | |
| MCP | `rmcp` (streamable HTTP server) | |
| OpenCode | `reqwest` + `eventsource-stream` (or `progenitor` from its OpenAPI) | |
| HTTP client | `reqwest` (rustls, json, gzip, stream) | Bitbucket, manifests, LiteLLM, installers, usage APIs |
| Archives | `flate2` + `tar`, `zip`, `sha2` | Codex/Antigravity installers |
| Keychain | `keyring` (service `cursor-access-token`, account `cursor-user`) | Cursor only |
| Locks | `fs4` / `fd-lock` | ChatGPT session lock |
| Process stats | link `native/resource-monitor` (`sysinfo` 0.39) | |
| Ports | `listeners` or `netstat2` | port discovery without `lsof` |
| CLI | `clap` 4 (derive, `env`) + `humantime` | trailing flags after positionals are fine in clap |
| Logging/tracing | `tracing`, `tracing-subscriber` + a custom NDJSON span layer, `opentelemetry-otlp` (optional) | |
| macOS bits | `plist`, `icns` | native app icons |
| QR (optional) | `qrcode` | serve banner; the dashboard drops it anyway |

### 8.2 Hard parts and risks (ranked)

1. **Encoding fidelity** (§1.5): absent vs null, `_tag`/`type`/`kind` discriminators, NaN strings, ISO milliseconds, trimmed strings, forward-compatible unions. *Mitigation:* WP-01 codegen, the Node oracle in CI, and the validating proxy during all development.
2. **Exact event-store and projection semantics**: the dispatch transaction, receipts, `snapshotSequence` = min of the cursors, subscribe-before-snapshot, coalescing windows (50 ms / 512), replay budgets (1,000 events / 8 MiB), and **sequence continuity on the existing DB**. *Mitigation:* replay the whole live event log through the Rust projector into an empty DB, then diff the projection tables against the live ones; run snapshot byte-diffs against the TS server on the same DB.
3. **Claude without the SDK**: argv, the control protocol in both directions, steering, interrupt-then-settle, about 60 system subtypes, `AskUserQuestion` answer keys, sub-agent and thinking dedupe (`ClaudeAdapter.ts` is 5.6k lines of edge cases). *Mitigation:* pin the CLI range from the manifest (≥ 2.1.280), and record golden NDJSON transcripts from the TS server. The `NTIVE:` lines in `logs/provider/events.*.log` are the inputs, and the `CANON:` lines are the expected outputs of the adapter.
4. **User-visible cadence in `ProviderRuntimeIngestion`**: 400 ms flushes and markdown-aware splitting. Golden tests on recorded streams with a virtual clock (`tokio::time::pause`).
5. **The size of `pullRequest/` + `sourceControl/`** (28k lines, five forges, GraphQL documents, TTL and staleness semantics). *Mitigation:* freeze the trait early, port forges in parallel, use recorded `gh`/`glab` fixtures (the TS tests already fake the CLIs).
6. **Concurrent CLI + server** on the same SQLite and `secrets/`: `busy_timeout`, `O_EXCL` races, atomic `UPDATE … RETURNING`.
7. **Logic hidden in `ws.ts`**: the bootstrap, post-RPC refresh hooks, `withPullRequestViewer`, the PR invalidate→sync chain, archive side effects. Treat `ws.ts` as a spec to port line by line (WP-09/11 plus `rpc_handlers`).
8. **WebSocket permessage-deflate** availability in Rust (see 8.1). Functionally optional.
9. **Upstream drift.** zenith syncs upstream T3 Code (`scripts/code-sync.sh`). After the port, every upstream server change must be re-ported by hand. *Mitigation:* rerun WP-01 codegen at each sync (contract changes show up as compile errors), port upstream server diffs per module owner, and keep the black-box suite green against both backends. A decision for the owner: whether zenith keeps tracking upstream's server at all after the cut-over.
10. **Windows parity** (shims, ConPTY, PowerShell fallbacks). zenith targets macOS, so macOS comes first and Linux next. Windows code paths can stay unimplemented unless the owner wants them.

---

## 9. Verifying compatibility

There are **no browser or e2e tests** in the repo. `apps/server` has 357 test files (216k lines). They are mostly white-box: Effect layers with mocks, run by vitest via `vp test run`. The exception is `apps/server/src/server.test.ts` (13.2k lines, ≈220 tests): it boots the real route layer on a real port and talks to it with the **real Effect RPC client over a real WebSocket** and real `fetch`. It still injects mocks for about 40 services in roughly 60% of its tests. `apps/web` has 411 unit-test files and `client-runtime` 98; their `rpc/session.test.ts` hand-writes server frames, which makes it a useful spec.

Strategy, in order of value for the effort:

1. **Node oracle + schema conformance (WP-01/WP-34, from week 1).**
   - A ~50-line Node script reads `{tag, kind: payload|success|error|exit|chunk, json}` lines on stdin. It decodes each one with `Schema.decodeUnknownSync(Schema.toCodecJson(schema))` from the real contracts, and reports issues with `SchemaIssue.defaultFormatter`.
   - Rust tests pipe every serialized fixture through it. Run it in both directions: TS-encoded fixtures must deserialize in Rust, and Rust-encoded values must decode in TS.
   - Run it in CI.
2. **Validating, recording proxy (WP-34).**
   - A MITM on `/ws`, `/api`, `/oauth` and `/.well-known`, between `apps/web` and either backend. It writes JSONL `{conn, dir, t, frame}`.
   - It maps `requestId` → `tag` and validates every S→C `Chunk.values` and `Exit.exit` against `Rpc.exitSchema(rpc)`, and every HTTP body against its endpoint schema.
   - Developers run it all the time. Zero schema issues and zero `Defect` frames is the bar.
   - In **record/replay** mode it records against TS and replays the C→S frames against Rust, after normalization:
     - request ids keyed by `(tag, ordinal)`;
     - timestamps, UUIDs and tokens normalized;
     - streams compared as flattened `values` per request, not by frame boundaries;
     - the replayer generates its own Acks.
3. **Black-box port of `server.test.ts`.**
   - Extract a `ServerUnderTest.start({homeDir, env}) → {httpUrl, wsUrl, bootstrapCredential}` abstraction. For TS it spawns `node apps/server/src/bin.ts serve --base-dir … --port 0`; for Rust it spawns the binary with the same flags.
   - Keep the existing helpers verbatim: `wsRpcProtocolLayer`, `withWsRpcClient`, `bootstrapBrowserSession`, `exchangeAccessToken`, `makeDpopProof`, `withFirstWsAckHeld`.
   - Triage the ~220 tests:
     - auth, cookies, CORS, DPoP, tickets, descriptor, OTLP, assets, upload and error shapes port as they are (≈40%);
     - git, project and workspace tests get real temp repos instead of mocks;
     - provider, terminal and preview tests use **process-level fakes**, which work for both backends: `src/testUtils/fakeCli.ts`, `provider/testFixtures/codexCollabMockPeer.mjs`, a fake `$SHELL`;
     - internal-failure injection stays as Rust unit tests, or goes behind a debug-only `--test-faults` hook.
   - Run with `BACKEND=ts|rust` in CI. Divergence is the signal.
4. **State compatibility tests.**
   - (a) Open a copy of a real `~/.zenith/code` with Rust, run the M1 flows, then reopen it with the TS server.
   - (b) Replay the full `orchestration_events` log of a real DB through the Rust projectors into an empty DB, then diff every projection table (allow for timestamps written at projection time).
   - (c) Check that sessions issued by TS verify in Rust, and the reverse.
   - (d) Diff `sqlite_master` for a fresh DB.
   - (e) Diff the `settings.json` and `keybindings.json` round-trips.
5. **Provider golden tests.** Feed the `NTIVE:` frames from `logs/provider/events.*.log` (36 recorded threads on the owner's machine, plus fresh recordings) into each Rust adapter, and compare its canonical output with the `CANON:` lines. Combine with the ingestion reactor using a virtual clock, and compare the resulting orchestration events.
6. **Playwright smoke flows** (greenfield, about 1–2 weeks):
   - Build `apps/web`, start the backend with a seeded home and fake CLIs on `PATH`, with the validating proxy in front.
   - Flows: pair → shell loads → open a thread (HTTP snapshot + `subscribeThread` with `afterSequence`) → send a message (fake codex) → approve → terminal attach and type (windowed acks) → reload (IndexedDB resume) → kill and restart the backend (supervisor backoff, resume) → revoke a client.
   - Assertions: DOM state, zero proxy issues, no console loop of "Durable RPC subscription lost its transport".
   - Seeding can reuse `apps/server/scripts/mobile-showcase-environment.ts`: it seeds `state.sqlite` and pairs through the CLI.
7. **Port the TS unit tests per module, as specs.** Each WP owner ports the relevant `*.test.ts` cases as Rust unit tests: decider/projector tests, git parsing tests, PR JSON normalizers, the terminal sanitizer, keybinding `when` parsing, and so on. They are the best description of edge cases.

**Exit criterion for the cut-over:** the black-box suite is green for both backends, there are zero proxy issues during a week of owner dogfooding, the state-compat tests are green, and the Playwright smoke flows are green.

---

## 10. CLI compatibility and cut-over

- **Keep `code/apps/server/dist/bin.mjs` as a shim** while both backends exist, so that `node bin.mjs <args>` keeps working for `src/lib/code/{manager,cli}.ts` and `crates/zenith-server/src/code/{manager,cli}.rs`:
  ```js
  #!/usr/bin/env node
  // Execs the Rust zenith-code binary with the same argv, stdio and exit code; forwards signals.
  import { spawn } from "node:child_process"; import path from "node:path"; import { fileURLToPath } from "node:url";
  const here = path.dirname(fileURLToPath(import.meta.url));
  const bin = process.env.ZENITH_CODE_BIN ?? path.resolve(here, "../../../../target/release/zenith-code");
  const child = spawn(bin, process.argv.slice(2), { stdio: "inherit" });
  for (const s of ["SIGTERM", "SIGINT", "SIGHUP"]) process.on(s, () => child.kill(s));
  child.on("exit", (code, signal) => (signal ? process.kill(process.pid, signal) : process.exit(code ?? 1)));
  ```
  A build switch (`npm run code:build` vs `code:build:rust`) decides which `bin.mjs` gets installed. The TS one stays available for rollback.
- **Then change both dashboards** to spawn `target/release/zenith-code` directly. It is one constant each: `CODE_BIN` in `src/lib/code/paths.ts`, and `paths.rs` in `zenith-server`. That removes Node from the runtime path. `scripts/mac/install.sh` then builds it with `cargo build --release -p zenith-code`; the web client still builds with Vite/pnpm, at build time only.
- **The Rust binary must:**
  - accept every flag in §6.17, including trailing `--base-dir`;
  - print the same `serve` banner (the dashboard redacts `Token:` lines and `#token=`) and the same `--json` documents;
  - write and delete `userdata/server-runtime.json` identically (the dashboard adopts orphans with it);
  - answer `/zenith/embed.json` as soon as it listens;
  - exit cleanly on SIGTERM within 5 s.
- **Static client lookup:** keep `resolveStaticDir`'s order: `<exe dir>/client/index.html`, then `code/apps/server/dist/client`, then `code/apps/web/dist`. Add a `--static-dir` flag (and `ZENITH_CODE_STATIC_DIR`) so packaging is explicit.
- **Branding:** user-visible server strings ("T3 Code server is ready.", the MCP server name, the codex `clientInfo` name) are brand-swapped at build time today (`scripts/lib/zenith-brand.ts`). In Rust, put them in one `brand.rs`. The technical identifiers keep their upstream names: `t3_session_*`, `T3CODE_*`, `t3-code` MCP id, `refs/t3/checkpoints`, `t3.json`, `.t3code/vcs.json`.
- **Version:** report `serverVersion` ≥ the web build's version (reuse `apps/server/package.json` `version`, today 0.0.43), or the web shows a "server out of date" banner (`apps/web/src/versionSkew.ts`).
- **Later option:** `zenith-server` (the Rust dashboard) links `zenith-code` as a library and runs it in-process on its own port. The `bin.mjs`/CLI contract then only matters for `auth`/`project` commands, which become direct function calls.

---

## Appendix A: all 148 WS RPC methods

The method name is the wire `tag`. Kind `stream` means `Rpc.make(…, {stream: true})`. "scope" is the required auth scope from `apps/server/src/auth/RpcAuthorization.ts`: `orch:` stands for `orchestration:`, and `EnvAuthErr` = `EnvironmentAuthorizationError` (auth.ts), present in every error union. Schema names are followed by their file in `packages/contracts/src/`. Payload and success types written inline are inline `Schema.Struct`s in `rpc.ts`.

One method's scope depends on its payload: `device.list` needs `orchestration:operate` when `retryHostId` or `updateTool` is set (`requiredScopeForDeviceList`). The table is checked for completeness at compile time in TS (`satisfies Record<WsRpcMethod, …>`); do the same in Rust (the WP-01 generator emits it).

| # | method | kind | scope | payload | success | error |
|---|---|---|---|---|---|---|
| 1 | `server.upsertKeybinding` | unary | orch:operate | ServerUpsertKeybindingInput (server) | ServerUpsertKeybindingResult (server) | KeybindingsConfigError (keybindings) \| EnvAuthErr |
| 2 | `server.removeKeybinding` | unary | orch:operate | ServerRemoveKeybindingInput (server) | ServerRemoveKeybindingResult (server) | KeybindingsConfigError (keybindings) \| EnvAuthErr |
| 3 | `server.probe` | unary | orch:read | {} | {} | EnvAuthErr |
| 4 | `server.getConfig` | unary | orch:read | {} | ServerConfig (server) | KeybindingsConfigError (keybindings) \| ServerSettingsError (settings) \| EnvAuthErr |
| 5 | `server.refreshProviders` | unary | orch:operate | Schema.Struct({  instanceId: Schema.optional(ProviderInstanceId (providerInstance)), cwd: Schema.optional(TrimmedNonEmptyString (baseSchemas)),  refreshModels: Schema.optional(Schema.Boolean), }) | ServerProviderUpdatedPayload (server) | EnvAuthErr \| ProviderSetupError (providerSetup) |
| 6 | `server.updateProvider` | unary | orch:operate | ServerProviderUpdateInput (server) | ServerProviderUpdatedPayload (server) | ServerProviderUpdateError (server) \| EnvAuthErr |
| 7 | `provider.consumeResetCredit` | unary | orch:operate | ProviderConsumeResetCreditInput (providerUsageLimits) | ProviderConsumeResetCreditResult (providerUsageLimits) | ProviderSetupError (providerSetup) \| UsageLimitSourceError (providerUsageLimits) \| EnvAuthErr |
| 8 | `provider.auth.start` | unary | orch:operate | ProviderAuthStartInput (providerSetup) | ProviderAuthState (providerSetup) | ProviderSetupRpcError |
| 9 | `provider.auth.respond` | unary | orch:operate | ProviderAuthRespondInput (providerSetup) | ProviderAuthState (providerSetup) | ProviderSetupRpcError |
| 10 | `provider.auth.complete` | unary | orch:operate | ProviderAuthCompleteInput (providerSetup) | ProviderAuthState (providerSetup) | ProviderSetupRpcError |
| 11 | `provider.chatgpt.reconnect-profile` | unary | orch:operate | ChatGptReconnectProfileInput (providerSetup) | Schema.NullOr(ChatGptReconnectProfile (providerSetup)) | ProviderSetupRpcError |
| 12 | `provider.chatgpt.import-profile` | unary | orch:operate | ChatGptImportProfileInput (providerSetup) | ProviderAuthState (providerSetup) | ProviderSetupRpcError |
| 13 | `provider.chatgpt.handoff.subscribe` | stream | orch:operate | ChatGptHandoffInput (providerSetup) | ChatGptHandoffState (providerSetup) | ProviderSetupRpcError |
| 14 | `provider.codex.auth-callback.subscribe` | stream | orch:operate | CodexAuthCallbackInput (providerSetup) | CodexAuthCallbackState (providerSetup) | ProviderSetupRpcError |
| 15 | `provider.auth.cancel` | unary | orch:operate | ProviderAuthCancelInput (providerSetup) | ProviderAuthState (providerSetup) | ProviderSetupRpcError |
| 16 | `provider.auth.logout` | unary | orch:operate | ProviderSetupInput (providerSetup) | ProviderAuthState (providerSetup) | ProviderSetupRpcError |
| 17 | `provider.auth.subscribe` | stream | orch:operate | ProviderSetupInput (providerSetup) | ProviderAuthState (providerSetup) | ProviderSetupRpcError |
| 18 | `provider.install.start` | unary | orch:operate | ProviderSetupInput (providerSetup) | ProviderInstallState (providerSetup) | ProviderSetupRpcError |
| 19 | `provider.install.cancel` | unary | orch:operate | ProviderInstallCancelInput (providerSetup) | ProviderInstallState (providerSetup) | ProviderSetupRpcError |
| 20 | `provider.install.subscribe` | stream | orch:read | ProviderSetupInput (providerSetup) | ProviderInstallState (providerSetup) | ProviderSetupRpcError |
| 21 | `provider.install.remove` | unary | orch:operate | ProviderSetupInput (providerSetup) | ProviderInstallState (providerSetup) | ProviderSetupRpcError |
| 22 | `server.updateServer` | unary | orch:operate | ServerSelfUpdateInput (server) | ServerSelfUpdateResult (server) | ServerSelfUpdateError (server) \| EnvAuthErr |
| 23 | `server.updateServerWithProgress` | stream | orch:operate | ServerSelfUpdateInput (server) | ServerSelfUpdateProgressEvent (server) | ServerSelfUpdateError (server) \| EnvAuthErr |
| 24 | `server.commitDesktopUpdate` | unary | orch:operate | DesktopUpdateCommitInput (server) | ServerSelfUpdateResult (server) | ServerSelfUpdateError (server) \| EnvAuthErr |
| 25 | `server.getSettings` | unary | orch:read | {} | ServerSettings (settings) | ServerSettingsError (settings) \| EnvAuthErr |
| 26 | `server.updateSettings` | unary | orch:operate | Schema.Struct({ patch: ServerSettingsPatch (settings) }) | ServerSettings (settings) | ServerSettingsError (settings) \| EnvAuthErr |
| 27 | `server.discoverSourceControl` | unary | orch:read | {} | SourceControlDiscoveryResult (sourceControl) | EnvAuthErr |
| 28 | `server.getTraceDiagnostics` | unary | orch:read | {} | ServerTraceDiagnosticsResult (server) | EnvAuthErr |
| 29 | `server.getProcessDiagnostics` | unary | orch:read | {} | ServerProcessDiagnosticsResult (server) | EnvAuthErr |
| 30 | `server.getHostResources` | unary | orch:read | {} | HostResourcesSnapshot (resourceTelemetry) | EnvAuthErr |
| 31 | `server.getProcessResourceHistory` | unary | orch:read | ServerProcessResourceHistoryInput (server) | ServerProcessResourceHistoryResult (server) | EnvAuthErr |
| 32 | `server.getResourceTelemetryHistory` | unary | orch:read | ResourceTelemetryHistoryInput (resourceTelemetry) | ResourceTelemetryHistory (resourceTelemetry) | EnvAuthErr |
| 33 | `server.retryResourceTelemetry` | unary | orch:operate | {} | ResourceTelemetryRetryResult (resourceTelemetry) | EnvAuthErr |
| 34 | `server.getUsageSummary` | unary | orch:read | UsageSummaryInput (usage) | UsageSummary (usage) | EnvAuthErr \| UsageReadError (usage) |
| 35 | `server.refreshUsageRates` | unary | orch:read | {} | UsagePricing (usage) | EnvAuthErr |
| 36 | `server.signalProcess` | unary | orch:operate | ServerSignalProcessInput (server) | ServerSignalProcessResult (server) | EnvAuthErr |
| 37 | `cloud.getRelayClientStatus` | unary | relay:read | {} | RelayClientStatusSchema (relayClient) | EnvAuthErr |
| 38 | `cloud.installRelayClient` | stream | relay:write | {} | RelayClientInstallProgressEventSchema (relayClient) | RelayClientInstallFailedError (relayClient) \| EnvAuthErr |
| 39 | `server.reportClientActivity` | unary | orch:read | ClientActivityReportInput (background) | — | EnvAuthErr |
| 40 | `server.reportHostPowerState` | unary | orch:operate | HostPowerSnapshot (background) | — | EnvAuthErr |
| 41 | `server.getBackgroundPolicy` | unary | orch:read | {} | BackgroundPolicySnapshot (background) | EnvAuthErr |
| 42 | `pullRequests.list` | unary | orch:read | PullRequestListInput (pullRequest) | PullRequestListResult (pullRequest) | PullRequestRpcError |
| 43 | `pullRequests.listStats` | unary | orch:read | PullRequestListStatsInput (pullRequest) | PullRequestListStatsResult (pullRequest) | PullRequestRpcError |
| 44 | `pullRequests.routing` | unary | orch:read | PullRequestRef (pullRequest) | PullRequestRoutingResult (pullRequest) | PullRequestRpcError |
| 45 | `pullRequests.routingIdentity` | unary | orch:read | PullRequestRoutingIdentityInput (pullRequest) | PullRequestRoutingIdentityResult (pullRequest) | PullRequestRpcError |
| 46 | `pullRequests.summary` | unary | orch:read | PullRequestRef (pullRequest) | PullRequestSummary (pullRequest) | PullRequestRpcError |
| 47 | `pullRequests.stack` | unary | orch:read | PullRequestRef (pullRequest) | Schema.NullOr(PullRequestStack (pullRequest)) | PullRequestRpcError |
| 48 | `pullRequests.linkedThreads` | unary | orch:read | PullRequestRef (pullRequest) | PullRequestLinkedThreadsResult (pullRequest) | PullRequestRpcError |
| 49 | `pullRequests.detail` | unary | orch:read | PullRequestRef (pullRequest) | PullRequestDetail (pullRequest) | PullRequestRpcError |
| 50 | `pullRequests.preview` | unary | orch:read | PullRequestRef (pullRequest) | PullRequestPreview (pullRequest) | PullRequestRpcError |
| 51 | `pullRequests.activity` | unary | orch:read | PullRequestRef (pullRequest) | PullRequestActivity (pullRequest) | PullRequestRpcError |
| 52 | `pullRequests.threadComments` | unary | orch:read | PullRequestThreadCommentsInput (pullRequest) | PullRequestThreadCommentsResult (pullRequest) | PullRequestRpcError |
| 53 | `pullRequests.diffFileContents` | unary | orch:read | PullRequestDiffFileContentsInput (pullRequest) | PullRequestDiffFileContentsResult (pullRequest) | PullRequestRpcError |
| 54 | `pullRequests.filesViewed` | unary | orch:read | PullRequestRef (pullRequest) | PullRequestFilesViewedResult (pullRequest) | PullRequestRpcError |
| 55 | `pullRequests.setFilesViewed` | unary | orch:operate | PullRequestSetFilesViewedInput (pullRequest) | Schema.Void | PullRequestRpcError |
| 56 | `pullRequests.runAction` | unary | orch:operate | PullRequestActionInput (pullRequest) | Schema.Void | PullRequestRpcError |
| 57 | `pullRequests.update` | unary | orch:operate | PullRequestUpdateInput (pullRequest) | Schema.Void | PullRequestRpcError |
| 58 | `pullRequests.comment` | unary | orch:operate | PullRequestCommentInput (pullRequest) | Schema.Void | PullRequestRpcError |
| 59 | `pullRequests.updateComment` | unary | orch:operate | PullRequestCommentUpdateInput (pullRequest) | Schema.Void | PullRequestRpcError |
| 60 | `pullRequests.submitReview` | unary | orch:operate | PullRequestSubmitReviewInput (pullRequest) | Schema.Void | PullRequestRpcError |
| 61 | `pullRequests.replyToThread` | unary | orch:operate | PullRequestThreadReplyInput (pullRequest) | Schema.Void | PullRequestRpcError |
| 62 | `pullRequests.setThreadResolution` | unary | orch:operate | PullRequestThreadResolutionInput (pullRequest) | Schema.Void | PullRequestRpcError |
| 63 | `pullRequests.setReaction` | unary | orch:operate | PullRequestReactionInput (pullRequest) | Schema.Void | PullRequestRpcError |
| 64 | `pullRequests.invalidate` | unary | orch:read | PullRequestInvalidateInput (pullRequest) | Schema.Void | PullRequestRpcError |
| 65 | `pullRequests.subscribeRefreshes` | stream | orch:read | {} | NonNegativeInt (baseSchemas) | EnvAuthErr |
| 66 | `pullRequests.reviewerCandidates` | unary | orch:read | PullRequestRef (pullRequest) | PullRequestReviewerCandidateList (pullRequest) | PullRequestRpcError |
| 67 | `pullRequests.requestReviewers` | unary | orch:operate | PullRequestReviewerRequestInput (pullRequest) | Schema.Void | PullRequestRpcError |
| 68 | `pullRequests.labelCandidates` | unary | orch:read | PullRequestRef (pullRequest) | PullRequestLabelCandidateList (pullRequest) | PullRequestRpcError |
| 69 | `pullRequests.setLabels` | unary | orch:operate | PullRequestLabelChangeInput (pullRequest) | Schema.Void | PullRequestRpcError |
| 70 | `sourceControl.lookupRepository` | unary | orch:read | SourceControlRepositoryLookupInput (sourceControl) | SourceControlRepositoryInfo (sourceControl) | SourceControlRepositoryError (sourceControl) \| EnvAuthErr |
| 71 | `sourceControl.cloneRepository` | unary | orch:operate | SourceControlCloneRepositoryInput (sourceControl) | SourceControlCloneRepositoryResult (sourceControl) | SourceControlRepositoryError (sourceControl) \| EnvAuthErr |
| 72 | `projectClone.start` | unary | orch:operate | ProjectCloneStartInput (projectClone) | ProjectCloneStartResult (projectClone) | SourceControlRepositoryError (sourceControl) \| OrchestrationDispatchCommandError (orchestration) \| EnvAuthErr |
| 73 | `projectClone.cancel` | unary | orch:operate | ProjectCloneActionInput (projectClone) | ProjectCloneActionResult (projectClone) | EnvAuthErr |
| 74 | `projectClone.retry` | unary | orch:operate | ProjectCloneActionInput (projectClone) | ProjectCloneActionResult (projectClone) | SourceControlRepositoryError (sourceControl) \| EnvAuthErr |
| 75 | `subscribeProjectClones` | stream | orch:read | ProjectCloneSubscribeInput (projectClone) | ProjectCloneListEvent (projectClone) | EnvAuthErr |
| 76 | `sourceControl.publishRepository` | unary | orch:operate | SourceControlPublishRepositoryInput (sourceControl) | SourceControlPublishRepositoryResult (sourceControl) | SourceControlRepositoryError (sourceControl) \| EnvAuthErr |
| 77 | `projects.searchEntries` | unary | orch:read | ProjectSearchEntriesInput (project) | ProjectSearchEntriesResult (project) | ProjectSearchEntriesError (project) \| EnvAuthErr |
| 78 | `projects.searchContents` | unary | orch:read | ProjectSearchContentsInput (project) | ProjectSearchContentsResult (project) | ProjectSearchContentsError (project) \| EnvAuthErr |
| 79 | `projects.listEntries` | unary | orch:read | ProjectListEntriesInput (project) | ProjectListEntriesResult (project) | ProjectListEntriesError (project) \| EnvAuthErr |
| 80 | `projects.readFile` | unary | orch:read | ProjectReadFileInput (project) | ProjectReadFileResult (project) | ProjectReadFileError (project) \| EnvAuthErr |
| 81 | `projects.writeFile` | unary | orch:operate | ProjectWriteFileInput (project) | ProjectWriteFileResult (project) | ProjectWriteFileError (project) \| EnvAuthErr |
| 82 | `shell.openInEditor` | unary | orch:operate | LaunchEditorInput (editor) | — | ExternalLauncherError (editor) \| EnvAuthErr |
| 83 | `filesystem.browse` | unary | orch:read | FilesystemBrowseInput (filesystem) | FilesystemBrowseResult (filesystem) | FilesystemBrowseError (filesystem) \| EnvAuthErr |
| 84 | `agentSessions.scan` | unary | orch:read | AgentSessionScanInput (agentSessions) | AgentSessionScanResult (agentSessions) | AgentSessionScanError (agentSessions) \| EnvAuthErr |
| 85 | `agentSessions.import` | unary | orch:operate | AgentSessionImportInput (agentSessions) | AgentSessionImportResult (agentSessions) | AgentSessionImportProjectChangedError (agentSessions) \| AgentSessionImportProjectNotFoundError (agentSessions) \| AgentSessionScanError (agentSessions) \| EnvAuthErr |
| 86 | `assets.createUrl` | unary | orch:read | AssetCreateUrlInput (assets) | AssetCreateUrlResult (assets) | AssetAccessError (assets) \| EnvAuthErr |
| 87 | `attachments.createUploadUrl` | unary | orch:operate | AttachmentCreateUploadUrlInput (assets) | AttachmentCreateUploadUrlResult (assets) | AttachmentUploadSigningKeyError (assets) \| EnvAuthErr |
| 88 | `attachments.delete` | unary | orch:operate | AttachmentDeleteInput (assets) | — | EnvAuthErr |
| 89 | `provider.uploadFeedback` | unary | orch:operate | ProviderUploadFeedbackInput (provider) | ProviderUploadFeedbackResult (provider) | ProviderUploadFeedbackError (provider) \| EnvAuthErr |
| 90 | `subscribeVcsStatus` | stream | orch:read | VcsStatusInput (git) | VcsStatusStreamEvent (git) | GitManagerServiceError (git) \| EnvAuthErr |
| 91 | `vcs.pull` | unary | orch:operate | VcsPullInput (git) | VcsPullResult (git) | GitCommandError (git) \| EnvAuthErr |
| 92 | `vcs.refreshStatus` | unary | orch:read | VcsStatusInput (git) | VcsStatusResult (git) | GitManagerServiceError (git) \| EnvAuthErr |
| 93 | `subscribeWorktreeSetup` | stream | orch:read | WorktreeSetupSubscribeInput (worktreeSetup) | WorktreeSetupStreamEvent (worktreeSetup) | EnvAuthErr |
| 94 | `worktreeSetup.cancel` | unary | orch:operate | WorktreeSetupCancelInput (worktreeSetup) | WorktreeSetupCancelResult (worktreeSetup) | EnvAuthErr |
| 95 | `git.runStackedAction` | stream | orch:operate | GitRunStackedActionInput (git) | GitActionProgressEvent (git) | GitManagerServiceError (git) \| EnvAuthErr |
| 96 | `git.resolvePullRequest` | unary | orch:operate | GitPullRequestRefInput (git) | GitResolvePullRequestResult (git) | GitManagerServiceError (git) \| EnvAuthErr |
| 97 | `git.preparePullRequestThread` | unary | orch:operate | GitPreparePullRequestThreadInput (git) | GitPreparePullRequestThreadResult (git) | GitManagerServiceError (git) \| EnvAuthErr |
| 98 | `vcs.listRefs` | unary | orch:read | VcsListRefsInput (git) | VcsListRefsResult (git) | GitCommandError (git) \| EnvAuthErr |
| 99 | `vcs.createWorktree` | unary | orch:operate | VcsCreateWorktreeInput (git) | VcsCreateWorktreeResult (git) | GitCommandError (git) \| EnvAuthErr |
| 100 | `vcs.removeWorktree` | unary | orch:operate | VcsRemoveWorktreeInput (git) | — | GitCommandError (git) \| EnvAuthErr |
| 101 | `vcs.createRef` | unary | orch:operate | VcsCreateRefInput (git) | VcsCreateRefResult (git) | GitCommandError (git) \| EnvAuthErr |
| 102 | `vcs.switchRef` | unary | orch:operate | VcsSwitchRefInput (git) | VcsSwitchRefResult (git) | GitCommandError (git) \| EnvAuthErr |
| 103 | `vcs.init` | unary | orch:operate | VcsInitInput (git) | — | VcsError (vcs) \| EnvAuthErr |
| 104 | `review.getDiffPreview` | unary | review:write | ReviewDiffPreviewInput (review) | ReviewDiffPreviewResult (review) | ReviewDiffPreviewError (review) \| EnvAuthErr |
| 105 | `review.getDiffFileContents` | unary | review:write | ReviewDiffFileContentsInput (review) | ReviewDiffFileContentsResult (review) | ReviewDiffPreviewError (review) \| EnvAuthErr |
| 106 | `terminal.open` | unary | terminal:operate | TerminalOpenInput (terminal) | TerminalSessionSnapshot (terminal) | TerminalError (terminal) \| EnvAuthErr |
| 107 | `terminal.attach` | stream | terminal:operate | TerminalAttachInput (terminal) | TerminalAttachStreamEvent (terminal) | TerminalError (terminal) \| EnvAuthErr |
| 108 | `terminal.write` | unary | terminal:operate | TerminalWriteInput (terminal) | — | TerminalError (terminal) \| EnvAuthErr |
| 109 | `terminal.resize` | unary | terminal:operate | TerminalResizeInput (terminal) | — | TerminalError (terminal) \| EnvAuthErr |
| 110 | `terminal.clear` | unary | terminal:operate | TerminalClearInput (terminal) | — | TerminalError (terminal) \| EnvAuthErr |
| 111 | `terminal.restart` | unary | terminal:operate | TerminalRestartInput (terminal) | TerminalSessionSnapshot (terminal) | TerminalError (terminal) \| EnvAuthErr |
| 112 | `terminal.close` | unary | terminal:operate | TerminalCloseInput (terminal) | — | TerminalError (terminal) \| EnvAuthErr |
| 113 | `preview.open` | unary | orch:operate | PreviewOpenInput (preview) | PreviewSessionSnapshot (preview) | PreviewError (preview) \| EnvAuthErr |
| 114 | `preview.navigate` | unary | orch:operate | PreviewNavigateInput (preview) | PreviewSessionSnapshot (preview) | PreviewError (preview) \| EnvAuthErr |
| 115 | `preview.resize` | unary | orch:operate | PreviewResizeInput (preview) | PreviewSessionSnapshot (preview) | PreviewError (preview) \| EnvAuthErr |
| 116 | `preview.refresh` | unary | orch:operate | PreviewRefreshInput (preview) | — | PreviewError (preview) \| EnvAuthErr |
| 117 | `preview.close` | unary | orch:operate | PreviewCloseInput (preview) | — | PreviewError (preview) \| EnvAuthErr |
| 118 | `preview.list` | unary | orch:read | PreviewListInput (preview) | PreviewListResult (preview) | EnvAuthErr |
| 119 | `preview.reportStatus` | unary | orch:operate | PreviewReportStatusInput (preview) | — | PreviewError (preview) \| EnvAuthErr |
| 120 | `previewAutomation.connect` | stream | orch:operate | PreviewAutomationHost (previewAutomation) | PreviewAutomationStreamEvent (previewAutomation) | PreviewAutomationError (previewAutomation) \| EnvAuthErr |
| 121 | `previewAutomation.respond` | unary | orch:operate | PreviewAutomationResponse (previewAutomation) | — | PreviewAutomationError (previewAutomation) \| EnvAuthErr |
| 122 | `previewAutomation.focusHost` | unary | orch:operate | PreviewAutomationHostFocus (previewAutomation) | — | EnvAuthErr |
| 123 | `subscribePreviewEvents` | stream | orch:read | {} | PreviewEvent (preview) | EnvAuthErr |
| 124 | `subscribeDiscoveredLocalServers` | stream | orch:read | Schema.Struct({ configuredUrls: Schema.optional(ConfiguredLocalServerUrls (preview)), }) | DiscoveredLocalServerList (preview) | EnvAuthErr |
| 125 | `device.testHost` | unary | orch:operate | SshDeviceHostConfig (device) | DeviceHostSummary (device) | DeviceError (device) \| EnvAuthErr |
| 126 | `device.list` | unary | orch:read | DeviceListInput (device) | DeviceServiceState (device) | DeviceError (device) \| EnvAuthErr |
| 127 | `device.configure` | unary | orch:operate | DeviceConfigureInput (device) | DeviceServiceState (device) | DeviceError (device) \| EnvAuthErr |
| 128 | `device.open` | unary | orch:operate | DeviceOpenInput (device) | DeviceSession (device) | DeviceError (device) \| EnvAuthErr |
| 129 | `device.close` | unary | orch:operate | DeviceCloseInput (device) | — | DeviceError (device) \| EnvAuthErr |
| 130 | `device.shutdown` | unary | orch:operate | DeviceShutdownInput (device) | — | DeviceError (device) \| EnvAuthErr |
| 131 | `device.detail` | unary | orch:read | DeviceDetailInput (device) | DeviceDetail (device) | DeviceError (device) \| EnvAuthErr |
| 132 | `device.action` | unary | orch:operate | DeviceActionInput (device) | DeviceDetail (device) | DeviceError (device) \| EnvAuthErr |
| 133 | `subscribeDeviceState` | stream | orch:read | {} | DeviceServiceState (device) | EnvAuthErr |
| 134 | `orchestration.dispatchCommand` | unary | orch:operate | ClientOrchestrationCommand (orchestration) | DispatchResult (orchestration) | OrchestrationDispatchCommandError (orchestration) \| EnvAuthErr |
| 135 | `orchestration.getWorkflowScript` | unary | orch:read | OrchestrationGetWorkflowScriptInput (orchestration) | OrchestrationGetWorkflowScriptResult (orchestration) | OrchestrationGetWorkflowScriptError (orchestration) \| EnvAuthErr |
| 136 | `orchestration.getTurnDiff` | unary | orch:read | OrchestrationGetTurnDiffInput (orchestration) | OrchestrationGetTurnDiffResult (orchestration) | OrchestrationGetTurnDiffError (orchestration) \| EnvAuthErr |
| 137 | `orchestration.getFullThreadDiff` | unary | orch:read | OrchestrationGetFullThreadDiffInput (orchestration) | OrchestrationGetFullThreadDiffResult (orchestration) | OrchestrationGetFullThreadDiffError (orchestration) \| EnvAuthErr |
| 138 | `orchestration.searchThreads` | unary | orch:read | OrchestrationSearchThreadsInput (orchestration) | OrchestrationSearchThreadsResult (orchestration) | OrchestrationSearchThreadsError (orchestration) \| EnvAuthErr |
| 139 | `orchestration.getArchivedShellSnapshot` | unary | orch:read | {} | OrchestrationShellSnapshot (orchestration) | OrchestrationGetSnapshotError (orchestration) \| EnvAuthErr |
| 140 | `orchestration.subscribeShell` | stream | orch:read | OrchestrationSubscribeShellInput (orchestration) | OrchestrationShellStreamItem (orchestration) | OrchestrationGetSnapshotError (orchestration) \| EnvAuthErr |
| 141 | `orchestration.subscribeThread` | stream | orch:read | OrchestrationSubscribeThreadInput (orchestration) | OrchestrationThreadStreamItem (orchestration) | OrchestrationGetSnapshotError (orchestration) \| EnvAuthErr |
| 142 | `subscribeTerminalEvents` | stream | terminal:operate | {} | TerminalEvent (terminal) | EnvAuthErr |
| 143 | `subscribeTerminalMetadata` | stream | terminal:operate | {} | TerminalMetadataStreamEvent (terminal) | EnvAuthErr |
| 144 | `subscribeServerConfig` | stream | orch:read | Schema.Struct({  environmentThemes: Schema.optional(Schema.Boolean),  usageLimitSources: Schema.optional(Schema.Boolean),  usageLimitsCommand: Schema.optional(Schema.Boolean), }) | ServerConfigStreamEvent (server) | KeybindingsConfigError (keybindings) \| ServerSettingsError (settings) \| EnvAuthErr |
| 145 | `subscribeServerLifecycle` | stream | orch:read | {} | ServerLifecycleStreamEvent (server) | EnvAuthErr |
| 146 | `subscribeAuthAccess` | stream | access:read | {} | AuthAccessStreamEvent (auth) | AuthAccessStreamError (auth) \| EnvAuthErr |
| 147 | `subscribeBackgroundPolicy` | stream | orch:read | {} | BackgroundPolicySnapshot (background) | EnvAuthErr |
| 148 | `subscribeResourceTelemetry` | stream | orch:read | {} | ResourceTelemetrySnapshot (resourceTelemetry) | EnvAuthErr |

## Appendix B: final SQLite schema

Reconstructed from `apps/server/src/persistence/Migrations/001…054`. The column lists were cross-checked against a live `~/.zenith/code/userdata/state.sqlite` (all 54 migrations applied). The trailing comments give the migration that added each column. For the byte-exact DDL, run `sqlite3 state.sqlite .schema` on a TS-created database; CI diffs `sqlite_master` between a TS-created and a Rust-created DB.

```sql
-- Event store (append-only, global order)
CREATE TABLE orchestration_events (
  sequence INTEGER PRIMARY KEY AUTOINCREMENT,      -- global, monotonic; the "snapshotSequence"
  event_id TEXT NOT NULL UNIQUE,                   -- UUIDv4
  aggregate_kind TEXT NOT NULL,                    -- 'project' | 'thread'
  stream_id TEXT NOT NULL,                         -- projectId | threadId
  stream_version INTEGER NOT NULL,                 -- per-aggregate, computed in INSERT via COALESCE(max+1, 0)
  event_type TEXT NOT NULL,
  occurred_at TEXT NOT NULL,
  command_id TEXT,
  causation_event_id TEXT,
  correlation_id TEXT,                             -- = command_id in practice
  actor_kind TEXT NOT NULL,                        -- 'client'|'server'|'provider' (inferred, see §2.3)
  payload_json TEXT NOT NULL,
  metadata_json TEXT NOT NULL
);
CREATE UNIQUE INDEX idx_orch_events_stream_version ON orchestration_events(aggregate_kind, stream_id, stream_version);
CREATE INDEX idx_orch_events_stream_sequence ON orchestration_events(aggregate_kind, stream_id, sequence);
CREATE INDEX idx_orch_events_command_id ON orchestration_events(command_id);
CREATE INDEX idx_orch_events_correlation_id ON orchestration_events(correlation_id);

CREATE TABLE orchestration_command_receipts (
  command_id TEXT PRIMARY KEY,
  aggregate_kind TEXT NOT NULL,
  aggregate_id TEXT NOT NULL,
  accepted_at TEXT NOT NULL,
  result_sequence INTEGER NOT NULL,
  status TEXT NOT NULL,                            -- 'accepted' | 'rejected'
  error TEXT
);
CREATE INDEX idx_orch_command_receipts_aggregate ON orchestration_command_receipts(aggregate_kind, aggregate_id);
CREATE INDEX idx_orch_command_receipts_sequence ON orchestration_command_receipts(result_sequence);

CREATE TABLE checkpoint_diff_blobs (               -- unused
  thread_id TEXT NOT NULL, from_turn_count INTEGER NOT NULL, to_turn_count INTEGER NOT NULL,
  diff TEXT NOT NULL, created_at TEXT NOT NULL,
  UNIQUE (thread_id, from_turn_count, to_turn_count)
);
CREATE INDEX idx_checkpoint_diff_blobs_thread_to_turn ON checkpoint_diff_blobs(thread_id, to_turn_count);

CREATE TABLE provider_session_runtime (            -- owned by provider layer (persistence/ProviderSessionRuntime.ts)
  thread_id TEXT PRIMARY KEY,
  provider_name TEXT NOT NULL,
  adapter_key TEXT NOT NULL,
  runtime_mode TEXT NOT NULL DEFAULT 'full-access',
  status TEXT NOT NULL,                            -- 'starting'|'running'|'stopped'|'error'
  last_seen_at TEXT NOT NULL,
  resume_cursor_json TEXT,
  runtime_payload_json TEXT,
  provider_instance_id TEXT
);
CREATE INDEX idx_provider_session_runtime_status ON provider_session_runtime(status);
CREATE INDEX idx_provider_session_runtime_provider ON provider_session_runtime(provider_name);
CREATE INDEX idx_provider_session_runtime_instance ON provider_session_runtime(provider_instance_id);

-- Projections (read models)
CREATE TABLE projection_projects (
  project_id TEXT PRIMARY KEY,
  title TEXT NOT NULL,
  workspace_root TEXT NOT NULL,
  scripts_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  deleted_at TEXT,
  default_model_selection_json TEXT,               -- 016
  default_thread_env_mode TEXT,                    -- 039
  favicon_path TEXT,                               -- 040
  auto_pull INTEGER NOT NULL DEFAULT 0,            -- 045
  project_icon_json TEXT                           -- 047 (encoded wire form of ProjectIconOverride)
);
CREATE INDEX idx_projection_projects_updated_at ON projection_projects(updated_at);
CREATE INDEX idx_projection_projects_workspace_root_deleted_at ON projection_projects(workspace_root, deleted_at);
-- NOTE: repositoryIdentity is NOT persisted; it is resolved at read time from git.

CREATE TABLE projection_threads (
  thread_id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  title TEXT NOT NULL,
  branch TEXT,
  worktree_path TEXT,
  latest_turn_id TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  deleted_at TEXT,
  runtime_mode TEXT NOT NULL DEFAULT 'full-access',          -- 010
  interaction_mode TEXT NOT NULL DEFAULT 'default',          -- 012
  model_selection_json TEXT,                                  -- 016 (always written)
  archived_at TEXT,                                           -- 017
  latest_user_message_at TEXT,                                -- 023
  pending_approval_count INTEGER NOT NULL DEFAULT 0,          -- 023
  pending_user_input_count INTEGER NOT NULL DEFAULT 0,        -- 023
  has_actionable_proposed_plan INTEGER NOT NULL DEFAULT 0,    -- 023
  settled_override TEXT,            -- 'settled'|'active'     -- 033
  settled_at TEXT,                                            -- 033
  snoozed_until TEXT, snoozed_at TEXT,                        -- 034
  title_regeneration_request_id TEXT, title_regeneration_started_at TEXT, -- 035
  pinned_at TEXT,                                             -- 036
  pin_order_key TEXT,                                         -- 038
  linked_pull_request_json TEXT,                              -- 042 (legacy)
  unsettled_at TEXT,                                          -- 043
  branch_pull_request_json TEXT,                              -- 048
  active_order_key TEXT,                                      -- 049
  title_state_json TEXT,                                      -- 052
  auto_settle_disabled_at TEXT                                -- 054
);
CREATE INDEX idx_projection_threads_project_id ON projection_threads(project_id);
CREATE INDEX idx_projection_threads_project_archived_at ON projection_threads(project_id, archived_at);
CREATE INDEX idx_projection_threads_project_deleted_created ON projection_threads(project_id, deleted_at, created_at);
CREATE INDEX idx_projection_threads_shell_active ON projection_threads(deleted_at, archived_at, project_id, created_at, thread_id);
CREATE INDEX idx_projection_threads_shell_archived ON projection_threads(deleted_at, archived_at, project_id, thread_id);

CREATE TABLE projection_thread_messages (
  message_id TEXT PRIMARY KEY,           -- 'import:*' reserved for imported agent sessions; 'reasoning:*' prefix for reasoning
  thread_id TEXT NOT NULL,
  turn_id TEXT,
  role TEXT NOT NULL,                    -- user|assistant|system|reasoning
  text TEXT NOT NULL,
  is_streaming INTEGER NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  attachments_json TEXT,                 -- 007
  context_json TEXT                      -- 051
);
CREATE INDEX idx_projection_thread_messages_thread_created ON projection_thread_messages(thread_id, created_at);
CREATE INDEX idx_projection_thread_messages_thread_created_id ON projection_thread_messages(thread_id, created_at, message_id);

CREATE TABLE projection_thread_activities (
  activity_id TEXT PRIMARY KEY,          -- = EventId
  thread_id TEXT NOT NULL,
  turn_id TEXT,
  tone TEXT NOT NULL,                    -- info|tool|approval|error
  kind TEXT NOT NULL,                    -- e.g. 'approval.requested', 'user-input.answer-submitted', ...
  summary TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  created_at TEXT NOT NULL,
  sequence INTEGER                       -- 008
);
CREATE INDEX idx_projection_thread_activities_thread_created ON projection_thread_activities(thread_id, created_at);
CREATE INDEX idx_projection_thread_activities_thread_sequence ON projection_thread_activities(thread_id, sequence);
CREATE INDEX idx_projection_thread_activities_thread_sequence_created_id ON projection_thread_activities(thread_id, sequence, created_at, activity_id);

CREATE TABLE projection_thread_sessions (
  thread_id TEXT PRIMARY KEY,
  status TEXT NOT NULL,                  -- idle|starting|running|ready|interrupted|stopped|error
  provider_name TEXT,
  provider_session_id TEXT,
  provider_thread_id TEXT,
  active_turn_id TEXT,
  last_error TEXT,
  updated_at TEXT NOT NULL,
  runtime_mode TEXT NOT NULL DEFAULT 'full-access',   -- 006
  provider_instance_id TEXT                           -- 028
);
CREATE INDEX idx_projection_thread_sessions_provider_session ON projection_thread_sessions(provider_session_id);
CREATE INDEX idx_projection_thread_sessions_instance ON projection_thread_sessions(provider_instance_id);

CREATE TABLE projection_turns (
  row_id INTEGER PRIMARY KEY AUTOINCREMENT,
  thread_id TEXT NOT NULL,
  turn_id TEXT,                          -- NULL for a pending turn start
  pending_message_id TEXT,
  assistant_message_id TEXT,
  state TEXT NOT NULL,                   -- pending|running|interrupted|completed|error
  requested_at TEXT NOT NULL,
  started_at TEXT,
  completed_at TEXT,
  checkpoint_turn_count INTEGER,
  checkpoint_ref TEXT,                   -- refs/t3/checkpoints/<b64url(threadId)>/turn/<n>
  checkpoint_status TEXT,                -- ready|missing|error
  checkpoint_files_json TEXT NOT NULL,
  source_proposed_plan_thread_id TEXT,   -- 015
  source_proposed_plan_id TEXT,          -- 015
  UNIQUE (thread_id, turn_id),
  UNIQUE (thread_id, checkpoint_turn_count)
);
CREATE INDEX idx_projection_turns_thread_requested ON projection_turns(thread_id, requested_at);
CREATE INDEX idx_projection_turns_thread_checkpoint_completed ON projection_turns(thread_id, checkpoint_turn_count, completed_at);
CREATE INDEX idx_projection_turns_thread_keyset ON projection_turns(thread_id, requested_at, turn_id);

CREATE TABLE projection_pending_approvals (
  request_id TEXT PRIMARY KEY,
  thread_id TEXT NOT NULL,
  turn_id TEXT,
  status TEXT NOT NULL,                  -- pending|resolved
  decision TEXT,
  created_at TEXT NOT NULL,
  resolved_at TEXT
);
CREATE INDEX idx_projection_pending_approvals_thread_status ON projection_pending_approvals(thread_id, status);

CREATE TABLE projection_thread_proposed_plans (
  plan_id TEXT PRIMARY KEY,
  thread_id TEXT NOT NULL,
  turn_id TEXT,
  plan_markdown TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  implemented_at TEXT,                   -- 014
  implementation_thread_id TEXT          -- 014
);
CREATE INDEX idx_projection_thread_proposed_plans_thread_created ON projection_thread_proposed_plans(thread_id, created_at);

CREATE TABLE projection_thread_pull_requests (       -- 050
  thread_id TEXT NOT NULL,
  host TEXT NOT NULL,
  repository TEXT NOT NULL,
  number INTEGER NOT NULL,
  url TEXT NOT NULL,
  source TEXT NOT NULL,                  -- manual|created|agent|stack|stack-dismissed
  linked_at TEXT NOT NULL,
  snapshot_json TEXT,
  stack_json TEXT,
  PRIMARY KEY (thread_id, host, repository, number)
);
CREATE INDEX idx_projection_thread_pull_requests_pr ON projection_thread_pull_requests(host, repository, number);

CREATE TABLE projection_state (            -- per-projector cursors
  projector TEXT PRIMARY KEY,
  last_applied_sequence INTEGER NOT NULL,
  updated_at TEXT NOT NULL
);
-- rows: projection.projects, projection.threads, projection.thread-messages,
-- projection.thread-proposed-plans, projection.thread-activities, projection.thread-sessions,
-- projection.thread-turns, projection.checkpoints, projection.pending-approvals,
-- projection.attachment-cleanup

-- Non-orchestration tables that live in the same DB
CREATE TABLE auth_pairing_links (          -- 031 + 032
  id TEXT PRIMARY KEY, credential TEXT NOT NULL UNIQUE, method TEXT NOT NULL, scopes TEXT NOT NULL,
  subject TEXT NOT NULL, label TEXT, created_at TEXT NOT NULL, expires_at TEXT NOT NULL,
  consumed_at TEXT, revoked_at TEXT, proof_key_thumbprint TEXT
);
CREATE INDEX idx_auth_pairing_links_active ON auth_pairing_links(revoked_at, consumed_at, expires_at);
CREATE TABLE auth_sessions (               -- 031 + 041
  session_id TEXT PRIMARY KEY, subject TEXT NOT NULL, scopes TEXT NOT NULL, method TEXT NOT NULL,
  client_label TEXT, client_ip_address TEXT, client_user_agent TEXT,
  client_device_type TEXT NOT NULL DEFAULT 'unknown', client_os TEXT, client_browser TEXT,
  issued_at TEXT NOT NULL, expires_at TEXT NOT NULL, last_connected_at TEXT, revoked_at TEXT,
  client_surface TEXT, client_app_version TEXT
);
CREATE INDEX idx_auth_sessions_active ON auth_sessions(revoked_at, expires_at, issued_at);
CREATE TABLE pull_request_files_viewed (   -- 053
  provider TEXT NOT NULL, host TEXT NOT NULL, repository TEXT NOT NULL, number INTEGER NOT NULL,
  viewer TEXT NOT NULL, path TEXT NOT NULL, revision TEXT, viewed_at TEXT NOT NULL,
  PRIMARY KEY (provider, host, repository, number, viewer, path)
) WITHOUT ROWID;
```

All 54 migrations, in order:

| # | Name | Effect |
|---|---|---|
| 1 | OrchestrationEvents | CREATE `orchestration_events` and 4 indexes |
| 2 | OrchestrationCommandReceipts | CREATE `orchestration_command_receipts` and 2 indexes |
| 3 | CheckpointDiffBlobs | CREATE `checkpoint_diff_blobs` and index. **Dead:** no code reads or writes it. |
| 4 | ProviderSessionRuntime | CREATE `provider_session_runtime` and 2 indexes |
| 5 | Projections | CREATE `projection_projects`, `projection_threads`, `projection_thread_messages`, `projection_thread_activities`, `projection_thread_sessions`, `projection_turns`, `projection_pending_approvals`, `projection_state`, and 8 indexes |
| 6 | ProjectionThreadSessionRuntimeModeColumns | `projection_thread_sessions` ADD `runtime_mode TEXT NOT NULL DEFAULT 'full-access'` |
| 7 | ProjectionThreadMessageAttachments | `projection_thread_messages` ADD `attachments_json TEXT` |
| 8 | ProjectionThreadActivitySequence | `projection_thread_activities` ADD `sequence INTEGER`, plus index (thread_id, sequence) |
| 9 | ProviderSessionRuntimeMode | No-op |
| 10 | ProjectionThreadsRuntimeMode | `projection_threads` ADD `runtime_mode TEXT NOT NULL DEFAULT 'full-access'` |
| 11 | OrchestrationThreadCreatedRuntimeMode | Data: `json_set(payload_json,'$.runtimeMode','full-access')` on `thread.created` events |
| 12 | ProjectionThreadsInteractionMode | `projection_threads` ADD `interaction_mode TEXT NOT NULL DEFAULT 'default'` |
| 13 | ProjectionThreadProposedPlans | CREATE `projection_thread_proposed_plans` and index |
| 14 | ProjectionThreadProposedPlanImplementation | ADD `implemented_at`, `implementation_thread_id` |
| 15 | ProjectionTurnsSourceProposedPlan | `projection_turns` ADD `source_proposed_plan_thread_id`, `source_proposed_plan_id` |
| 16 | CanonicalizeModelSelections (235 LOC) | ADD `projection_projects.default_model_selection_json` and `projection_threads.model_selection_json`, backfilled from the old string model. **DROP COLUMN** `projection_projects.default_model` and `projection_threads.model` (needs SQLite ≥ 3.35). Rewrites event payloads. |
| 17 | ProjectionThreadsArchivedAt | ADD `archived_at` |
| 18 | ProjectionThreadsArchivedAtIndex | Index (project_id, archived_at) |
| 19 | ProjectionSnapshotLookupIndexes | 2 indexes |
| 20 | AuthAccessManagement | CREATE `auth_pairing_links`, `auth_sessions` (both superseded by 31) |
| 21 | AuthSessionClientMetadata | ADD label and client_* columns |
| 22 | AuthSessionLastConnectedAt | ADD `last_connected_at` |
| 23 | ProjectionThreadShellSummary | ADD `latest_user_message_at`, `pending_approval_count`, `pending_user_input_count`, `has_actionable_proposed_plan` |
| 24 | BackfillProjectionThreadShellSummary (277 LOC) | Data backfill of pending approvals and shell counts from activities |
| 25 | CleanupInvalidProjectionPendingApprovals | Data delete and recount |
| 26 | CanonicalizeModelSelectionOptions (138 LOC) | Data: rewrites model-selection JSON in projections and events |
| 27 | ProviderSessionRuntimeInstanceId | ADD `provider_instance_id`, plus index |
| 28 | ProjectionThreadSessionInstanceId | ADD `provider_instance_id`, plus index |
| 29 | ProjectionThreadDetailOrderingIndexes | 2 indexes |
| 30 | ProjectionThreadShellArchiveIndexes | 2 indexes |
| 31 | AuthAuthorizationScopes | **DROP and recreate** `auth_pairing_links` and `auth_sessions` with `scopes` in place of `role` |
| 32 | AuthPairingProofKeyThumbprint | ADD `proof_key_thumbprint` |
| 33 | ProjectionThreadsSettled | ADD `settled_override`, `settled_at` |
| 34 | ProjectionThreadsSnoozed | ADD `snoozed_until`, `snoozed_at` |
| 35 | ProjectionThreadTitleRegeneration | ADD `title_regeneration_request_id`, `title_regeneration_started_at` |
| 36 | ProjectionThreadsPinned | ADD `pinned_at` |
| 37 | ProjectionTurnsKeysetIndex | Index (thread_id, requested_at, turn_id) |
| 38 | ProjectionThreadsPinOrderKey | ADD `pin_order_key` |
| 39 | ProjectionProjectsDefaultThreadEnvMode | ADD `default_thread_env_mode` |
| 40 | ProjectionProjectFaviconPath | ADD `favicon_path` |
| 41 | AuthSessionClientConnection | ADD `client_surface`, `client_app_version` |
| 42 | ProjectionThreadLinkedPullRequest | ADD `linked_pull_request_json` (legacy) |
| 43 | ProjectionThreadsUnsettledAt | ADD `unsettled_at` |
| 44 | ClearAutomaticProjectModelDefaults | Data: nulls auto-seeded `defaultModelSelection` in projections and `project.created` payloads |
| 45 | ProjectionProjectsAutoPull | ADD `auto_pull INTEGER NOT NULL DEFAULT 0` |
| 46 | RepairAutomaticSettlementTimestamps | Data: repairs `settled_at` written by auto-settle (identified by `command_id LIKE 'server:auto-settle:%'`) |
| 47 | ProjectionProjectIcon | ADD `project_icon_json` |
| 48 | ProjectionThreadBranchPullRequest | ADD `branch_pull_request_json` |
| 49 | ProjectionThreadsActiveOrderKey | ADD `active_order_key` |
| 50 | ProjectionThreadPullRequests | CREATE `projection_thread_pull_requests` and index; migrates the legacy `linked_pull_request_json` in JS |
| 51 | ProjectionThreadMessageContext | ADD `context_json` |
| 52 | ProjectionThreadTitleState | ADD `title_state_json` (unguarded ALTER) |
| 53 | PullRequestFilesViewed | CREATE `pull_request_files_viewed` WITHOUT ROWID |
| 54 | ProjectionThreadsAutoSettleDisabledAt | ADD `auto_settle_disabled_at` |

Migration tracking table (created by Effect's SQLite `Migrator`):

```sql
CREATE TABLE IF NOT EXISTS effect_sql_migrations (
  migration_id integer PRIMARY KEY NOT NULL,
  created_at datetime NOT NULL DEFAULT current_timestamp,
  name VARCHAR(255) NOT NULL
);
-- rows (1,'OrchestrationEvents') … (54,'ProjectionThreadsAutoSettleDisabledAt'); names = file names without the "NNN_" prefix
```

Migrations that rewrite data, not just DDL, matter only for a fresh DB replaying 1–54. The consolidated DDL makes them no-ops on an empty DB: 011, 016 (also `DROP COLUMN`), 024, 025, 026, 044, 046, 050 (JS-side migration of `linked_pull_request_json`).

## Appendix C: environment variables

| Variable | Meaning | Keep in Rust? |
|---|---|---|
| `T3CODE_HOME` | base dir (= `--base-dir`) | yes |
| `T3CODE_MODE`, `T3CODE_PORT`, `T3CODE_HOST`, `T3CODE_NO_BROWSER`, `T3CODE_AUTO_BOOTSTRAP_PROJECT_FROM_CWD`, `T3CODE_LOG_WS_EVENTS`, `T3CODE_TAILSCALE_SERVE`, `T3CODE_TAILSCALE_SERVE_PORT`, `T3CODE_BOOTSTRAP_FD` | server flags (`cli/config.ts:124-161`) | yes, except the bootstrap fd |
| `VITE_DEV_SERVER_URL`, `T3CODE_DEV_ALLOWED_ORIGINS`, `T3CODE_DEV_AUTH_TOKEN` | dev mode | dev URL and allowed origins, yes; dev token, optional |
| `T3CODE_LOG_LEVEL`, `T3CODE_TRACE_MIN_LEVEL`, `T3CODE_TRACE_TIMING_ENABLED`, `T3CODE_TRACE_FILE`, `T3CODE_TRACE_MAX_BYTES`, `T3CODE_TRACE_MAX_FILES`, `T3CODE_TRACE_BATCH_WINDOW_MS` | logging and trace file | yes |
| `T3CODE_OTLP_{TRACES,METRICS,LOGS}_URL`, `T3CODE_OTLP_EXPORT_INTERVAL_MS`, `T3CODE_OTLP_HEADERS`, `T3CODE_OTLP_PROTOCOL`, standard `OTEL_*` | OTLP export | optional |
| `T3CODE_TELEMETRY_ENABLED` (default false in zenith), `T3CODE_POSTHOG_KEY`, `T3CODE_POSTHOG_HOST`, … | PostHog | no-op |
| `T3CODE_RESOURCE_MONITOR_PATH` | sidecar override | no (the monitor is linked in) |
| `T3CODE_CODEX_LAUNCH_ARGS` | extra codex args | yes |
| `T3CODE_STRICT_PROVIDER_LIFECYCLE_GUARD` | `"0"` disables a guard in ingestion | yes |
| `T3CODE_BITBUCKET_API_BASE_URL`, `_ACCESS_TOKEN`, `_EMAIL`, `_API_TOKEN` | Bitbucket | yes |
| `T3CODE_RELAY_URL`, `T3CODE_CLERK_*`, `T3CODE_HOSTED_APP_URL`, `T3CODE_RELAY_CLIENT_OTLP_*`, `T3_SERVICE_LAUNCHER_CONTEXT`, `T3_BOOT_SERVICE_UNIT` | T3 Connect / launcher | no |
| `ZENITH_CODE_PARENT_ORIGINS` | allowed parent frames (CSP, embed.json) | **yes** |
| `ZENITH_NO_STARTUP_TOKEN` | `"1"`: `serve` prints only `… server is ready.`, no startup token, pairing URL or QR (zenith's LaunchAgent sets it; zenith.app mints its own tokens with `auth pairing create`) | **yes** |
| written to children: `T3_MCP_BEARER_TOKEN` (codex), `CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `CLAUDE_CODE_ENTRYPOINT=sdk-ts`, `OPENCODE_SERVER_PASSWORD`, `OPENCODE_CONFIG_CONTENT`, `GEMINI_HOME`/API keys (antigravity), `T3CODE_PROJECT_ROOT`/`T3CODE_WORKTREE_PATH` (project scripts), `TERM`/`COLORTERM` (terminals) | | yes |
| stripped from terminals | `T3CODE_*`, `VITE_*`, `PORT`, `ELECTRON_RENDERER_PORT`, `ELECTRON_RUN_AS_NODE` | yes |

## Appendix D: where the research came from

- Everything above was read from this worktree's `code/` (identical to the main checkout's `code/` at the time of writing).
- The Effect runtime behaviour (envelopes, acks, ping, encodings) comes from the installed `effect@4.0.0-rc.115` (patched) in the main checkout's `code/node_modules/.pnpm/`. Encodings were confirmed by running `Schema.toCodecJson` encoders.
- The RPC table was extracted from `rpc.ts` and `RpcAuthorization.ts` by script, and the JSON Schema feasibility check ran over all 444 RPC schemas.
- The DB facts were read from a scratch copy of the live `~/.zenith/code/userdata/state.sqlite`.
- To regenerate Appendix A after an upstream sync, parse `Rpc.make(WS_METHODS.x, {payload, success, error, stream})` in `packages/contracts/src/rpc.ts` and join with `RPC_REQUIRED_SCOPES`. The WP-01 generator should emit this table as a side product.
