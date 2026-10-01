// RPC conformance: drives a zenith code server (the Rust one, `zenith-code dev-serve
// --conformance`) with the REAL Effect RPC client, over the same stack as the web app
// (`packages/client-runtime/src/rpc/session.ts`): RpcClient.makeProtocolSocket +
// RpcSerialization.layerJson + Socket.layerWebSocket, effect rc.115 with zenith's patch.
//
// The test RpcGroup below is mirrored by the Rust handlers in
// crates/zenith-code/src/server/conformance.rs.
//
//   node conformance.mjs ws://127.0.0.1:PORT/ws
//
// Env: PING_SECONDS (default 32) for the ping/pong survival check; 0 skips it.
// Exits 0 when every check passes. `run.sh` builds and starts the server for you.

import * as Effect from "effect/Effect";
import * as Exit from "effect/Exit";
import * as Fiber from "effect/Fiber";
import * as Layer from "effect/Layer";
import * as Ref from "effect/Ref";
import * as Schedule from "effect/Schedule";
import * as Schema from "effect/Schema";
import * as Stream from "effect/Stream";
import * as Rpc from "effect/unstable/rpc/Rpc";
import * as RpcClient from "effect/unstable/rpc/RpcClient";
import * as RpcGroup from "effect/unstable/rpc/RpcGroup";
import * as RpcSerialization from "effect/unstable/rpc/RpcSerialization";
import * as Socket from "effect/unstable/socket/Socket";

const url = process.argv[2];
if (!url) {
  console.error("usage: node conformance.mjs ws://127.0.0.1:PORT/ws");
  process.exit(2);
}
const pingSeconds = Number(process.env.PING_SECONDS ?? 32);

// --- the test group ---------------------------------------------------------------------

class ConformanceError extends Schema.TaggedError()("ConformanceError", {
  message: Schema.String,
  code: Schema.Number,
}) {}

class EnvironmentAuthorizationError extends Schema.TaggedError()("EnvironmentAuthorizationError", {
  message: Schema.String,
  requiredScope: Schema.String,
}) {}

const Item = Schema.Struct({ index: Schema.Number, pad: Schema.optionalKey(Schema.String) });
const Empty = Schema.Struct({});

const Group = RpcGroup.make(
  Rpc.make("conformance.echo", {
    payload: Schema.Struct({ text: Schema.String }),
    success: Schema.Struct({ text: Schema.String, length: Schema.Number }),
    error: ConformanceError,
  }),
  Rpc.make("conformance.fail", { payload: Empty, error: ConformanceError }),
  Rpc.make("conformance.die", { payload: Empty }),
  Rpc.make("conformance.panic", { payload: Empty }),
  Rpc.make("conformance.forbidden", {
    payload: Empty,
    success: Schema.String,
    error: EnvironmentAuthorizationError,
  }),
  // The Rust side expects `n` as a number: a string makes the payload fail to decode.
  Rpc.make("conformance.decode", { payload: Schema.Struct({ n: Schema.String }), success: Schema.Number }),
  Rpc.make("conformance.unknown", { payload: Empty }),
  Rpc.make("conformance.count", {
    payload: Schema.Struct({ key: Schema.String, count: Schema.Number, spaced: Schema.Boolean }),
    success: Item,
    stream: true,
  }),
  Rpc.make("conformance.failAfter", {
    payload: Schema.Struct({ count: Schema.Number }),
    success: Item,
    error: ConformanceError,
    stream: true,
  }),
  Rpc.make("conformance.ticker", {
    payload: Schema.Struct({ key: Schema.String, intervalMs: Schema.Number }),
    success: Item,
    stream: true,
  }),
  Rpc.make("conformance.windowed", {
    payload: Schema.Struct({
      key: Schema.String,
      count: Schema.optionalKey(Schema.Number),
      pad: Schema.Number,
    }),
    success: Item,
    stream: true,
  }),
  Rpc.make("conformance.probe", {
    payload: Schema.Struct({ key: Schema.String }),
    success: Schema.Struct({ produced: Schema.Number, cancelled: Schema.Boolean }),
  }),
);

// --- harness ----------------------------------------------------------------------------

const results = [];
const check = (name, condition, detail) => {
  results.push({ name, ok: Boolean(condition) });
  console.log(`${condition ? "PASS" : "FAIL"}  ${name}${detail === undefined ? "" : `  (${detail})`}`);
};

const dieReason = (exit) =>
  Exit.isFailure(exit) ? exit.cause.reasons.find((reason) => reason._tag === "Die") : undefined;
