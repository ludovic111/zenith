// @effect-diagnostics nodeBuiltinImport:off globalDate:off - a standalone Node oracle script.
/**
 * The TypeScript `UsageService` as an oracle for the Rust port (`zc-usage`).
 *
 *   node apps/server/scripts/usage-oracle.ts <config.json>
 *
 * The config names a base directory (the state directory is `<baseDir>/userdata`, where a
 * pinned `usage-model-rates.json` may already sit), the server settings, the host process
 * environment and platform, and the `UsageSummaryInput`s to read in order with one service
 * instance. The rate fetch always fails, so pricing comes from the pinned file only. Prints
 * `{results: [{summary, elapsedMs}]}` (summaries wire-encoded).
 *
 * The Rust side is `cargo test -p zc-usage --test golden -- --ignored`, which builds the
 * fixtures, runs this script and the Rust service on the same read-only inputs, and compares.
 */
import * as NodeFs from "node:fs";

import * as NodeServices from "@effect/platform-node/NodeServices";
import { HostProcessEnvironment, HostProcessPlatform } from "@t3tools/shared/hostProcess";
import { UsageSummary, type UsageSummaryInput } from "@t3tools/contracts";
import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as Schema from "effect/Schema";
import { HttpClient, HttpClientResponse } from "effect/unstable/http";

import * as ServerConfig from "../src/config.ts";
import * as ServerSettings from "../src/serverSettings.ts";
import * as UsageService from "../src/usage/UsageService.ts";

interface OracleConfig {
  readonly baseDir: string;
  readonly cwd: string;
  readonly settings: Parameters<typeof ServerSettings.layerTest>[0];
  readonly environment: Record<string, string>;
  readonly platform: NodeJS.Platform;
  readonly inputs: ReadonlyArray<UsageSummaryInput>;
}

const encodeSummary = Schema.encodeSync(UsageSummary);

const config = JSON.parse(NodeFs.readFileSync(process.argv[2]!, "utf8")) as OracleConfig;

const layers = ServerConfig.layerTest(config.cwd, config.baseDir).pipe(
  Layer.provideMerge(NodeServices.layer),
  Layer.provideMerge(Layer.succeed(HostProcessPlatform, config.platform)),
  Layer.provideMerge(ServerSettings.layerTest(config.settings)),
  Layer.provideMerge(
    Layer.succeed(
      HttpClient.HttpClient,
      HttpClient.make((request) =>
        Effect.succeed(HttpClientResponse.fromWeb(request, new Response("offline", { status: 503 }))),
      ),
    ),
  ),
  Layer.provideMerge(Layer.succeed(HostProcessEnvironment, config.environment)),
);

const program = Effect.gen(function* () {
  const service = yield* UsageService.make;
  const results: Array<{ summary: unknown; elapsedMs: number }> = [];
  for (const input of config.inputs) {
    const started = performance.now();
    const summary = yield* service.readSummary(input);
    results.push({ summary: encodeSummary(summary), elapsedMs: performance.now() - started });
  }
  return results;
}).pipe(Effect.provide(layers), Effect.scoped);

const results = await Effect.runPromise(program);
process.stdout.write(JSON.stringify({ results }));
