// The TS oracle of tests/golden.rs (agent session part): runs the real AgentSessionScanner of
// code/apps/server with the default provider settings (the host's ~/.claude and ~/.codex, or
// CLAUDE_CONFIG_DIR / CODEX_HOME), a test ServerConfig on a temp base dir, a fixed clock, and
// a projection query that knows the given project roots. Only reads the homes.
//
// Run as `node --input-type=module -e <this file>` from code/apps/server, with ZC_SERVER_SRC
// pointing at code/apps/server/src and the request on stdin:
//   {"baseDir": "...", "nowMs": 0, "projects": ["/root", ...], "recent": ["/root", ...]}
// Prints `@@ORACLE@@` then {"scan": AgentSessionScanResult, "recent": {root: [outcome, ...]}}
// where an outcome is {tag, source?, thread?}.

import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as Stream from "effect/Stream";
import * as TestClock from "effect/testing/TestClock";
import * as NodeServices from "@effect/platform-node/NodeServices";

const src = process.env.ZC_SERVER_SRC;
const imp = (path) => import(`${src}/${path}`);
const AgentSessionScanner = await imp("project/AgentSessionScanner.ts");
const ServerConfig = await imp("config.ts");
const ServerSettings = await imp("serverSettings.ts");
const ProjectionSnapshotQuery = await imp("orchestration/Services/ProjectionSnapshotQuery.ts");

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));

const projects = request.projects.map((workspaceRoot, index) => ({
  id: `project-${index + 1}`,
  title: "Project",
  workspaceRoot,
  defaultModelSelection: null,
  scripts: [],
  createdAt: "2026-01-01T00:00:00.000Z",
  updatedAt: "2026-01-01T00:00:00.000Z",
}));
const queries = Layer.succeed(ProjectionSnapshotQuery.ProjectionSnapshotQuery, {
  getShellSnapshot: () =>
    Effect.succeed({ snapshotSequence: 0, projects, threads: [], updatedAt: "2026-01-01T00:00:00.000Z" }),
  getImportedAgentSessionSources: () => Effect.succeed([]),
});

const layer = AgentSessionScanner.layer.pipe(
  Layer.provide(
    Layer.mergeAll(
      ServerSettings.layerTest({}),
      ServerConfig.layerTest(request.baseDir, request.baseDir),
      queries,
    ),
  ),
  Layer.provideMerge(NodeServices.layer),
);

const result = await Effect.runPromise(
  Effect.gen(function* () {
    yield* TestClock.setTime(request.nowMs);
    const scanner = yield* AgentSessionScanner.AgentSessionScanner;
    const scan = yield* scanner.scan;
    const recent = {};
    for (const root of request.recent) {
      const outcomes = yield* scanner.recentThreads(root).pipe(Stream.runCollect);
      recent[root] = Array.from(outcomes, (outcome) => ({
        tag: outcome._tag,
        ...(outcome.source ? { source: outcome.source } : {}),
        ...(outcome.thread ? { thread: outcome.thread } : {}),
      }));
    }
    return { scan, recent };
  }).pipe(Effect.provide(layer), Effect.provide(TestClock.layer())),
);
process.stdout.write("@@ORACLE@@" + JSON.stringify(result));
