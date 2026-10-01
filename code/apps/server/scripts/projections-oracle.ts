#!/usr/bin/env node
/**
 * The TypeScript side of the zc-projections gates (crates/zenith-code/crates/zc-projections).
 * Runs the real projection pipeline and snapshot queries from source on a database COPY.
 *
 *   node scripts/projections-oracle.ts bootstrap --db <copy>          # drop + rebuild projections
 *   node scripts/projections-oracle.ts identities --db <copy>         # repository identities (git)
 *   node scripts/projections-oracle.ts query --db <copy> --requests <file> [--identities <file>]
 *
 * `query` reads `[{method, args}]` and prints one JSON line per request with the wire-encoded
 * result (`{ok: value}` or `{error: tag}`). Never point it at the live database.
 */
import * as NodeServices from "@effect/platform-node/NodeServices";
import * as NodeFS from "node:fs";
import * as NodeOS from "node:os";
import * as NodePath from "node:path";
import {
  OrchestrationProject,
  OrchestrationProjectShell,
  OrchestrationReadModel,
  OrchestrationSearchThreadsResult,
  OrchestrationShellSnapshot,
  OrchestrationThread,
  OrchestrationThreadActivity,
  OrchestrationThreadDetailSnapshot,
  OrchestrationThreadShell,
  type RepositoryIdentity,
} from "@t3tools/contracts";
import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as Option from "effect/Option";
import * as Schema from "effect/Schema";
import * as SqlClient from "effect/unstable/sql/SqlClient";

import { ServerConfig } from "../src/config.ts";
import { projectThreadDetailSnapshot } from "../src/orchestration/ActivityPayloadProjection.ts";
import { OrchestrationProjectionPipelineLive } from "../src/orchestration/Layers/ProjectionPipeline.ts";
import { OrchestrationProjectionSnapshotQueryLive } from "../src/orchestration/Layers/ProjectionSnapshotQuery.ts";
import { OrchestrationProjectionPipeline } from "../src/orchestration/Services/ProjectionPipeline.ts";
import { ProjectionSnapshotQuery } from "../src/orchestration/Services/ProjectionSnapshotQuery.ts";
import * as ThreadBackgroundLiveness from "../src/orchestration/ThreadBackgroundLiveness.ts";
import * as ThreadPlanProgress from "../src/orchestration/ThreadPlanProgress.ts";
import { OrchestrationEventStoreLive } from "../src/persistence/Layers/OrchestrationEventStore.ts";
import { OrchestrationEventStore } from "../src/persistence/Services/OrchestrationEventStore.ts";
import { makeSqlitePersistenceLive } from "../src/persistence/Layers/Sqlite.ts";
import * as RepositoryIdentityResolver from "../src/project/RepositoryIdentityResolver.ts";

const PROJECTION_TABLES = [
  "projection_projects",
  "projection_threads",
  "projection_thread_messages",
  "projection_thread_activities",
  "projection_thread_sessions",
  "projection_turns",
  "projection_pending_approvals",
  "projection_thread_proposed_plans",
  "projection_thread_pull_requests",
  "projection_state",
];

