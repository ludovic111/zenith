# zenith code in Rust: verification harness

How we know the Rust server (`crates/zenith-code`) is compatible with the TypeScript one. This is
WP-34 of [the porting plan](../zenith-code-rust-plan.md) (§9). Everything lives in
`code/scripts/compat/`. It is plain Node (≥ 24, which runs `.ts` directly) on the installed
workspace packages. Nothing needs building, and nothing depends on server internals.

All commands run from `code/`. A worktree without `node_modules` can symlink the main checkout's,
read-only: `code/node_modules`, every `apps/*/node_modules`, `packages/*/node_modules` and
`scripts/node_modules`, plus `oxlint-plugin-t3code/node_modules` for linting.

| Tool | What it does |
|---|---|
| `proxy.ts` | Validating, recording proxy between apps/web and a backend |
| `capture.ts` | Starts a backend on a temp home, runs a scripted session through the proxy, writes a recording |
| `replay.ts` | Replays a recording's client side against another backend, records the answers, diffs them |
| `diff.ts` | Compares two recordings request by request, after normalization |
| `validate.ts` | Validates a recording offline, using the proxy's validators |
| `oracle.ts` | Node oracle (§9.1): decodes JSON lines on stdin with the real contracts |
| `blackbox/*.test.ts` | Black-box tests on `ServerUnderTest`, for `BACKEND=ts` or `BACKEND=rust` |
| `auth-interop.ts` | Both backends in turn on one base dir: each accepts the other's tokens, cookies, tickets and pairing credentials |
| `auth-live-copy.ts` | On a temp copy of `~/.zenith/code`: its live sessions, signed with its key the TS way, authenticate on Rust, and a Rust cookie verifies with the TS code |

## The oracle: the contracts themselves

`lib/contracts.ts` loads `packages/contracts/src` from the worktree, so it always checks against
the contracts at HEAD. It decodes the way the web client does:

- `WsRpcGroup` gives the tag → RPC table. A request payload decodes with `payloadSchema`.
- `Chunk.values` decode with `NonEmptyArray(stream success schema)`.
- `Exit` decodes with `Rpc.exitSchema(rpc)`.
- Every codec is `Schema.toCodecJson`, exactly like `RpcSerialization.layerJson` and HttpApi.
- `EnvironmentHttpApi` is walked with `HttpApi.reflect`: method and path pattern, payload codecs by
  content type, and success and error codecs by status. Middleware errors are included.
- Issues are formatted with `SchemaIssue.makeFormatterDefault()`.

**Strict mode** (`--strict`) also re-encodes every decoded value and compares it with the wire
JSON. That catches what the client tolerates but TS never sends, such as extra keys,
non-canonical numbers, or untrimmed strings. The TS server produces **zero** such warnings on
every recording, including the live-data run, so strict mode is a fair bar for Rust.

## 1. The validating, recording proxy

```sh
node scripts/compat/proxy.ts --target http://127.0.0.1:<backend port> --port 4790 \
  [--record session.jsonl] [--no-redact] [--strict] [--static <built web client dir>]
```

Open `http://127.0.0.1:4790/` and pair with `/pair#token=<Token from the backend banner>`. The
proxy forwards everything unchanged: status, headers, `Set-Cookie`, compressed bodies, and the
WebSocket upgrade. The upgrade is only accepted once the backend has accepted it; a backend 401
reaches the client as is. The original `Host` header is kept, and each side negotiates
permessage-deflate on its own.

What it validates:

- **Every server→client `/ws` frame.**
  - Errors: a `Chunk` or `Exit` that does not decode, a `Defect` frame, a `Chunk` for a unary
    RPC, a `Chunk` after `Exit`, a second `Exit`, a stream `Exit` whose success value is not
    `null`, an unknown request id, and a `requestId` echoed with another JSON type than the
    request's id.
  - Warnings: `Exit` with a `Die` cause, a batched array frame.
  - Client payloads that do not decode are also flagged as warnings.
