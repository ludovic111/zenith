// The TS oracle of tests/golden.rs (checkpoint part): runs the real CheckpointReactor of
// code/apps/server (with the real orchestration engine on in-memory SQLite, the projection
// pipeline and snapshot query, the real CheckpointStore and CheckpointDiffQuery) through a
// scripted scenario on a temp repository, with a scripted provider service.
//
// Run as `node --input-type=module -e <this file>` from code/apps/server, with ZC_SERVER_SRC
// pointing at code/apps/server/src and the request on stdin:
//   {"baseDir": "...", "session": {...ProviderSession}, "steps": [{"op": ...}, ...]}
// Ops: dispatch {command} | emit {event} | receipts {count} | drain | write {path, contents}
//      | turnDiff {input} | fullThreadDiff {input} | waitEvent {type}
// Prints `@@ORACLE@@` then {"events": [encoded orchestration events], "results": [...], "receipts": [...]}.

import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as ManagedRuntime from "effect/ManagedRuntime";
import * as PubSub from "effect/PubSub";
import * as Queue from "effect/Queue";
import * as Schema from "effect/Schema";
import * as Scope from "effect/Scope";
import * as Exit from "effect/Exit";
import * as Stream from "effect/Stream";
import * as NodeServices from "@effect/platform-node/NodeServices";
import * as NodeFS from "node:fs";

const src = process.env.ZC_SERVER_SRC;
const imp = (path) => import(`${src}/${path}`);
const Contracts = await import("@t3tools/contracts");
const CheckpointStore = await imp("checkpointing/CheckpointStore.ts");
const CheckpointDiffQuery = await imp("checkpointing/CheckpointDiffQuery.ts");
const VcsDriverRegistry = await imp("vcs/VcsDriverRegistry.ts");
const VcsProcess = await imp("vcs/VcsProcess.ts");
const { VcsStatusBroadcaster } = await imp("vcs/VcsStatusBroadcaster.ts");
const RepositoryIdentityResolver = await imp("project/RepositoryIdentityResolver.ts");
const { CheckpointReactorLive } = await imp("orchestration/Layers/CheckpointReactor.ts");
const { OrchestrationEngineLive } = await imp("orchestration/Layers/OrchestrationEngine.ts");
const { OrchestrationProjectionPipelineLive } = await imp("orchestration/Layers/ProjectionPipeline.ts");
const { OrchestrationProjectionSnapshotQueryLive } = await imp("orchestration/Layers/ProjectionSnapshotQuery.ts");
const ThreadBackgroundLiveness = await imp("orchestration/ThreadBackgroundLiveness.ts");
const ThreadPlanProgress = await imp("orchestration/ThreadPlanProgress.ts");
const { RuntimeReceiptBusTest } = await imp("orchestration/Layers/RuntimeReceiptBus.ts");
const RuntimeReceiptBus = await imp("orchestration/Services/RuntimeReceiptBus.ts");
const { OrchestrationEventStoreLive } = await imp("persistence/Layers/OrchestrationEventStore.ts");
const { OrchestrationCommandReceiptRepositoryLive } = await imp("persistence/Layers/OrchestrationCommandReceipts.ts");
const { SqlitePersistenceMemory } = await imp("persistence/Layers/Sqlite.ts");
const { OrchestrationEngineService } = await imp("orchestration/Services/OrchestrationEngine.ts");
const { CheckpointReactor } = await imp("orchestration/Services/CheckpointReactor.ts");
const { ProviderService } = await imp("provider/Services/ProviderService.ts");
const { ServerConfig } = await imp("config.ts");
const WorkspaceEntries = await imp("workspace/WorkspaceEntries.ts");
const { PullRequestService } = await imp("pullRequest/PullRequestService.ts");

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString("utf8"));

const runtimeEvents = Effect.runSync(PubSub.unbounded());
const rollbacks = [];
const unsupported = () => Effect.die(new Error("unsupported in the oracle"));
const providerService = {
  startSession: unsupported,
  sendTurn: unsupported,
  compactThread: unsupported,
  interruptTurn: unsupported,
  respondToRequest: unsupported,
  respondToUserInput: unsupported,
  stopSession: unsupported,
  listSessions: () => Effect.succeed(request.session ? [request.session] : []),
  getCapabilities: () => Effect.succeed({ sessionModelSwitch: "in-session" }),
  assertConversationRollbackSupported: () => Effect.void,
  getInstanceInfo: unsupported,
  rollbackConversation: (input) => Effect.sync(() => void rollbacks.push(input)),
  uploadFeedback: unsupported,
  get streamEvents() {
    return Stream.fromPubSub(runtimeEvents);
  },
};

