// End-to-end: the REAL Effect RPC client with the REAL contracts (`WsRpcGroup` of
// packages/contracts/src/rpc.ts, which decodes every reply strictly) against the Rust terminal
// RPC handlers, over a WebSocket, on a real PTY. Same client stack as the web app
// (`client-runtime/src/rpc/session.ts`).
//
//   node terminal-e2e.mjs ws://127.0.0.1:PORT/ws /abs/path/to/contracts/src/rpc.ts /abs/cwd
//
// Exits 0 when every check passes. Run by `tests/e2e_ts_client.rs`.

import * as Effect from "effect/Effect";
import * as Exit from "effect/Exit";
import * as Fiber from "effect/Fiber";
import * as Layer from "effect/Layer";
import * as Schedule from "effect/Schedule";
import * as Stream from "effect/Stream";
import * as RpcClient from "effect/unstable/rpc/RpcClient";
import * as RpcSerialization from "effect/unstable/rpc/RpcSerialization";
import * as Socket from "effect/unstable/socket/Socket";
import { pathToFileURL } from "node:url";

const [url, contractsRpc, cwd] = process.argv.slice(2);
if (!url || !contractsRpc || !cwd) {
  console.error("usage: node terminal-e2e.mjs ws://HOST/ws /path/to/contracts/src/rpc.ts /cwd");
  process.exit(2);
}
const { WsRpcGroup } = await import(pathToFileURL(contractsRpc).href);

const results = [];
const check = (name, condition, detail) => {
  results.push({ name, ok: Boolean(condition) });
  console.log(`${condition ? "PASS" : "FAIL"}  ${name}${detail === undefined ? "" : `  (${detail})`}`);
};

// Wire observation (frames pass through untouched): chunks received minus acks sent.
const wire = { tagOf: new Map(), outstanding: new Map(), maxOutstanding: new Map(), chunks: new Map(), defects: [], parseErrors: 0 };
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
  ws.addEventListener("message", (event) => {
    for (const frame of framesOf(event.data)) {
      if (frame._tag === "Defect") wire.defects.push(frame.defect);
      if (frame._tag !== "Chunk") continue;
      const id = String(frame.requestId);
      wire.chunks.set(id, (wire.chunks.get(id) ?? 0) + 1);
      const outstanding = (wire.outstanding.get(id) ?? 0) + 1;
      wire.outstanding.set(id, outstanding);
      wire.maxOutstanding.set(id, Math.max(wire.maxOutstanding.get(id) ?? 0, outstanding));
    }
  });
  const send = ws.send.bind(ws);
  ws.send = (data) => {
    for (const frame of framesOf(data)) {
      if (frame._tag === "Request") wire.tagOf.set(String(frame.id), frame.tag);
      if (frame._tag === "Ack") {
        const id = String(frame.requestId);
        wire.outstanding.set(id, (wire.outstanding.get(id) ?? 0) - 1);
      }
    }
    return send(data);
  };
  return ws;
};
const requestIdsFor = (tag) => [...wire.tagOf].filter(([, t]) => t === tag).map(([id]) => id);
const maxOutstandingFor = (tag) => Math.max(0, ...requestIdsFor(tag).map((id) => wire.maxOutstanding.get(id) ?? 0));
const chunksFor = (tag) => requestIdsFor(tag).reduce((total, id) => total + (wire.chunks.get(id) ?? 0), 0);

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
    ),
  ),
);

const waitUntil = (label, predicate, timeoutMs = 10_000) =>
  Effect.gen(function* () {
    const deadline = Date.now() + timeoutMs;
    while (!predicate()) {
      if (Date.now() > deadline) return yield* Effect.die(new Error(`timed out waiting for ${label}`));
      yield* Effect.sleep("20 millis");
    }
  });