- **Every API HTTP response** (`/api`, `/oauth`, `/.well-known`, `/zenith/embed.json`): the body
  must decode with the endpoint's schema for that status. A status the endpoint does not declare
  is a warning, or an error when it is 5xx.

Issues print live. Ctrl-C prints a summary of traffic per endpoint and tag, plus issue counts, and
exits 1 if there was any error. **The bar for the cut-over is zero errors and zero `Defect`
frames.**

`--static <dir>` serves a built web client for non-API GETs, so the proxy can front a backend that
does not serve the SPA yet. The main checkout's `code/apps/server/dist/client` works.

Recording format, one JSON object per line:

```text
{"conn":"ws1","dir":"open|c2s|s2c|close|issue|meta","t":<ms>,"frame":…}
{"conn":"http3","dir":"c2s","t":5,"frame":{"method","url","headers","body"}}
{"conn":"http3","dir":"s2c","t":7,"frame":{"status","headers","body"}}
```

Redaction is on by default (`--no-redact` turns it off). Cookie values, `authorization`, `dpop`,
`?wsTicket=`, `#token=` and JSON keys such as `credential`, `access_token`, `ticket` and `token`
become `<redacted>`.

## 2. Record, replay, diff

```sh
# record against TS (synthetic home; see "Recordings" below)
node scripts/compat/capture.ts --scenario web-session --out scripts/compat/recordings/web-session.jsonl

# replay against a fresh backend, validate its answers, and diff them against the recording
BACKEND=rust node scripts/compat/replay.ts --recording scripts/compat/recordings/web-session.jsonl \
  --out /tmp/web-session.rust.jsonl --diff

# or diff any two recordings
node scripts/compat/diff.ts left.jsonl right.jsonl [--sequences ordinal|rebase|exact] \
  [--ignore-keys <re>] [--ignore-paths <re>] [--keep-volatile] [--json]
```

**The replayer** (`lib/replay.ts`):

- **Setup.** It starts the backend the way the capture did: an empty home, an empty `HOME`,
  providers off, and the deterministic git repo, whose path is substituted in every client frame.
  `--target <url> --credential <token>` replays against a backend you started yourself.
- **Frames.** It re-sends the recorded Requests and Interrupts with the same ids. It drops the
  recorded Acks and Pings and Acks every Chunk itself.
- **Causal waits.** Before each client event it waits, bounded by `--causal-timeout`, until the
  backend has produced as many stream values and Exits as the recording had at that point.
- **Substitution.** It learns server-generated values by walking each recorded answer alongside the
  replayed one: pairing credentials, cookies, bearer tokens, ws tickets and ids. It then rewrites
  later client frames with them, and maps the banner credential through the recording's meta.

**The differ** (`lib/diff.ts`, `lib/normalize.ts`):

- **Keys.** Each recording is reduced to request views:
  - `ws1 <tag>#<n>`: stream values flattened across Chunks, plus the Exit;
  - `HTTP <METHOD> <endpoint>#<n>`: status, body and the headers that matter (content type,
    cache-control, CORS, CSP, set-cookie).
- **Normalization**, one normalizer per side, fed in event order:
  - uuids become `<uuid:N>` by first-seen order, also inside strings;
  - ISO and HTTP dates become placeholders, and so do epoch-ms fields;
  - secrets and cookie values, ports, `t3_session_<port>_<hash>`, and signed asset and upload URLs
    become placeholders;
  - path aliases from the recording meta become `<home>`, `<workspace>` and `<user-home>`;
  - trace, span and fiber ids, pids and durations are blanked;
  - sequences become `<seq:N>` by first-seen order. `--sequences exact` compares them literally,
    which works when both sides start from an empty DB, and is how the recordings here compare.
- **Long-lived streams** that never ended are compared on their common prefix, with a note.

**Baseline.** A capture against TS replayed against a fresh TS diffs clean:

