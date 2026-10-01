/**
 * The TypeScript orchestration core as an oracle for the Rust port (`zc-orchestration`).
 *
 *   node apps/server/scripts/orchestration-oracle.ts project <state.sqlite> > read-model.json
 *   node apps/server/scripts/orchestration-oracle.ts dispatch <empty dir> [--now <iso>] --out events.json < commands.jsonl
 *
 * `project` reads every `orchestration_events` row of a database (read-only), decodes it the
 * way `OrchestrationEventStore` does, folds the events through `projector.ts` from an empty
 * command read model, and prints the model encoded with the wire codec.
 *
 * `dispatch` starts the real engine (`OrchestrationEngineLive` with the SQL projection
 * pipeline and snapshot query) on `<dir>/state.sqlite`, dispatches each command line
 * (`{"command": OrchestrationCommand, "origin"?: OrchestrationClientOrigin}`) in order, and
 * prints `{results, events}`: each dispatch's `{sequence}` or `{error: {_tag, message}}`, then
 * every stored event row (with `streamVersion` and `actorKind`). `--now` pins the clock
 * (`TestClock`) so the decider's timestamps are reproducible.
 *
 * Never point it at a live database: copy it first.
 */
import * as NodeFs from "node:fs";
import * as NodeSqlite from "node:sqlite";
import * as Readline from "node:readline";

import * as NodeServices from "@effect/platform-node/NodeServices";
import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as ManagedRuntime from "effect/ManagedRuntime";
import * as Cause from "effect/Cause";
import * as Schema from "effect/Schema";
import * as TestClock from "effect/testing/TestClock";

import {
  OrchestrationClientOrigin,
  OrchestrationCommand,
  OrchestrationEvent,
  OrchestrationReadModel,
} from "@t3tools/contracts";
import { createEmptyReadModel, projectEvent } from "../src/orchestration/projector.ts";

const EPOCH = "1970-01-01T00:00:00.000Z";

/** `--out <file>`, else stdout (the engine's logger also writes to stdout). */
function writeOutput(text: string) {
  const outFlag = process.argv.indexOf("--out");
  if (outFlag === -1) {
    process.stdout.write(text);
  } else {
    NodeFs.writeFileSync(process.argv[outFlag + 1]!, text);
  }
}

function readEventRows(dbPath: string): Array<Record<string, unknown>> {
  const db = new NodeSqlite.DatabaseSync(dbPath, { readOnly: true });
  try {
    return db
      .prepare(
        `SELECT sequence, event_id AS eventId, event_type AS type, aggregate_kind AS aggregateKind,
                stream_id AS aggregateId, occurred_at AS occurredAt, command_id AS commandId,
                causation_event_id AS causationEventId, correlation_id AS correlationId,
                payload_json AS payload, metadata_json AS metadata, stream_version AS streamVersion,
                actor_kind AS actorKind
         FROM orchestration_events ORDER BY sequence ASC`,
      )
      .all() as Array<Record<string, unknown>>;
  } finally {
    db.close();
  }
}

async function project(dbPath: string) {
  const decodeEvent = Schema.decodeUnknownSync(OrchestrationEvent);
  let model = createEmptyReadModel(EPOCH);
  for (const row of readEventRows(dbPath)) {
    const event = decodeEvent({
      ...row,
      payload: JSON.parse(row.payload as string),
      metadata: JSON.parse(row.metadata as string),
      streamVersion: undefined,
      actorKind: undefined,
    });
    model = await Effect.runPromise(projectEvent(model, event));
  }
  const encoded = Schema.encodeSync(Schema.toCodecJson(OrchestrationReadModel))(model);
  writeOutput(JSON.stringify(encoded));
}

