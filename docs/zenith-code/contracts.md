# zenith code in Rust: the wire contracts (`zc-contracts`)

`crates/zenith-code/crates/zc-contracts` holds serde types for every schema of
`code/packages/contracts`, the table of the 148 WebSocket RPC methods and the table of the 24
typed HTTP endpoints. It is **generated** from the Effect schemas (plan §7.2, WP-01) and only
depends on `serde`, `serde_json`, `jiff` and `base64`.

Every type models the **encoded** side of its schema: what `Schema.toCodecJson` writes and
reads. Serialize a value with `serde_json` and the web client decodes it; deserialize what the
client sends and you get the same data the TS server would.

## Regenerating

```sh
cd code
node scripts/gen-rust-contracts.ts                       # code + sampled/curated fixtures
node scripts/gen-rust-contracts.ts --harvest <db copy>   # + real payloads (see below)
cargo test -p zc-contracts                               # round-trip tests
cargo test -p zc-contracts -- --ignored oracle           # Rust-encoded values through TS
```

The generator needs `code/node_modules` (it imports the contracts and `effect` 4.0.0-rc.115)
and `rustfmt`. Its output is deterministic (fixed seeds, declaration order), so regenerating
without a contract change is a no-op: run it at every upstream sync and commit the result.
`fixtures/generation-report.json` lists the counts, the schemas the sampler could not cover,
and the legacy shapes Rust does not read.

Files:

| Path | What |
|---|---|
| `code/scripts/gen-rust-contracts.ts` | the generator |
| `code/scripts/contracts-oracle.ts` | the TS oracle: JSON lines `{schema, value}` on stdin → `{ok, issue?}` per line |
| `code/scripts/contracts-curated.json` | hand-written edge cases (TS validates and canonicalizes them) |
| `src/prim.rs` | hand-written primitives (the only hand-written code besides `lib.rs`, `tests.rs`) |
| `src/generated/<module>.rs` | one file per contracts module (`orchestration.ts` → `orchestration.rs`) |
| `src/generated/lits.rs` | single-literal unit types (`LitThreadCreated`, `Lit1`, …) |
| `src/generated/rpc_methods.rs`, `http_endpoints.rs` | the method and endpoint tables |
| `src/generated/registry.rs` | round-trip by schema id (tests, or feature `registry`) |
| `fixtures/samples.jsonl` | ~3 sampled values per schema id, encoded by TS |
| `fixtures/curated.jsonl` | the curated cases, encoded by TS |
| `fixtures/harvested/` | real payloads, **git-ignored** (personal data) |

## Using it

```rust
use zc_contracts::{methods, Rpc, RpcKind, RpcMethod, METHODS};

// dispatch on the wire tag
let rpc = Rpc::from_tag("orchestration.subscribeShell").unwrap();
assert_eq!(rpc.spec().kind, RpcKind::Stream);
let scope = rpc.spec().scope; // AuthEnvironmentScope::OrchestrationRead

// type-checked handlers
fn handle<M: RpcMethod>(payload: M::Payload) -> Result<M::Success, M::Error> { todo!() }
type P = <methods::OrchestrationDispatchCommand as RpcMethod>::Payload; // ClientOrchestrationCommand
```

- `Rpc` (enum, one variant per method, `Rpc::ALL`), `METHODS[rpc as usize]` (`MethodSpec`:
  tag, kind, scope, Rust type names), `methods::<Method>` marker types implementing
  `RpcMethod` (`Payload`, `Success` = one stream item for streams, `Error`; `Never` when the
  method cannot fail), and `zc_for_each_rpc!(my_macro)` which expands
  `my_macro! { (Marker, "tag", Unary|Stream), … }` for generating dispatch code.
- Method names: the tag in PascalCase (`server.upsertKeybinding` → `ServerUpsertKeybinding`,
  `provider.chatgpt.reconnect-profile` → `ProviderChatgptReconnectProfile`).