| Recording | Result |
|---|---|
| web-session | 40/40 same |
| auth | 20/20 same |
| web-browser | 39/39 same |

So any difference against Rust is a real divergence, not noise.

## 3. Black-box runner

```sh
node --test --test-reporter=spec scripts/compat/blackbox/*.test.ts            # BACKEND=ts
BACKEND=rust node --test scripts/compat/blackbox/*.test.ts                     # once it serves
COMPAT_PROXY=0 …    # talk to the backend directly instead of through the validating proxy
COMPAT_ECHO=1 …     # show the server's output
COMPAT_WARNINGS=1 … # print the proxy's warnings too
```

`lib/serverUnderTest.ts` is `ServerUnderTest.start({backend, homeDir, env, settings, isolateHome, args})`
→ `{httpUrl, wsUrl, port, homeDir, bootstrapCredential, cli(args), stop()}`. It spawns
`<cmd> serve --host 127.0.0.1 --port <free> … --base-dir <home>`, which is exactly what the
dashboard runs. It waits for the banner's `Token: <credential>` line and for
`/.well-known/t3/environment`.

| Backend | Command | Override |
|---|---|---|
| `BACKEND=ts` (default) | `node code/apps/server/dist/bin.mjs` if built in this checkout, else `node code/apps/server/src/bin.ts` (Node runs the TS source) | `ZENITH_CODE_TS_ENTRY` |
| `BACKEND=rust` | `<repo>/target/debug/zenith-code` | `ZENITH_CODE_BIN` |

`cli(args)` runs `<cmd> <args> --base-dir <home>`, which tests the CLI output contract (§6.17).
`stop()` sends SIGTERM and reports how long the exit took. Each `describe` block gets one backend
(about 1.3 s to start from source).

The harness (`blackbox/harness.ts`):

- It is **hermetic by default**: an empty `HOME`, and settings with every provider driver
  disabled (`SYNTHETIC_SETTINGS`), so no provider CLI is probed. A logged-in `claude` answers from
  the keychain even with an empty `HOME`, which is why providers are turned off as well.
- By default every request goes through the validating proxy, and a block fails if the proxy saw
  an error. The suite therefore doubles as a conformance run.
- Fresh credentials come from the HTTP API (`newCredential`, `newSessionCookie`,
  `newBearerToken`), so tests stay independent of each other even though `serve`'s pairing tokens
  are one-time.

**Ported so far: 69 tests, all green against TS (about 30 s). Against Rust (`zenith-code serve`,
2026-10-01): 68 of 69; the one failure is permessage-deflate, which tungstenite 0.29 cannot negotiate.**

- **`auth.test.ts`** (28 tests), from `server.test.ts`:
  - session state, cookie name and attributes;
  - bootstrap, one-time credentials, token exchange and ws tickets;
  - CORS on responses and preflights (including OTLP);
  - 401, 403 and 404 error shapes;
  - pairing links: create, list and revoke;
  - client sessions: list, revoke one, revoke others, and user-agent metadata;
  - access read/write scope separation.
- **`ws.test.ts`** (16 tests):
  - handshake auth: cookie, ticket, bearer, rejection of a token in the query string, 401;
  - permessage-deflate;
  - per-RPC scopes through the real Effect RPC client;
  - envelope rules from §1.3: Ping/Pong, unknown tag and bad payload give a per-request `Exit(Die)`
    and never a `Defect`, string ids echo as strings, `subscribeServerConfig` never ends,
    Interrupt;
  - `subscribeAuthAccess` leaks no credential.
- **`dpop.test.ts`** (8 tests), from `server.test.ts:2499-2766`:
  - token exchange with a DPoP proof, no bearer downgrade, the proof-bound token on
    `/api/auth/session` and `/api/auth/websocket-ticket` (missing proof, other key);
  - future-dated and stale proofs (`time_window`), replay across exchanges, forwarded hosts
    ignored and spoofed ones refused (`request_mismatch`), malformed proofs;
  - every 401 carries `www-authenticate: DPoP`.