async function dispatch(dir: string, now: string | undefined) {
  const { OrchestrationEngineLive } = await import(
    "../src/orchestration/Layers/OrchestrationEngine.ts"
  );
  const { OrchestrationProjectionPipelineLive } = await import(
    "../src/orchestration/Layers/ProjectionPipeline.ts"
  );
  const { OrchestrationProjectionSnapshotQueryLive } = await import(
    "../src/orchestration/Layers/ProjectionSnapshotQuery.ts"
  );
  const { OrchestrationEventStoreLive } = await import(
    "../src/persistence/Layers/OrchestrationEventStore.ts"
  );
  const { OrchestrationCommandReceiptRepositoryLive } = await import(
    "../src/persistence/Layers/OrchestrationCommandReceipts.ts"
  );
  const { makeSqlitePersistenceLive } = await import(
    "../src/persistence/Layers/Sqlite.ts"
  );
  const RepositoryIdentityResolver = await import(
    "../src/project/RepositoryIdentityResolver.ts"
  );
  const ThreadBackgroundLiveness = await import(
    "../src/orchestration/ThreadBackgroundLiveness.ts"
  );
  const ThreadPlanProgress = await import("../src/orchestration/ThreadPlanProgress.ts");
  const { ServerConfig } = await import("../src/config.ts");
  const { OrchestrationEngineService } = await import(
    "../src/orchestration/Services/OrchestrationEngine.ts"
  );

  const dbPath = `${dir}/state.sqlite`;
  const layer = Layer.mergeAll(
    OrchestrationEngineLive.pipe(
      Layer.provide(OrchestrationProjectionSnapshotQueryLive),
      Layer.provide(OrchestrationProjectionPipelineLive),
    ),
    OrchestrationProjectionSnapshotQueryLive,
  ).pipe(
    Layer.provideMerge(ThreadBackgroundLiveness.layer),
    Layer.provide(ThreadPlanProgress.layer),
    Layer.provide(OrchestrationEventStoreLive),
    Layer.provideMerge(OrchestrationCommandReceiptRepositoryLive),
    Layer.provide(RepositoryIdentityResolver.layer),
    Layer.provide(makeSqlitePersistenceLive(dbPath)),
    Layer.provideMerge(ServerConfig.layerTest(dir, { prefix: "zc-orchestration-oracle-" })),
    Layer.provideMerge(NodeServices.layer),
    Layer.provideMerge(now === undefined ? Layer.empty : TestClock.layer()),
  );
  const runtime = ManagedRuntime.make(layer);
  if (now !== undefined) await runtime.runPromise(TestClock.setTime(Date.parse(now)));
  const engine = await runtime.runPromise(Effect.service(OrchestrationEngineService));
  const decodeCommand = Schema.decodeUnknownSync(Schema.toCodecJson(OrchestrationCommand));
  const decodeOrigin = Schema.decodeUnknownSync(Schema.toCodecJson(OrchestrationClientOrigin));

  const results: Array<unknown> = [];
  const lines = Readline.createInterface({ input: process.stdin });
  for await (const line of lines) {
    if (line.trim().length === 0) continue;
    const input = JSON.parse(line) as { command: unknown; origin?: unknown };
    const command = decodeCommand(input.command);
    const origin = input.origin === undefined ? undefined : decodeOrigin(input.origin);
    const exit = await runtime.runPromiseExit(
      engine.dispatch(command, origin === undefined ? undefined : { origin }),
    );
    if (exit._tag === "Success") {
      results.push({ sequence: exit.value.sequence });
    } else {
      const error = Cause.squash(exit.cause) as { _tag?: string; message?: string };
      results.push({ error: { _tag: error?._tag ?? "Unknown", message: error?.message ?? String(error) } });
    }
  }
  await runtime.dispose();
  const events = readEventRows(dbPath).map((row) => ({
    ...row,
    payload: JSON.parse(row.payload as string),
    metadata: JSON.parse(row.metadata as string),
  }));
  writeOutput(JSON.stringify({ results, events }));
}

const [mode, target, ...rest] = process.argv.slice(2);
const nowFlag = rest.indexOf("--now");
const fixedNow = nowFlag === -1 ? undefined : rest[nowFlag + 1];
if (mode === "project" && target) {
  await project(target);
} else if (mode === "dispatch" && target) {
  await dispatch(target, fixedNow);
} else {
  process.stderr.write("usage: orchestration-oracle.ts project <state.sqlite> | dispatch <dir>\n");
  process.exit(2);
}
