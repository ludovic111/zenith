// @effect-diagnostics nodeBuiltinImport:off globalDate:off globalTimers:off - a standalone Node oracle script.
/**
 * The TypeScript provider runtime ingestion as an oracle for the Rust port (`zc-reactors`).
 *
 *   node apps/server/scripts/ingestion-oracle.ts script <events.<thread>.log> [--deltas] --out script.jsonl
 *   node apps/server/scripts/ingestion-oracle.ts run <script.jsonl> --out ts-events.json
 *
 * `script` turns the `CANON:` lines of one provider event log (one thread) into a replay
 * script: setup commands (a project, the thread, a ready session), then for each runtime event
 * a clock step to its `createdAt` and the event itself. A synthesized `thread.turn.start`
 * precedes every `turn.started`, standing in for the user message the log does not carry.
 * Events the logger truncated (`{truncated: true}`) are left out. The logger never writes
 * `content.delta`; with `--deltas`, the text of each completed assistant or reasoning item is
 * streamed back as deltas spread between the previous event and the completion, so the
 * buffering cadence is exercised too.
 *
 * `run` replays a script through `ProviderRuntimeIngestionLive` on a fresh in-memory engine.
 * Every layer reads one virtual clock (moved by the clock steps, never backwards; sleeps stay
 * real), so the streaming cadence and every "now" depend on the script only. Each step is
 * drained before the next. Prints every stored domain event, wire-encoded.
 *
 * The Rust side is `cargo test -p zc-reactors --test ingestion_replay_gate -- --ignored`.
 * Never point `script` at a live log directory: copy the logs first.
 */
import * as NodeFs from "node:fs";

import * as NodeServices from "@effect/platform-node/NodeServices";
import * as Clock from "effect/Clock";
import * as Deferred from "effect/Deferred";
import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as ManagedRuntime from "effect/ManagedRuntime";
import * as PubSub from "effect/PubSub";
import * as Schema from "effect/Schema";
import * as Scope from "effect/Scope";
import * as Stream from "effect/Stream";

import {
  OrchestrationCommand,
  OrchestrationEvent,
  ProviderDriverKind,
  ProviderRuntimeEvent,
  type ProviderSession,
} from "@t3tools/contracts";

/** A replay step, shared with the Rust gate. */
type Step =
  | { readonly kind: "command"; readonly command: Record<string, unknown> }
  | { readonly kind: "clock"; readonly millis: number }
  | { readonly kind: "event"; readonly event: Record<string, unknown> }
  | { readonly kind: "session"; readonly session: Record<string, unknown> };

const PROJECT_ID = "replay-project";
const WORKSPACE_ROOT = "/nonexistent/replay-workspace";

function writeOutput(text: string) {
  const outFlag = process.argv.indexOf("--out");
  if (outFlag === -1) {
    process.stdout.write(text);
  } else {
    NodeFs.writeFileSync(process.argv[outFlag + 1]!, text);
  }
}