- **`server.test.ts`** (9 tests):
  - descriptor and `environment-id`;
  - `embed.json`: origin filtering, defaults, `no-store`, no CSP;
  - `server-runtime.json` written while running, deleted on SIGTERM;
  - the exit takes under 5 s, and the environment id is stable across restarts;
  - CLI `auth pairing create --json`, `auth session issue --json`, `auth pairing revoke`;
  - a real temp project: `project.create` and `thread.create` over HTTP dispatch, the shell over
    HTTP and WS, the thread snapshot, `subscribeThread`, `afterSequence` resume, and the duplicate
    root error.

The originals run in desktop mode with a reusable token. Here the backend runs like zenith runs it:
`serve` on loopback, policy `loopback-browser`, one-time tokens. Expectations that depend on the
mode were adapted, and each test names the `server.test.ts` line it comes from.

- **`runtime.test.ts`** (8 tests), the runtime around the feature crates:
  - `subscribeServerLifecycle` replays `welcome` then `ready`, both carrying the descriptor;
  - `server.getConfig` assembles environment, auth, providers, editors, keybindings and settings;
  - `server.probe`, keybinding upsert/remove (file and `keybindingsUpdated`), the relay client
    status and device state, `GET /api/connect/link-state` (401, then unlinked);
  - `project add|rename|remove` through the running server (the shell sees it live; duplicate root
    and unknown project are errors), and offline with a stale `server-runtime.json` that gets
    removed.

**Next batches to port**, in the triage order of §9.3:

- the cloud/relay stubs (`server.test.ts:2771-4453`);
- the static SPA tests: run the backend with a client dir;
- the OTLP proxying;
- `projects.*` / `vcs.*` / `git.*` on real temp repos;
- terminal tests, with a fake `$SHELL`;
- provider tests, with the process-level fakes (`src/testUtils/fakeCli.ts`,
  `codexCollabMockPeer.mjs`).

**Two findings already**: the TS server differs from the plan.

1. `POST /api/auth/pairing-token` sends no `cache-control` header. Only `browser-session` does.
2. A duplicate `project.create` over HTTP dispatch is a **500** `EnvironmentInternalError`
   `orchestration_dispatch_failed`, not a 400.

The tests pin what TS does.

## 4. Recordings

`code/scripts/compat/recordings/` holds sessions recorded against the TS server. They are
synthetic: an empty home, an empty `HOME`, providers off, and a deterministic git repo (fixed
author and dates, so the commit hashes are stable). Client ids and timestamps are fixed too. Their
tokens belonged to a temp server that no longer exists, so they are stored unredacted, which the
replayer needs. The machine's home path, user name, host name and machine label, and any e-mail
address, are scrubbed when the line is written (`machineScrubs`).

| File | What | How |
|---|---|---|
| `web-session.jsonl` | **Startup:** descriptor, session, browser-session pairing, `embed.json`, `subscribeServerConfig`, `subscribeServerLifecycle`, `subscribeAuthAccess`, `subscribeShell`, `subscribeBackgroundPolicy`, `probe`, `getConfig`, `getSettings`, background policy, relay status, archived shell. **Project and thread:** create both, HTTP shell and thread snapshot, `subscribeThread` with `afterSequence`, worktree setup, `preview.list`. **Workspace and VCS:** `projects.listEntries`, `searchEntries` and `readFile` (plus a missing file), `vcs.refreshStatus`, `subscribeVcsStatus`, `vcs.listRefs`. **Edits:** rename, `searchThreads`, keybinding upsert and remove. **Errors:** unknown tag, bad payload, unknown thread 404, duplicate project. | `capture.ts --scenario web-session` |
| `auth.jsonl` | One-time reuse 401; unauthenticated 401; pairing tokens (labelled, empty scopes 400, `access:read`); pairing links; token exchange; 403 scope; ws ticket socket with `subscribeAuthAccess` and an RPC scope error; revoke a link; revoked bootstrap; clients; revoke-others; CORS preflight | `capture.ts --scenario auth` |
| `web-browser.jsonl` | **The real apps/web in a browser** (the built client served by the proxy). It pairs, loads the shell, opens the seeded thread, opens Settings, and reloads the thread. This is the client's genuine RPC sequence: `subscribeProjectClones`, `subscribeTerminalMetadata`, `subscribeDeviceState`, `assets.createUrl`, `server.discoverSourceControl`, `vcs.listRefs`, `server.reportClientActivity`, … | `capture.ts --scenario interactive --static <client dir>` |