function arg(name: string): string | undefined {
  const index = process.argv.indexOf(`--${name}`);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

const command = process.argv[2];
const dbPath = command === "scenarios" ? NodePath.join(arg("out-dir") ?? "", "unused.sqlite") : arg("db");
if (!dbPath || (command !== "scenarios" && !NodeFS.existsSync(dbPath))) {
  console.error("--db <copy of state.sqlite> is required");
  process.exit(2);
}
if (NodePath.resolve(dbPath).includes(`${NodePath.sep}.zenith${NodePath.sep}`)) {
  console.error("refusing to open a database under ~/.zenith: pass a copy");
  process.exit(2);
}

const baseDir = NodeFS.mkdtempSync(NodePath.join(NodeOS.tmpdir(), "zc-projections-oracle-"));
const persistence = makeSqlitePersistenceLive(dbPath).pipe(Layer.provide(NodeServices.layer));

const encode = <S extends Schema.Top>(schema: S) => Schema.encodeUnknownSync(Schema.toCodecJson(schema));
const encodeOption =
  <S extends Schema.Top>(schema: S) =>
  (value: Option.Option<unknown>) =>
    Option.isNone(value) ? null : encode(schema)(value.value);

const bootstrap = Effect.gen(function* () {
  const sql = yield* SqlClient.SqlClient;
  for (const table of PROJECTION_TABLES) {
    yield* sql.unsafe(`DELETE FROM ${table}`);
  }
  const pipeline = yield* OrchestrationProjectionPipeline;
  const started = Date.now();
  yield* pipeline.bootstrap;
  console.log(JSON.stringify({ ok: true, ms: Date.now() - started }));
}).pipe(
  Effect.provide(
    OrchestrationProjectionPipelineLive.pipe(
      Layer.provideMerge(OrchestrationEventStoreLive),
      Layer.provideMerge(ServerConfig.layerTest(process.cwd(), baseDir)),
      Layer.provideMerge(persistence),
      Layer.provideMerge(NodeServices.layer),
    ),
  ),
);

const identities = Effect.gen(function* () {
  const sql = yield* SqlClient.SqlClient;
  const resolver = yield* RepositoryIdentityResolver.RepositoryIdentityResolver;
  const rows = yield* sql<{ root: string }>`SELECT DISTINCT workspace_root AS root FROM projection_projects`;
  const out: Record<string, RepositoryIdentity | null> = {};
  for (const row of rows) {
    out[row.root] = yield* resolver.resolve(row.root);
  }
  console.log(JSON.stringify(out));
}).pipe(
  Effect.provide(
    Layer.mergeAll(RepositoryIdentityResolver.layer, persistence).pipe(
      Layer.provideMerge(NodeServices.layer),
    ),
  ),
);

type Request = { readonly method: string; readonly args?: ReadonlyArray<unknown> };

const answerRequests = (requests: ReadonlyArray<Request>) =>
  Effect.gen(function* () {
    const q = yield* ProjectionSnapshotQuery;
    const run = (request: Request): Effect.Effect<unknown, unknown> => {
      const a = (request.args ?? []) as Array<any>;
      switch (request.method) {
        case "getShellSnapshot":
          return q.getShellSnapshot(a[0]).pipe(Effect.map(encode(OrchestrationShellSnapshot)));
        case "getArchivedShellSnapshot":
          return q.getArchivedShellSnapshot().pipe(Effect.map(encode(OrchestrationShellSnapshot)));
        case "getSnapshot":
          return q.getSnapshot().pipe(Effect.map(encode(OrchestrationReadModel)));
        case "getCommandReadModel":
          return q.getCommandReadModel().pipe(Effect.map(encode(OrchestrationReadModel)));
        case "getThreadDetailSnapshot":
          return q
            .getThreadDetailSnapshot(a[0], a[1] ?? undefined)
            .pipe(Effect.map(encodeOption(OrchestrationThreadDetailSnapshot)));
        case "getThreadDetailSnapshotProjected":
          return q
            .getThreadDetailSnapshot(a[0], a[1] ?? undefined)
            .pipe(
              Effect.map((snapshot) =>
                encodeOption(OrchestrationThreadDetailSnapshot)(
                  Option.map(snapshot, (value) => projectThreadDetailSnapshot(value, a[2] === true)),
                ),
              ),
            );
        case "getThreadDetailById":
          return q.getThreadDetailById(a[0], a[1]).pipe(Effect.map(encodeOption(OrchestrationThread)));
        case "getThreadShellById":
          return q.getThreadShellById(a[0]).pipe(Effect.map(encodeOption(OrchestrationThreadShell)));
        case "getProjectShellById":
          return q.getProjectShellById(a[0]).pipe(Effect.map(encodeOption(OrchestrationProjectShell)));
        case "getProjectShells":
          return q
            .getProjectShells(a[0])
            .pipe(Effect.map((rows) => rows.map(encode(OrchestrationProjectShell))));
        case "getActiveProjectByWorkspaceRoot":
          return q
            .getActiveProjectByWorkspaceRoot(a[0])
            .pipe(Effect.map(encodeOption(OrchestrationProject)));
        case "searchThreads":
          return q.searchThreads(a[0]).pipe(Effect.map(encode(OrchestrationSearchThreadsResult)));
        case "listActivitiesByKind":
          return q
            .listActivitiesByKind(a[0])
            .pipe(Effect.map((rows) => rows.map(encode(OrchestrationThreadActivity))));
        case "getUserInputActivity":
          return q
            .getUserInputActivity(a[0])
            .pipe(Effect.map(encodeOption(OrchestrationThreadActivity)));
        case "getThreadRuntimeContext":
          return q.getThreadRuntimeContext(a[0]).pipe(Effect.map(Option.getOrNull));
        case "getTurnStartMessage":
          return q.getTurnStartMessage(a[0]).pipe(Effect.map(Option.getOrNull));
        case "getThreadCheckpointContext":
          return q.getThreadCheckpointContext(a[0]).pipe(Effect.map(Option.getOrNull));
        case "getFullThreadDiffContext":
          return q.getFullThreadDiffContext(a[0], a[1]).pipe(Effect.map(Option.getOrNull));
        case "getFirstActiveThreadIdByProjectId":
          return q.getFirstActiveThreadIdByProjectId(a[0]).pipe(Effect.map(Option.getOrNull));
        case "getImportedAgentSessionSources":
          return q.getImportedAgentSessionSources(a[0]);
        case "listThreadsWithPullRequests":
          return q.listThreadsWithPullRequests();
        case "getDeletedWorktreeThreads":
          return q.getDeletedWorktreeThreads();
        case "getSnapshotSequence":
          return q.getSnapshotSequence();
        case "getCounts":
          return q.getCounts();
        case "getEventReplayStats":
          return q.getEventReplayStats(a[0]);
        default:
          return Effect.fail(`unknown method ${request.method}`);
      }
    };
    const lines: Array<string> = [];
    for (const request of requests) {
      const result = yield* Effect.result(run(request));
      lines.push(
        JSON.stringify(
          result._tag === "Success"
            ? { ok: result.success === undefined ? null : result.success }
            : { error: String((result.failure as { _tag?: string })?._tag ?? result.failure) },
        ),
      );
    }
    return lines;
  });

const queryLayer = (
  persistenceLayer: typeof persistence,
  identities: Record<string, RepositoryIdentity | null>,
) =>
  OrchestrationProjectionSnapshotQueryLive.pipe(
    Layer.provide(ThreadBackgroundLiveness.layer),
    Layer.provide(ThreadPlanProgress.layer),
    Layer.provide(
      Layer.succeed(RepositoryIdentityResolver.RepositoryIdentityResolver, {
        resolve: (root: string) => Effect.succeed(identities[root] ?? null),
      }),
    ),
    Layer.provideMerge(persistenceLayer),
  );

const query = Effect.gen(function* () {
  const requests: ReadonlyArray<Request> = JSON.parse(
    NodeFS.readFileSync(arg("requests") ?? "", "utf8"),
  );
  const file = arg("identities");
  const identityMap = file === undefined ? {} : JSON.parse(NodeFS.readFileSync(file, "utf8"));
  const lines = yield* answerRequests(requests).pipe(
    Effect.provide(queryLayer(persistence, identityMap)),
  );
  for (const line of lines) console.log(line);
});

type Scenario = {
  readonly name: string;
  readonly attachments?: ReadonlyArray<string>;
  readonly events: ReadonlyArray<Record<string, unknown>>;
  readonly requests?: ReadonlyArray<Request>;
  readonly identities?: Record<string, RepositoryIdentity | null>;
};

/**
 * `scenarios --file <json> --out-dir <dir>`: for each scenario and mode (`bootstrap`: append
 * every event then bootstrap; `live`: append and projectEvent one by one), a fresh database
 * `<dir>/<name>-<mode>.sqlite` and the attachment files left in `<dir>/<name>-<mode>-files.json`.
 */
const runScenario = (scenario: Scenario, mode: "bootstrap" | "live", outDir: string) => {
  const scenarioBase = NodePath.join(outDir, `${scenario.name}-${mode}-base`);
  const scenarioDb = NodePath.join(outDir, `${scenario.name}-${mode}.sqlite`);
  return Effect.gen(function* () {
    const config = yield* ServerConfig;
    for (const file of scenario.attachments ?? []) {
      NodeFS.writeFileSync(NodePath.join(config.attachmentsDir, file), file);
    }
    const eventStore = yield* OrchestrationEventStore;
    const pipeline = yield* OrchestrationProjectionPipeline;
    for (const [index, event] of scenario.events.entries()) {
      const stored = yield* eventStore.append(event as never).pipe(
        Effect.mapError(
          (error) =>
            new Error(
              `${scenario.name} event ${index} (${String(event.type)}): ${String((error as { message?: unknown }).message ?? error)}`,
            ),
        ),
      );
      if (mode === "live") {
        yield* pipeline.projectEvent(stored);
      }
    }
    if (mode === "bootstrap") {
      yield* pipeline.bootstrap;
    }
    NodeFS.writeFileSync(
      NodePath.join(outDir, `${scenario.name}-${mode}-files.json`),
      JSON.stringify(NodeFS.readdirSync(config.attachmentsDir).toSorted()),
    );
  }).pipe(
    Effect.andThen(
      mode === "bootstrap" && scenario.requests !== undefined
        ? answerRequests(scenario.requests).pipe(
            Effect.tap((lines) =>
              Effect.sync(() =>
                NodeFS.writeFileSync(
                  NodePath.join(outDir, `${scenario.name}-answers.jsonl`),
                  lines.join("\n"),
                ),
              ),
            ),
            Effect.provide(
              queryLayer(
                makeSqlitePersistenceLive(scenarioDb).pipe(Layer.provide(NodeServices.layer)),
                scenario.identities ?? {},
              ),
            ),
          )
        : Effect.void,
    ),
  ).pipe(
    Effect.provide(
      OrchestrationProjectionPipelineLive.pipe(
        Layer.provideMerge(OrchestrationEventStoreLive),
        Layer.provideMerge(ServerConfig.layerTest(process.cwd(), scenarioBase)),
        Layer.provideMerge(makeSqlitePersistenceLive(scenarioDb).pipe(Layer.provide(NodeServices.layer))),
        Layer.provideMerge(NodeServices.layer),
      ),
    ),
  );
};

const scenarios = Effect.gen(function* () {
  const list: ReadonlyArray<Scenario> = JSON.parse(NodeFS.readFileSync(arg("file") ?? "", "utf8"));
  const outDir = arg("out-dir") ?? "";
  for (const scenario of list) {
    for (const mode of ["bootstrap", "live"] as const) {
      yield* runScenario(scenario, mode, outDir);
    }
  }
  console.log(JSON.stringify({ ok: true, scenarios: list.length }));
});

const program =
  command === "bootstrap"
    ? bootstrap
    : command === "identities"
      ? identities
      : command === "query"
        ? query
        : command === "scenarios"
          ? scenarios
          : Effect.die(`unknown command ${command}`);

Effect.runPromise(program as Effect.Effect<void, unknown>)
  .catch((error) => {
    console.error(String(error?.message ?? error), error?.stack ?? "");
    process.exitCode = 1;
  })
  .finally(() => NodeFS.rmSync(baseDir, { recursive: true, force: true }));