function buildScript(logPath: string, withDeltas: boolean): Array<Step> {
  const events: Array<Record<string, any>> = [];
  for (const line of NodeFs.readFileSync(logPath, "utf8").split("\n")) {
    const marker = line.indexOf("CANON: ");
    if (marker === -1) continue;
    const event = JSON.parse(line.slice(marker + "CANON: ".length));
    // The logger cuts oversized events down to `{truncated: true, ...}`; they cannot be replayed.
    if (event.truncated === true) continue;
    events.push(event);
  }
  if (events.length === 0) throw new Error(`no CANON lines in ${logPath}`);
  const first = events[0]!;
  const threadId = String(first.threadId);
  const provider = String(first.provider);
  const instanceId = String(first.providerInstanceId ?? first.provider);
  const createdAt = String(first.createdAt);
  const selection = { instanceId, model: "replay-model" };
  const steps: Array<Step> = [
    { kind: "clock", millis: Date.parse(createdAt) },
    {
      kind: "command",
      command: {
        type: "project.create",
        commandId: "replay-project-create",
        projectId: PROJECT_ID,
        title: "Replay Project",
        workspaceRoot: WORKSPACE_ROOT,
        defaultModelSelection: selection,
        createdAt,
      },
    },
    {
      kind: "command",
      command: {
        type: "thread.create",
        commandId: "replay-thread-create",
        threadId,
        projectId: PROJECT_ID,
        title: "Replay Thread",
        modelSelection: selection,
        interactionMode: "default",
        runtimeMode: "full-access",
        branch: null,
        worktreePath: null,
        createdAt,
      },
    },
    {
      kind: "command",
      command: {
        type: "thread.session.set",
        commandId: "replay-session-seed",
        threadId,
        session: {
          threadId,
          status: "ready",
          providerName: provider,
          providerInstanceId: instanceId,
          runtimeMode: "full-access",
          activeTurnId: null,
          updatedAt: createdAt,
          lastError: null,
        },
        createdAt,
      },
    },
    {
      kind: "session",
      session: {
        provider,
        providerInstanceId: instanceId,
        status: "ready",
        runtimeMode: "full-access",
        threadId,
        createdAt,
        updatedAt: createdAt,
      },
    },
  ];
  let turn = 0;
  let previousAt = Date.parse(createdAt);
  let deltaCount = 0;
  for (const event of events.filter((candidate) => candidate.threadId === threadId)) {
    const at = Date.parse(String(event.createdAt));
    const streamKind =
      event.payload?.itemType === "assistant_message"
        ? "assistant_text"
        : event.payload?.itemType === "reasoning"
          ? "reasoning_text"
          : undefined;
    if (
      withDeltas &&
      event.type === "item.completed" &&
      streamKind !== undefined &&
      typeof event.payload?.detail === "string" &&
      event.payload.detail.length > 0
    ) {
      // The logger drops `content.delta`; rebuild a stream from the completed text, spread
      // evenly between the previous event and the completion so the 400 ms pacing runs.
      const tokens: Array<string> = event.payload.detail.split(/(\s+)/);
      const chunks: Array<string> = [];
      for (let index = 0; index < tokens.length; index += 6) {
        chunks.push(tokens.slice(index, index + 6).join(""));
      }
      const span = Math.max(0, at - previousAt - 2);
      chunks.forEach((delta, index) => {
        const deltaAt = previousAt + 1 + Math.floor((span * index) / chunks.length);
        deltaCount += 1;
        steps.push({ kind: "clock", millis: deltaAt });
        steps.push({
          kind: "event",
          event: {
            type: "content.delta",
            eventId: `replay-delta-${deltaCount}`,
            provider: event.provider,
            ...(event.providerInstanceId ? { providerInstanceId: event.providerInstanceId } : {}),
            createdAt: new Date(deltaAt).toISOString(),
            threadId,
            ...(event.turnId ? { turnId: event.turnId } : {}),
            ...(event.itemId ? { itemId: event.itemId } : {}),
            payload: { streamKind, delta },
          },
        });
      });
    }
    previousAt = Math.max(previousAt, at);
    if (event.type === "turn.started") {
      turn += 1;
      const requestedAt = new Date(at - 1).toISOString();
      steps.push({ kind: "clock", millis: at - 1 });
      steps.push({
        kind: "command",
        command: {
          type: "thread.turn.start",
          commandId: `replay-turn-start-${turn}`,
          threadId,
          message: {
            messageId: `replay-user-message-${turn}`,
            role: "user",
            text: `Replayed turn ${turn}`,
            attachments: [],
          },
          interactionMode: "default",
          runtimeMode: "full-access",
          createdAt: requestedAt,
        },
      });
    }
    steps.push({ kind: "clock", millis: at });
    steps.push({ kind: "event", event });
  }
  return steps;
}