- `device.list` needs `orchestration:operate` when `retryHostId` or `updateTool` is set; the
  table only has its base scope.
- `ENDPOINTS` / `endpoints::<GroupEndpoint>` (`HttpEndpoint`: `Params`, `Payload`, `Success`,
  `Error`; `EndpointSpec` has method, path, auth, payload encoding and the status of each body).
- The RPC envelope (`Request`, `Chunk`, `Exit`, causes, defects) is not here: that is `zc-rpc`.

Naming: types keep the export names of the contracts. Anonymous structs and unions are named
after their parent and field (`ServerConfigSettings`), members of tagged unions after their tag
(`OrchestrationEventThreadCreated`), unions of named members after them
(`ProjectIdOrThreadId`), RPC inline schemas after the method (`ServerProbePayload`,
`ServerRefreshProvidersPayload`), HTTP ones after the endpoint (`OrchestrationThreadSnapshotQuery`).
Fields are snake_case with `#[serde(rename)]`. `ThreadId`-like brands are `String` newtypes
(`ThreadId::new`, `as_str`, `From<&str>`, `Display`).

## Mapping

| Effect schema (wire rule, plan §1.5) | Rust |
|---|---|
| `Struct` | struct, fields in declaration order (extra keys ignored, like TS) |
| `optionalKey(X)` | `Option<T>`, `default`, `skip_serializing_if = "Option::is_none"` |
| `optional(X)` (null accepted) | same; `null` reads as `None` |
| `NullOr(X)`, key required | `Option<T>` with `deserialize_with = "prim::nullable"` (a missing key is an error) |
| `optionalKey(NullOr(X))`, `optional(NullOr(X))` | `Option<Option<T>>` with `prim::double_option`: absent / `null` / value |
| `withDecodingDefault(…)` | `T` with a generated `default = …` (the TS default, encoded) and `null` → default; always written |
| optional `Unknown` | `Option<serde_json::Value>`; a present `null` is `Some(Value::Null)` |
| `Number` unrefined, `Finite` | `JsNumber` (`"NaN"`, `"Infinity"`, `"-Infinity"`; integral values without fraction) |
| `Int` (and refinements of it) | `i64` |
| `String`, `TemplateLiteral`, `TrimmedString`, refined strings | `String` (aliases keep the contract name) |
| branded `String` / `Int` | newtype (`string_newtype!`, `int_newtype!`) |
| `Literal` in a field | unit type from `lits.rs` (validates on decode) |
| union of string literals | `enum` with `#[serde(rename)]`, `as_str()`, `ALL` |
| union of structs with a common literal key (`_tag`, `type`, `kind`, …) | enum of newtype variants, generated `Deserialize` dispatching on the key |
| other unions | enum, members tried in TS order (with literal/kind pre-checks) |
| `DateTimeUtc` | `DateTimeUtc` (ms since epoch, `toISOString` format, the whole `Date` range) |
| `Option(X)` | `EOption<T>` (`{"_tag":"Some","value"}` / `{"_tag":"None"}`) |
| `OptionFromNullOr(X)` | `Option<T>` |
| `Uint8Array` | `Base64Bytes` |
| `Unknown`, `Any`, `Defect`, `Json` | `serde_json::Value` |
| `Array(X)` | `Vec<T>`; tuples → tuples |
| `Record(String, X)` | `BTreeMap<K, V>`; struct + index signature → `#[serde(flatten)] rest` |
| `Suspend` | `Box<T>` |
| `Class`, `TaggedError`, `TaggedStruct` | struct of the encoded fields, `_tag` as a literal field |
| `Void` (RPC success) | `()` (`null`) |
| `Never` (errors of methods that cannot fail) | `Never` (uninhabited) |
| `ForwardCompatibleArray(X)` | `LenientVec<T>`: elements that do not decode are dropped |
| `ForwardCompatibleOptional(X)` / `ForwardCompatibleNullable(X)` | `Option<T>` with `prim::lenient_option` (unknown → absent / null) |
| `OmittedWhenNull(X)` | `Option<T>` (absent ⇔ null), lenient |