const defectText = (defect) =>
  typeof defect === "string" ? defect : defect instanceof Error ? defect.message : JSON.stringify(defect);

const counters = { pongs: 0, pings: 0, connects: 0, disconnects: 0 };
const hooks = RpcClient.ConnectionHooks.of({
  onConnect: Effect.sync(() => void counters.connects++),
  onDisconnect: Effect.sync(() => void counters.disconnects++),
  onPing: Effect.sync(() => void counters.pings++),
  onPong: Effect.sync(() => void counters.pongs++),
});

// What crosses the wire, seen from the client's socket (observation only: frames are
// passed through untouched). Per request: chunks received minus acks sent, at most.
const wire = { tagOf: new Map(), outstanding: new Map(), maxOutstanding: new Map(), defects: [], parseErrors: 0, pings: 0, pongs: 0 };
const framesOf = (data) => {
  try {
    const value = JSON.parse(String(data));
    return Array.isArray(value) ? value : [value];
  } catch {
    wire.parseErrors++;
    return [];
  }
};
const observedWebSocket = (socketUrl, protocols) => {
  const ws = new globalThis.WebSocket(socketUrl, protocols);
  // Registered before Effect's listener, so a chunk is counted before the client acks it.
  ws.addEventListener("message", (event) => {
    for (const frame of framesOf(event.data)) {
      if (frame._tag === "Defect") wire.defects.push(frame.defect);
      if (frame._tag === "Pong") wire.pongs++;
      if (frame._tag !== "Chunk") continue;
      const id = String(frame.requestId);
      const outstanding = (wire.outstanding.get(id) ?? 0) + 1;
      wire.outstanding.set(id, outstanding);
      wire.maxOutstanding.set(id, Math.max(wire.maxOutstanding.get(id) ?? 0, outstanding));
    }
  });
  const send = ws.send.bind(ws);
  ws.send = (data) => {
    for (const frame of framesOf(data)) {
      if (frame._tag === "Request") wire.tagOf.set(String(frame.id), frame.tag);
      if (frame._tag === "Ping") wire.pings++;
      if (frame._tag === "Ack") {
        const id = String(frame.requestId);
        wire.outstanding.set(id, (wire.outstanding.get(id) ?? 0) - 1);
      }
    }
    return send(data);
  };
  return ws;
};
const maxOutstandingFor = (predicate) => {
  let max = 0;
  for (const [id, value] of wire.maxOutstanding) {
    if (predicate(wire.tagOf.get(id) ?? "", id)) max = Math.max(max, value);
  }
  return max;
};

const protocolLayer = Layer.effect(
  RpcClient.Protocol,
  RpcClient.makeProtocolSocket({ retryTransientErrors: false, retryPolicy: Schedule.recurs(0) }),
).pipe(
  Layer.provide(
    Layer.mergeAll(
      Socket.layerWebSocket(url, { openTimeout: "15 seconds" }).pipe(
        Layer.provide(Layer.succeed(Socket.WebSocketConstructor, observedWebSocket)),
      ),
      RpcSerialization.layerJson,
      Layer.succeed(RpcClient.ConnectionHooks, hooks),
    ),
  ),
);

// --- the checks -------------------------------------------------------------------------