`validate.ts --strict` on all three: 0 errors. The only warnings are the 3 error-shape probes of
web-session, which are deliberate.

### Live-data validation (not committed)

```sh
node scripts/compat/capture.ts --scenario live-read-only --home live --strict --out /tmp/live.jsonl
```

This copies `~/.zenith/code` into a temp dir and only reads the source: the database, the WAL,
settings, keybindings, environment-id and themes. It does **not** copy secrets or logs. It then
makes the copy safe:

- `continueThreadsAfterServerUpdate` is off;
- `defaultAutoPull` is off, globally and per project;
- `projection_projects.auto_pull` is set to 0.

So startup neither resumes provider turns nor pulls real repositories. The scenario is read-only:

- startup, then both snapshots;
- the 8 most recently updated threads, each opened both ways: the HTTP snapshot plus
  `subscribeThread` resume, and a full `subscribeThread`.

It never dispatches, never touches VCS, and never sends a turn. `capture.ts` refuses to write such
a recording under `recordings/`.

Result on 2026-10-01: 15 HTTP exchanges, 70 server frames (16 `subscribeThread`), **0 errors and 0
warnings in strict mode**.

## 5. The Node oracle for Rust tests

```sh
printf '%s\n' '{"tag":"server.getConfig","kind":"success","json":{…}}' \
              '{"tag":"orchestration.subscribeShell","kind":"chunk","json":{"kind":"synchronized"}}' \
              '{"http":"GET /api/auth/session","status":200,"json":{…}}' | node scripts/compat/oracle.ts
```

| `kind` | Meaning |
|---|---|
| `payload` | a request payload |
| `success` | a unary success value |
| `error` | a typed failure |
| `exit` | a whole Exit |
| `chunk` | one stream item |

HTTP lines take `http: "<METHOD> <path>"` plus `status`, or `kind: "payload"`. The output is one
`{"line","ok","message"?}` per input line, and the exit code is 1 on any failure. Rust tests
serialize their fixtures as such lines and pipe them through it (§9.1). The recordings above are
also a corpus of TS-encoded values to deserialize in Rust.

## Plugging in the Rust backend

The harness needs only what the dashboard needs. Once `zenith-code serve` can do all of this, run
the same tools with `BACKEND=rust`:

1. accept `serve --host 127.0.0.1 --port <p> --base-dir <home>`;
2. print the banner lines `T3 Code server is ready.`, `Connection string: …`, `Token: <credential>`
   and `Pairing URL: …/pair#token=…`;
3. answer `/.well-known/t3/environment`;
4. exit on SIGTERM.

Then:

- `BACKEND=rust node --test scripts/compat/blackbox/*.test.ts`: the black-box suite, with schema
  validation of every answer.
- `BACKEND=rust node scripts/compat/replay.ts --recording <rec> --out <out> --diff` for each
  recording: wire diffs against TS, plus validation.
- `node scripts/compat/proxy.ts --target <rust server> --static <client dir> --strict` while
  dogfooding: live validation behind the real web app.

Until then, `BACKEND=rust` fails fast with "Rust backend not built". In CI, run the black-box suite
and the replays for both backends; any divergence is the signal.