A named union with a `null` member (e.g. `WorktreeCleanup`) is the non-null enum; fields and
the registry wrap it in `Option`.

## Tests (the oracle gate)

- `fixtures/samples.jsonl`: values sampled with Effect's `Arbitrary` from **every** schema id
  (1,574: all exports, payload/success/error of the 148 RPCs, params/payload/success/error of
  the 24 endpoints) and encoded by TS. When whole-value sampling fails (unknown fields, strict
  refinements) the generator composes the value field by field and lets TS validate it.
- `fixtures/curated.jsonl`: absent vs null patches, decoding defaults (`DEFAULT_SERVER_SETTINGS`),
  NaN strings, offset dates and the `Date` range edge, Effect `Option`, trimmed strings, `Void`.
- `fixtures/harvested/` (git-ignored, from `--harvest <copy of state.sqlite>`): every
  `orchestration_events` row (both the TS wire encoding and the row as stored: Rust reads the
  DB payloads as they are) and the `CANON:` provider runtime events of `logs/provider`.
  Never point `--harvest` at the live database: copy it (`state.sqlite` + `-wal`) first.

Each fixture is decoded into its Rust type and encoded back; the result must equal the fixture
after normalization: object key order is ignored and numbers compare by value. The ignored
`oracle` test then pipes every Rust re-encoding through `contracts-oracle.ts`: zero issues.
Status at generation: 4,949 sampled + 17 curated fixtures, 8,084 events (+ 8,084 raw rows) and
4,063 provider events harvested, all passing both directions.

## Known deviations

1. **Strictness.** Refinements (`isNonEmpty`, lengths, patterns, ranges, `isInt` beyond `i64`,
   `Finite` rejecting NaN) are not checked by Rust, and `optionalKey(X)` accepts an explicit
   `null` as absent (TS rejects it). Rust is more lenient, never stricter, except for 3.
2. **Trimming.** `TrimmedString` is a plain `String`: TS trims on decode *and* encode, so a
   Rust-emitted untrimmed string is trimmed by the client; trim what you store yourself.
3. **Legacy decode shapes.** A few schemas decode older shapes in TS through a transformation;
   Rust reads the canonical (encoded) shape only: `ModelSelection` (and its copy in
   `ProjectSettingsOverrides`) and the record form of `ProviderOptionSelections` (listed in
   `generation-report.json`). Every `orchestration_events` row of the live database was already
   in the canonical shape. (Union members with their own encoding, like the boolean form of
   `ClientSettingsSchema.confirmQuit`, are read.) `ProjectIconOverride` is the exception the
   generator models on its **encoded** side (`ENCODED_SIDE_SCHEMAS`): TS encodes a monogram as
   `{kind: "lucide", name: "folder-code", color, monogramText}` for older peers, so the Rust
   `ProjectIconOverrideLucide` carries `monogramText`/`monogram`; read the monogram with
   `zc_orchestration::support::project_icon_monogram` and write icons through
   `canonical_project_icon` (what TS's decode-then-encode produces).
4. **Present-but-undefined keys.** TS encodes `{k: undefined}` for `Schema.optional` keys as
   `"k": null`; Rust does not distinguish it from an absent key (`None`, not written). TS
   decodes both to `undefined`.
5. **Lone UTF-16 surrogates** cannot be represented in Rust strings (serde_json rejects them).
6. **Dates.** `DateTimeUtc` accepts ISO dates and date-times (`Z`, offset, or none = UTC);
   JavaScript's `new Date(string)` accepts more formats and reads offset-less times as local.
7. **Records** are `BTreeMap`s: keys are written sorted, not in insertion order (cosmetic).
8. **Structurally identical union members** (`ProjectIdOrThreadId`) decode as the first one,
   like TS; the wire is the same.