async function run(scriptPath: string) {
  const { OrchestrationEngineLive } =
    await import("../src/orchestration/Layers/OrchestrationEngine.ts");
  const { OrchestrationProjectionPipelineLive } =
    await import("../src/orchestration/Layers/ProjectionPipeline.ts");
  const { OrchestrationProjectionSnapshotQueryLive } =
    await import("../src/orchestration/Layers/ProjectionSnapshotQuery.ts");
  const { OrchestrationEventStoreLive } =
    await import("../src/persistence/Layers/OrchestrationEventStore.ts");
  const { OrchestrationCommandReceiptRepositoryLive } =
    await import("../src/persistence/Layers/OrchestrationCommandReceipts.ts");
  const { SqlitePersistenceMemory } = await import("../src/persistence/Layers/Sqlite.ts");
  const RepositoryIdentityResolver = await import("../src/project/RepositoryIdentityResolver.ts");
  const ThreadBackgroundLiveness = await import("../src/orchestration/ThreadBackgroundLiveness.ts");
  const ThreadPlanProgress = await import("../src/orchestration/ThreadPlanProgress.ts");
  const { ProviderRuntimeIngestionLive } =
    await import("../src/orchestration/Layers/ProviderRuntimeIngestion.ts");
  const { ProviderService } = await import("../src/provider/Services/ProviderService.ts");
  const CheckpointStore = await import("../src/checkpointing/CheckpointStore.ts");
  const { ServerConfig } = await import("../src/config.ts");
  const { ServerSettingsService } = await import("../src/serverSettings.ts");
  const { OrchestrationEngineService } =
    await import("../src/orchestration/Services/OrchestrationEngine.ts");
  const { ProviderRuntimeIngestionService } =
    await import("../src/orchestration/Services/ProviderRuntimeIngestion.ts");

  const steps = NodeFs.readFileSync(scriptPath, "utf8")
    .split("\n")
    .filter((line) => line.trim().length > 0)
    .map((line) => JSON.parse(line) as Step);

  // One virtual clock for every layer; sleeps stay real.
  let virtualNow = 0;
  const realClock = Effect.runSync(Effect.service(Clock.Clock));
  const virtualClock: Clock.Clock = {
    currentTimeMillisUnsafe: () => virtualNow,
    currentTimeMillis: Effect.sync(() => virtualNow),
    currentTimeNanosUnsafe: () => BigInt(virtualNow) * 1_000_000n,
    currentTimeNanos: Effect.sync(() => BigInt(virtualNow) * 1_000_000n),
    monotonicTimeNanosUnsafe: () => realClock.monotonicTimeNanosUnsafe(),
    monotonicTimeNanos: realClock.monotonicTimeNanos,
    sleep: (duration) => realClock.sleep(duration),
  };

  const sessions: Array<ProviderSession> = [];
  const runtimeEvents = Effect.runSync(
    PubSub.unbounded<{
      readonly event: ProviderRuntimeEvent;
      readonly enqueued: Deferred.Deferred<void>;
    }>(),
  );
  const unsupported = () => Effect.die(new Error("Unsupported provider call in replay")) as never;
  const providerService = {
    startSession: unsupported,
    sendTurn: unsupported,
    compactThread: unsupported,
    interruptTurn: unsupported,
    respondToRequest: unsupported,
    respondToUserInput: unsupported,
    stopSession: unsupported,
    listSessions: () => Effect.succeed([...sessions]),
    getCapabilities: () => Effect.succeed({ sessionModelSwitch: "in-session" }),
    assertConversationRollbackSupported: unsupported,
    getInstanceInfo: (instanceId: any) => {
      const driverKind = ProviderDriverKind.make(String(instanceId));
      return Effect.succeed({
        instanceId,
        driverKind,
        displayName: undefined,
        enabled: true,
        continuationIdentity: {
          driverKind,
          continuationKey: `${driverKind}:instance:${instanceId}`,
        },
      });
    },
    rollbackConversation: unsupported,
    uploadFeedback: unsupported,
    get streamEvents() {
      return Stream.fromPubSub(runtimeEvents).pipe(
        Stream.flatMap(({ event, enqueued }) =>
          Stream.concat(
            Stream.make(event),
            Stream.fromEffect(Deferred.succeed(enqueued, undefined)).pipe(Stream.drain),
          ),
        ),
      );
    },
  } as unknown as typeof ProviderService.Service;

  const orchestrationLayer = OrchestrationEngineLive.pipe(
    Layer.provide(OrchestrationProjectionSnapshotQueryLive),
    Layer.provide(OrchestrationProjectionPipelineLive),
    Layer.provide(OrchestrationEventStoreLive),
    Layer.provide(OrchestrationCommandReceiptRepositoryLive),
    Layer.provide(RepositoryIdentityResolver.layer),
    Layer.provide(SqlitePersistenceMemory),
  );
  const snapshotLayer = OrchestrationProjectionSnapshotQueryLive.pipe(
    Layer.provide(RepositoryIdentityResolver.layer),
    Layer.provide(SqlitePersistenceMemory),
  );
  const layer = ProviderRuntimeIngestionLive.pipe(
    Layer.provideMerge(orchestrationLayer),
    Layer.provideMerge(snapshotLayer),
    Layer.provideMerge(ThreadBackgroundLiveness.layer),
    Layer.provideMerge(ThreadPlanProgress.layer),
    Layer.provideMerge(SqlitePersistenceMemory),
    Layer.provideMerge(Layer.succeed(ProviderService, providerService)),
    Layer.provideMerge(ServerSettingsService.layerTest({})),
    Layer.provideMerge(
      Layer.succeed(CheckpointStore.CheckpointStore, {
        isGitRepository: () => Effect.succeed(false),
      } as never),
    ),
    Layer.provideMerge(ServerConfig.layerTest(process.cwd(), process.cwd())),
    Layer.provideMerge(NodeServices.layer),
    Layer.provideMerge(Layer.succeed(Clock.Clock, virtualClock)),
  );
  const runtime = ManagedRuntime.make(layer);
  const engine = await runtime.runPromise(Effect.service(OrchestrationEngineService));
  const ingestion = await runtime.runPromise(Effect.service(ProviderRuntimeIngestionService));
  const scope = await Effect.runPromise(Scope.make("sequential"));
  await runtime.runPromise(ingestion.start().pipe(Scope.provide(scope)));

  const decodeCommand = Schema.decodeUnknownSync(Schema.toCodecJson(OrchestrationCommand));
  const decodeRuntimeEvent = Schema.decodeUnknownSync(ProviderRuntimeEvent);
  const settle = () => new Promise((resolve) => setTimeout(resolve, 5));
  const errors: Array<unknown> = [];
  for (const [index, step] of steps.entries()) {
    switch (step.kind) {
      case "clock":
        virtualNow = Math.max(virtualNow, step.millis);
        break;
      case "session":
        sessions.splice(0, sessions.length, step.session as unknown as ProviderSession);
        break;
      case "command": {
        const exit = await runtime.runPromiseExit(engine.dispatch(decodeCommand(step.command)));
        if (exit._tag === "Failure") errors.push({ step: index, error: String(exit.cause) });
        // The ingestion reacts to `thread.turn-start-requested` on its domain subscription.
        await settle();
        await runtime.runPromise(ingestion.drain);
        break;
      }
      case "event": {
        const event = decodeRuntimeEvent(step.event);
        await runtime.runPromise(
          Effect.gen(function* () {
            const enqueued = yield* Deferred.make<void>();
            yield* PubSub.publish(runtimeEvents, { event, enqueued });
            yield* Deferred.await(enqueued);
          }),
        );
        await runtime.runPromise(ingestion.drain);
        break;
      }
    }
  }
  await settle();
  await runtime.runPromise(ingestion.drain);

  const encodeEvent = Schema.encodeSync(Schema.toCodecJson(OrchestrationEvent));
  const stored = await runtime.runPromise(
    Stream.runCollect(engine.readEvents(0)).pipe(Effect.map((events) => Array.from(events))),
  );
  await Effect.runPromise(Scope.close(scope, { _tag: "Success", value: undefined } as never));
  await runtime.dispose();
  writeOutput(JSON.stringify({ errors, events: stored.map((event) => encodeEvent(event)) }));
}

const [mode, target] = process.argv.slice(2);
if (mode === "script" && target) {
  writeOutput(
    buildScript(target, process.argv.includes("--deltas"))
      .map((step) => JSON.stringify(step))
      .join("\n") + "\n",
  );
} else if (mode === "run" && target) {
  await run(target);
} else {
  process.stderr.write("usage: ingestion-oracle.ts script <log> | run <script.jsonl>\n");
  process.exit(2);
}
