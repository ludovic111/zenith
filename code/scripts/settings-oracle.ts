/**
 * The TS side of the zc-settings gate: runs the real settings, keybindings and environment theme
 * services against a base directory and prints one JSON line per operation.
 *
 *   node scripts/settings-oracle.ts <baseDir> <ops.json>
 *
 * The Rust side is `cargo run -p zc-settings --example settings_oracle -- <baseDir> <ops.json>`;
 * run both on two copies of the same user data and diff the lines and the files left on disk
 * (docs/zenith-code/settings.md). Never point either at the live `~/.zenith/code`.
 *
 * `ops.json` is an array of `{"op": …}`: `start`, `file` (`name`: a file of `userdata/`),
 * `getSettings` (redacted for clients), `getSettingsRaw`, `updateSettings` (`patch`),
 * `keybindings`, `upsertKeybinding` / `removeKeybinding` (`input`), `themes`, `decode` /
 * `decodePatch` (`input`: decode then encode `ServerSettings` / `ServerSettingsPatch`).
 * Logs go to stdout too: keep the lines that start with `{"op"`.
 */
import * as Fs from "node:fs";

import * as NodeServices from "@effect/platform-node/NodeServices";
import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as Schema from "effect/Schema";

import * as Contracts from "../packages/contracts/src/index.ts";
import * as ServerSecretStore from "../apps/server/src/auth/ServerSecretStore.ts";
import * as ServerConfig from "../apps/server/src/config.ts";
import * as EnvironmentTheme from "../apps/server/src/environmentTheme.ts";
import * as Keybindings from "../apps/server/src/keybindings.ts";
import * as Sqlite from "../apps/server/src/persistence/Layers/Sqlite.ts";
import * as ServerSettingsModule from "../apps/server/src/serverSettings.ts";

const [baseDir, opsPath] = process.argv.slice(2);
if (!baseDir || !opsPath) {
  console.error("usage: node scripts/settings-oracle.ts <baseDir> <ops.json>");
  process.exit(2);
}
const ops: Array<any> = JSON.parse(Fs.readFileSync(opsPath, "utf8"));

const configLayer = ServerConfig.layerTest(process.cwd(), baseDir);
const sqliteLayer = Layer.unwrap(
  Effect.gen(function* () {
    const { dbPath } = yield* ServerConfig.ServerConfig;
    return Sqlite.makeSqlitePersistenceLive(dbPath);
  }),
);
const layer = Layer.mergeAll(
  ServerSettingsModule.layer.pipe(Layer.provide(ServerSecretStore.layer)),
  Keybindings.layer,
  EnvironmentTheme.layer,
).pipe(
  Layer.provideMerge(sqliteLayer),
  Layer.provideMerge(configLayer),
  Layer.provideMerge(NodeServices.layer),
);

const encodeSettings = Schema.encodeSync(Contracts.ServerSettings);
const decodePatch = Schema.decodeUnknownSync(Contracts.ServerSettingsPatch);
const encodeResolved = Schema.encodeSync(Contracts.ResolvedKeybindingsConfig);
const encodeIssues = Schema.encodeSync(Schema.Array(Contracts.ServerConfigIssue));
const decodeUpsert = Schema.decodeUnknownSync(Contracts.ServerUpsertKeybindingInput);
const decodeRemove = Schema.decodeUnknownSync(Contracts.ServerRemoveKeybindingInput);

const program = Effect.gen(function* () {
  const settings = yield* ServerSettingsModule.ServerSettingsService;
  const keybindings = yield* Keybindings.Keybindings;
  const themes = yield* EnvironmentTheme.EnvironmentThemeService;
  const out: Array<unknown> = [];
  for (const op of ops) {
    const exit = yield* Effect.exit(
      Effect.gen(function* () {
        switch (op.op) {
          case "start":
            yield* settings.start;
            yield* keybindings.start;
            return null;
          case "getSettings":
            return encodeSettings(
              ServerSettingsModule.redactServerSettingsForClient(yield* settings.getSettings),
            );
          case "getSettingsRaw":
            return encodeSettings(yield* settings.getSettings);
          case "updateSettings":
            return encodeSettings(
              ServerSettingsModule.redactServerSettingsForClient(
                yield* settings.updateSettings(decodePatch(op.patch)),
              ),
            );
          case "keybindings": {
            const state = yield* keybindings.loadConfigState;
            return {
              keybindings: encodeResolved(state.keybindings),
              issues: encodeIssues(state.issues as any),
            };
          }
          case "upsertKeybinding":
            return encodeResolved(yield* keybindings.upsertKeybindingRule(decodeUpsert(op.input)));
          case "removeKeybinding":
            return encodeResolved(yield* keybindings.removeKeybindingRule(decodeRemove(op.input)));
          case "decode":
            return encodeSettings(Schema.decodeUnknownSync(Contracts.ServerSettings)(op.input));
          case "decodePatch":
            return Schema.encodeSync(Contracts.ServerSettingsPatch)(decodePatch(op.input));
          case "themes":
            return yield* themes.current;
          case "file": {
            const file = `${baseDir}/userdata/${op.name}`;
            return Fs.existsSync(file) ? Fs.readFileSync(file, "utf8") : null;
          }
          default:
            throw new Error(`unknown op ${op.op}`);
        }
      }),
    );
    if (exit._tag === "Success") {
      out.push({ op: op.op, ok: exit.value });
      continue;
    }
    // A typed failure prints its fields without the cause; anything else is a defect.
    const fail: any = (exit.cause as any).reasons.find((reason: any) => reason._tag === "Fail");
    if (!fail) {
      out.push({ op: op.op, error: "defect" });
      continue;
    }
    const { cause: _cause, ...fields } = fail.error;
    out.push({ op: op.op, error: { _tag: fail.error._tag, ...fields } });
  }
  return out;
});

const result = await Effect.runPromise(
  Effect.scoped(program.pipe(Effect.provide(layer))) as Effect.Effect<Array<unknown>>,
);
for (const line of result) console.log(JSON.stringify(line));
process.exit(0);