const orchestrationLayer = OrchestrationEngineLive.pipe(
  Layer.provide(OrchestrationProjectionSnapshotQueryLive),
  Layer.provide(ThreadBackgroundLiveness.layer),
  Layer.provide(ThreadPlanProgress.layer),
  Layer.provide(OrchestrationProjectionPipelineLive),
  Layer.provide(OrchestrationEventStoreLive),
  Layer.provide(OrchestrationCommandReceiptRepositoryLive),
  Layer.provide(RepositoryIdentityResolver.layer),
  Layer.provide(SqlitePersistenceMemory),
);
const vcsStatus = Layer.succeed(VcsStatusBroadcaster, {
  getStatus: () => Effect.die("unused"),
  refreshLocalStatus: () =>
    Effect.succeed({
      isRepo: true,
      hasPrimaryRemote: false,
      isDefaultRef: true,
      refName: "main",
      hasWorkingTreeChanges: false,
      workingTree: { files: [], insertions: 0, deletions: 0 },
    }),
  refreshStatus: () => Effect.die("unused"),
  refreshPullRequestStatus: () => Effect.succeed(null),
  streamStatus: () => Stream.empty,
});
const storeLayer = CheckpointStore.layer.pipe(Layer.provide(VcsDriverRegistry.layer));
const layer = CheckpointReactorLive.pipe(
  Layer.provideMerge(orchestrationLayer),
  Layer.provideMerge(CheckpointDiffQuery.layer.pipe(Layer.provide(storeLayer), Layer.provideMerge(
    OrchestrationProjectionSnapshotQueryLive.pipe(
      Layer.provide(ThreadBackgroundLiveness.layer),
      Layer.provide(ThreadPlanProgress.layer),
      Layer.provide(RepositoryIdentityResolver.layer),
      Layer.provide(SqlitePersistenceMemory),
    ),
  ))),
  Layer.provideMerge(RuntimeReceiptBusTest),
  Layer.provideMerge(Layer.succeed(ProviderService, providerService)),
  Layer.provideMerge(Layer.mock(PullRequestService)({ refreshAfterTurn: () => Effect.void })),
  Layer.provideMerge(vcsStatus),
  Layer.provideMerge(storeLayer),
  Layer.provideMerge(Layer.mock(WorkspaceEntries.WorkspaceEntries)({ refresh: () => Effect.void })),
  Layer.provideMerge(VcsProcess.layer),
  Layer.provideMerge(ServerConfig.layerTest(process.cwd(), request.baseDir)),
  Layer.provideMerge(NodeServices.layer),
);

const runtime = ManagedRuntime.make(layer);
const engine = await runtime.runPromise(Effect.service(OrchestrationEngineService));
const reactor = await runtime.runPromise(Effect.service(CheckpointReactor));
const diffQuery = await runtime.runPromise(Effect.service(CheckpointDiffQuery.CheckpointDiffQuery));
const bus = await runtime.runPromise(Effect.service(RuntimeReceiptBus.RuntimeReceiptBus));
const scope = await Effect.runPromise(Scope.make("sequential"));
const receiptQueue = await Effect.runPromise(
  Effect.gen(function* () {
    const queue = yield* Queue.unbounded();
    yield* Stream.runForEach(bus.streamEventsForTest, (receipt) => Queue.offer(queue, receipt)).pipe(
      Effect.forkIn(scope, { startImmediately: true }),
    );
    yield* reactor.start().pipe(Scope.provide(scope));
    return queue;
  }),
);

const decodeCommand = Schema.decodeUnknownSync(Schema.toCodecJson(Contracts.OrchestrationCommand));
const encodeEvent = Schema.encodeSync(Schema.toCodecJson(Contracts.OrchestrationEvent));
const encodeError = (error) => ({ _tag: error?._tag, message: error?.message });
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const results = [];
const receipts = [];

for (const step of request.steps) {
  switch (step.op) {
    case "dispatch":
      await runtime.runPromise(engine.dispatch(decodeCommand(step.command)));
      break;
    case "emit":
      Effect.runSync(PubSub.publish(runtimeEvents, step.event));
      break;
    case "receipts":
      for (let i = 0; i < step.count; i++) receipts.push(await Effect.runPromise(Queue.take(receiptQueue)));
      break;
    case "drain":
      await sleep(30);
      await Effect.runPromise(reactor.drain);
      await sleep(30);
      await Effect.runPromise(reactor.drain);
      break;
    case "write":
      NodeFS.writeFileSync(step.path, step.contents);
      break;
    case "turnDiff":
    case "fullThreadDiff": {
      const effect = step.op === "turnDiff" ? diffQuery.getTurnDiff(step.input) : diffQuery.getFullThreadDiff(step.input);
      const exit = await runtime.runPromiseExit(effect);
      results.push(Exit.isSuccess(exit) ? { ok: exit.value } : { error: encodeError(exit.cause.reasons?.[0]?.error) });
      break;
    }
    case "waitEvent":
      for (let attempt = 0; attempt < 1500; attempt++) {
        const events = await runtime.runPromise(Stream.runCollect(engine.readEvents(0)));
        if (Array.from(events).some((event) => event.type === step.type)) break;
        await sleep(10);
      }
      break;
    default:
      throw new Error(`unknown op ${step.op}`);
  }
}

const events = Array.from(await runtime.runPromise(Stream.runCollect(engine.readEvents(0)))).map(encodeEvent);
await Effect.runPromise(Scope.close(scope, Exit.void));
await runtime.dispose();
process.stdout.write(
  "\n@@ORACLE@@" + JSON.stringify({
    events,
    results,
    receipts: receipts.map((r) => JSON.parse(JSON.stringify(r))),
    rollbacks,
  }),
);
process.exit(0);