const program = Effect.gen(function* () {
  const client = yield* RpcClient.make(Group);

  // A stream left running through the error checks: a connection-wide Defect would kill it.
  const background = yield* Ref.make(0);
  const backgroundFiber = yield* client["conformance.ticker"]({ key: "background", intervalMs: 5 }).pipe(
    Stream.runForEach(() => Ref.update(background, (n) => n + 1)),
    Effect.forkChild,
  );
  yield* Effect.sleep("50 millis");

  // Unary success.
  const echo = yield* client["conformance.echo"]({ text: "héllo, monde" });
  check("unary success", echo.text === "héllo, monde" && echo.length === 12, JSON.stringify(echo));

  // Typed failure.
  const failed = yield* Effect.flip(client["conformance.fail"]({}));
  check(
    "unary typed failure (Fail)",
    failed instanceof ConformanceError && failed.code === 7 && failed.message === "requested failure",
    `${failed?._tag} ${failed?.message}`,
  );

  // Die from a handler error, and from a panic: a per-request Exit, not a Defect.
  const died = yield* Effect.exit(client["conformance.die"]({}));
  check("unary die (Die)", defectText(dieReason(died)?.defect) === "conformance die");
  const panicked = yield* Effect.exit(client["conformance.panic"]({}));
  check("handler panic becomes Die", defectText(dieReason(panicked)?.defect) === "conformance panic");

  // Scope check.
  const forbidden = yield* Effect.flip(client["conformance.forbidden"]({}));
  check(
    "missing scope fails with EnvironmentAuthorizationError",
    forbidden instanceof EnvironmentAuthorizationError && forbidden.requiredScope === "access:write",
    forbidden?.message,
  );

  // Bad payload and unknown method: per-request Die.
  const badPayload = yield* Effect.exit(client["conformance.decode"]({ n: "not a number" }));
  check("undecodable payload is a per-request Die", dieReason(badPayload) !== undefined, defectText(dieReason(badPayload)?.defect));
  const unknown = yield* Effect.exit(client["conformance.unknown"]({}));
  check(
    "unknown tag is a per-request Die",
    defectText(dieReason(unknown)?.defect) === "Unknown request tag: conformance.unknown",
  );

  // The background stream survived every failure above.
  const before = yield* Ref.get(background);
  yield* Effect.sleep("100 millis");
  const after = yield* Ref.get(background);
  check("no connection-wide Defect (a concurrent stream kept flowing)", after > before && before > 0, `${before} -> ${after}`);
  yield* Fiber.interrupt(backgroundFiber);

  // Many concurrent requests on one socket.
  const many = yield* Effect.forEach(
    Array.from({ length: 200 }, (_, i) => i),
    (i) => client["conformance.echo"]({ text: `#${i}` }),
    { concurrency: "unbounded" },
  );
  check("200 concurrent requests", many.every((r, i) => r.text === `#${i}`));

  // Stream with many chunks, consumed slowly: the server must wait for acks.
  {
    const total = 300;
    let consumed = 0;
    let inOrder = true;
    yield* client["conformance.count"]({ key: "paced", count: total, spaced: true }).pipe(
      Stream.runForEach((item) =>
        Effect.gen(function* () {
          if (item.index !== consumed) inOrder = false;
          consumed++;
          yield* Effect.sleep("2 millis");
        }),
      ),
    );
    check("stream: 300 chunks, in order, then success", consumed === total && inOrder, `${consumed} items`);
    const chunks = [...wire.tagOf].filter(([, tag]) => tag === "conformance.count").length;
    check(
      "stream: one chunk on the wire at a time (each waits for the client's Ack)",
      maxOutstandingFor((tag) => tag === "conformance.count") === 1,
      `max unacknowledged ${maxOutstandingFor((tag) => tag === "conformance.count")} over ${chunks} request(s)`,
    );
  }

  // Ready values are batched into big chunks.
  {
    const items = yield* Stream.runCollect(
      client["conformance.count"]({ key: "batched", count: 5000, spaced: false }),
    );
    const list = Array.from(items);
    check("stream: 5000 batched values", list.length === 5000 && list.every((it, i) => it.index === i));
  }

  // Stream that fails with a typed error after 3 items.
  {
    const seen = [];
    const error = yield* client["conformance.failAfter"]({ count: 3 }).pipe(
      Stream.runForEach((item) => Effect.sync(() => seen.push(item.index))),
      Effect.flip,
    );
    check(
      "stream: typed failure after 3 items",
      seen.join(",") === "0,1,2" && error instanceof ConformanceError && error.message === "stream failed",
    );
  }

  // Interrupt mid-stream: taking 5 items closes the stream, the client sends Interrupt,
  // the server drops the handler.
  {
    const taken = yield* Stream.runCollect(
      client["conformance.ticker"]({ key: "interrupted", intervalMs: 5 }).pipe(Stream.take(5)),
    );
    yield* Effect.sleep("200 millis");
    const first = yield* client["conformance.probe"]({ key: "interrupted" });
    yield* Effect.sleep("150 millis");
    const second = yield* client["conformance.probe"]({ key: "interrupted" });
    check(
      "stream: interrupt mid-stream cancels the handler",
      Array.from(taken).length === 5 && first.cancelled && second.produced === first.produced,
      JSON.stringify(second),
    );
  }

  // Interrupting a pending unary call.
  {
    const fiber = yield* client["conformance.ticker"]({ key: "fiber", intervalMs: 5 }).pipe(
      Stream.runDrain,
      Effect.forkChild,
    );
    yield* Effect.sleep("60 millis");
    yield* Fiber.interrupt(fiber);
    yield* Effect.sleep("100 millis");
    const probe = yield* client["conformance.probe"]({ key: "fiber" });
    check("stream: fiber interrupt cancels the handler", probe.cancelled && probe.produced > 0, JSON.stringify(probe));
  }

  // Windowed stream (terminal-style acks): complete, in order, bounded lead.
  {
    const total = 200;
    let consumed = 0;
    let inOrder = true;
    yield* client["conformance.windowed"]({ key: "windowed", count: total, pad: 2000 }).pipe(
      Stream.runForEach((item) =>
        Effect.gen(function* () {
          if (item.index !== consumed || item.pad?.length !== 2000) inOrder = false;
          consumed++;
          yield* Effect.sleep("2 millis");
        }),
      ),
    );
    check("windowed stream: 200 chunks, in order, then success", consumed === total && inOrder);
    const windowedMax = maxOutstandingFor((tag) => tag === "conformance.windowed");
    check("windowed stream: at most 8 chunks unacknowledged", windowedMax <= 8, `max unacknowledged ${windowedMax}`);
    // ~20 KB chunks: the 64 KiB byte limit fills the window after 4 chunks.
    const big = yield* Stream.runCollect(
      client["conformance.windowed"]({ key: "windowed-big", count: 40, pad: 20000 }),
    );
    const bigId = [...wire.tagOf].filter(([, tag]) => tag === "conformance.windowed").at(-1)?.[0];
    const bigMax = wire.maxOutstanding.get(bigId) ?? 0;
    check(
      "windowed stream: 64 KiB byte limit (at most 4 chunks of 20 KB unacknowledged)",
      Array.from(big).length === 40 && bigMax <= 4,
      `max unacknowledged ${bigMax}`,
    );
    const taken = yield* Stream.runCollect(
      client["conformance.windowed"]({ key: "windowed-int", pad: 100 }).pipe(Stream.take(30)),
    );
    yield* Effect.sleep("150 millis");
    const probe = yield* client["conformance.probe"]({ key: "windowed-int" });
    check("windowed stream: interrupt mid-stream", Array.from(taken).length === 30 && probe.cancelled, JSON.stringify(probe));
  }

  // Ping/pong over time, while a stream is held back by a slow consumer (20 items a
  // second): the server spends that time waiting for acks on it, and pongs must not
  // wait behind it. (The consumer must not be much slower: the client's socket reader
  // stops while a full stream buffer, up to 2×16 items, drains, and it cannot read
  // pongs meanwhile, whatever the server does.)
  if (pingSeconds > 0) {
    let slowConsumed = 0;
    const slow = yield* client["conformance.ticker"]({ key: "slow", intervalMs: 1 }).pipe(
      Stream.runForEach(() => Effect.sleep("50 millis").pipe(Effect.andThen(Effect.sync(() => slowConsumed++)))),
      Effect.forkChild,
    );
    const pongsBefore = counters.pongs;
    const pingsBefore = wire.pings;
    console.log(`...  waiting ${pingSeconds}s for ping/pong`);
    yield* Effect.sleep(`${pingSeconds} seconds`);
    const pongs = counters.pongs - pongsBefore;
    const pings = wire.pings - pingsBefore;
    const probe = yield* client["conformance.probe"]({ key: "slow" });
    const stillUp = yield* client["conformance.echo"]({ text: "still here" });
    // The client pings every 5 s and gives up after 3 missed pongs.
    check(
      `ping/pong for ${pingSeconds}s, socket kept alive`,
      pings >= Math.floor(pingSeconds / 5) - 1 &&
        pongs >= pings - 1 &&
        wire.pongs === counters.pongs &&
        counters.disconnects === 0 &&
        stillUp.text === "still here",
      `${pings} pings, ${pongs} pongs in the window; ${wire.pings}/${wire.pongs} in all; ${counters.disconnects} disconnects`,
    );
    // The ticker could produce ~1000 items a second; acks hold it to what the client took.
    check(
      "slow consumer: the server stayed paced by acks",
      probe.produced <= slowConsumed + 40 && maxOutstandingFor((tag) => tag === "conformance.ticker") === 1,
      `produced ${probe.produced}, consumed ${slowConsumed}`,
    );
    yield* Fiber.interrupt(slow);
  }

  check("no Defect frame during the run", wire.defects.length === 0 && wire.parseErrors === 0, JSON.stringify(wire.defects));
  check("one connection for the whole run", counters.connects === 1 && counters.disconnects === 0);
}).pipe(Effect.scoped, Effect.provide(protocolLayer));

const exit = await Effect.runPromiseExit(program);
if (Exit.isFailure(exit)) {
  console.error("FAIL  the run itself failed:", JSON.stringify(exit.cause.reasons.map((r) => r.error ?? r.defect ?? r), null, 1));
  process.exit(1);
}
const failures = results.filter((r) => !r.ok);
console.log(`\n${results.length - failures.length}/${results.length} checks passed`);
process.exit(failures.length === 0 ? 0 : 1);