const program = Effect.gen(function* () {
  const client = yield* RpcClient.make(WsRpcGroup);
  const threadId = "e2e-thread";
  const terminalId = "term-1";
  const session = { threadId, terminalId };

  // Global events and metadata, decoded with the contract schemas.
  const allEvents = [];
  const eventsFiber = yield* client.subscribeTerminalEvents({}).pipe(
    Stream.runForEach((event) => Effect.sync(() => allEvents.push(event))),
    Effect.forkChild,
  );
  const metadata = [];
  const metadataFiber = yield* client.subscribeTerminalMetadata({}).pipe(
    Stream.runForEach((event) => Effect.sync(() => metadata.push(event))),
    Effect.forkChild,
  );
  yield* waitUntil("metadata snapshot", () => metadata.length > 0);
  check("subscribeTerminalMetadata starts with a snapshot", metadata[0]?.type === "snapshot" && Array.isArray(metadata[0].terminals));

  // Open.
  const opened = yield* client["terminal.open"]({ ...session, cwd, cols: 100, rows: 24 });
  check(
    "terminal.open returns a running snapshot",
    opened.status === "running" && opened.pid > 0 && opened.label === "Terminal 1" && opened.cwd === cwd && opened.worktreePath === null,
    JSON.stringify({ status: opened.status, label: opened.label, sequence: opened.sequence }),
  );

  // Attach: snapshot first, then live events, consumed slowly.
  const attached = [];
  let output = "";
  const attachFiber = yield* client["terminal.attach"]({ ...session, cols: 100, rows: 24 }).pipe(
    Stream.runForEach((event) =>
      Effect.gen(function* () {
        attached.push(event);
        if (event.type === "output") {
          output += event.data;
          // A slow consumer, so unacknowledged chunks pile up to the window.
          yield* Effect.sleep("1 millis");
        }
      }),
    ),
    Effect.forkChild,
  );
  yield* waitUntil("attach snapshot", () => attached.length > 0);
  check("terminal.attach starts with the snapshot", attached[0].type === "snapshot" && attached[0].snapshot.pid === opened.pid);

  // Type a command.
  yield* client["terminal.write"]({ ...session, data: "echo e2e-$((6*7))\n" });
  yield* waitUntil("command output", () => output.includes("e2e-42"));
  check("terminal.write runs a command; its output streams back", output.includes("e2e-42"));

  // Resize.
  yield* client["terminal.resize"]({ ...session, cols: 132, rows: 40 });
  yield* client["terminal.write"]({ ...session, data: "echo size:$(stty size)\n" });
  yield* waitUntil("stty size", () => output.includes("size:40 132"));
  check("terminal.resize resizes the PTY", output.includes("size:40 132"));

  // A flood of output: the server runs ahead of the acks, within the window.
  yield* client["terminal.write"]({
    ...session,
    data: "i=0; while [ $i -lt 4000 ]; do echo flood-$i-abcdefghijklmnopqrstuvwxyz0123456789; i=$((i+1)); done; echo flood-$((1000+1))-done\n",
  });
  yield* waitUntil("flood", () => output.includes("flood-1001-done"), 30_000);
  const attachMax = maxOutstandingFor("terminal.attach");
  check(
    "terminal.attach: windowed acks (more than one, at most 8 chunks unacknowledged)",
    attachMax > 1 && attachMax <= 8,
    `max unacknowledged ${attachMax} over ${chunksFor("terminal.attach")} chunks`,
  );
  const lines = output.split("\r\n").filter((line) => /^flood-\d+-a/.test(line));
  check(
    "flood output complete and in order",
    lines.length === 4000 && lines.every((line, index) => line.startsWith(`flood-${index}-`)),
    `${lines.length} lines`,
  );
  const sequences = attached.filter((e) => e.type === "output").map((e) => e.sequence);
  check(
    "output sequences strictly increase",
    sequences.every((s, i) => i === 0 || s > sequences[i - 1]),
  );
  const eventsMax = maxOutstandingFor("subscribeTerminalEvents");
  check("subscribeTerminalEvents: at most 8 chunks unacknowledged", eventsMax >= 1 && eventsMax <= 8, `max ${eventsMax}`);
  check("subscribeTerminalMetadata: one chunk at a time", maxOutstandingFor("subscribeTerminalMetadata") === 1);

  // Clear.
  yield* client["terminal.clear"](session);
  yield* waitUntil("cleared", () => attached.some((e) => e.type === "cleared"));
  check("terminal.clear publishes cleared", attached.some((e) => e.type === "cleared"));

  // Restart.
  const restarted = yield* client["terminal.restart"]({ ...session, cwd, cols: 90, rows: 20 });
  check("terminal.restart returns a fresh snapshot", restarted.history === "" && restarted.status === "running" && restarted.pid !== opened.pid);
  yield* waitUntil("restarted", () => attached.some((e) => e.type === "restarted"));
  check("the attach stream sees restarted", attached.some((e) => e.type === "restarted"));
  yield* client["terminal.write"]({ ...session, data: "exit 7\n" });
  yield* waitUntil("exited", () => attached.some((e) => e.type === "exited"));
  const exited = attached.find((e) => e.type === "exited");
  check("exit code and signal reach the client", exited.exitCode === 7 && exited.exitSignal === 0, JSON.stringify(exited));

  // Reattach with restartIfNotRunning.
  const reattached = yield* client["terminal.attach"]({ ...session, cwd, restartIfNotRunning: true }).pipe(
    Stream.take(1),
    Stream.runCollect,
  );
  const reattachSnapshot = Array.from(reattached)[0];
  check(
    "terminal.attach with restartIfNotRunning starts a new shell",
    reattachSnapshot.type === "snapshot" && reattachSnapshot.snapshot.status === "running",
  );

  // Typed failures.
  const lookup = yield* Effect.flip(client["terminal.write"]({ threadId, terminalId: "missing", data: "x" }));
  check(
    "unknown terminal fails with TerminalSessionLookupError",
    lookup._tag === "TerminalSessionLookupError" && lookup.message === `Unknown terminal thread: ${threadId}, terminal: missing`,
    lookup.message,
  );
  const missingCwd = yield* Effect.flip(client["terminal.open"]({ threadId, terminalId: "term-9", cwd: `${cwd}/missing` }));
  check("missing cwd fails with TerminalCwdNotFoundError", missingCwd._tag === "TerminalCwdNotFoundError" && missingCwd.cwd === `${cwd}/missing`);
  const noCwd = yield* Effect.flip(
    client["terminal.attach"]({ threadId, terminalId: "term-8" }).pipe(Stream.runCollect),
  );
  check("attach without cwd to an unknown terminal fails", noCwd._tag === "TerminalSessionLookupError");

  // Close.
  yield* client["terminal.close"]({ threadId, deleteHistory: true });
  yield* waitUntil("closed", () => attached.some((e) => e.type === "closed"));
  check("terminal.close publishes closed", attached.some((e) => e.type === "closed"));
  yield* waitUntil("metadata remove", () => metadata.some((e) => e.type === "remove"));
  check(
    "metadata: upserts then a removal",
    metadata.some((e) => e.type === "upsert" && e.terminal.threadId === threadId) &&
      metadata.some((e) => e.type === "remove" && e.threadId === threadId),
  );
  check(
    "global events cover the lifecycle",
    ["started", "output", "cleared", "restarted", "exited", "closed"].every((type) => allEvents.some((e) => e.type === type)),
    [...new Set(allEvents.map((e) => e.type))].join(","),
  );

  yield* Fiber.interrupt(attachFiber);
  yield* Fiber.interrupt(eventsFiber);
  yield* Fiber.interrupt(metadataFiber);
  check("no Defect frame", wire.defects.length === 0 && wire.parseErrors === 0, JSON.stringify(wire.defects));
}).pipe(Effect.scoped, Effect.provide(protocolLayer));

const exit = await Effect.runPromiseExit(program);
if (Exit.isFailure(exit)) {
  console.error("FAIL  the run itself failed:", String(exit.cause));
  process.exit(1);
}
const failed = results.filter((r) => !r.ok);
console.log(`${results.length - failed.length}/${results.length} checks passed`);
process.exit(failed.length === 0 ? 0 : 1);
