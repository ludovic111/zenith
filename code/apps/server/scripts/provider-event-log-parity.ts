/**
 * Writes provider event logs with the TypeScript `EventNdjsonLogger`, for the byte-for-byte
 * parity test of the Rust port (`crates/zenith-code/crates/zc-providers/tests/logger.rs`).
 *
 *   node apps/server/scripts/provider-event-log-parity.ts <fixture.json> <outDir>
 *
 * The fixture is `{ clockMs, options, records: [{ stream, threadId, event }] }`. The clock is
 * pinned to `clockMs` and its `sleep` never resolves, so a batch is only written when a buffer
 * limit is reached or the store closes: the output does not depend on timing.
 */
import * as NodeFS from "node:fs";
import * as NodePath from "node:path";

import * as Clock from "effect/Clock";
import * as Effect from "effect/Effect";

import {
  type EventNdjsonStream,
  makeEventNdjsonLogStore,
} from "../src/provider/Layers/EventNdjsonLogger.ts";

interface Fixture {
  readonly clockMs: number;
  readonly options: Record<string, number>;
  readonly records: ReadonlyArray<{
    readonly stream: EventNdjsonStream;
    readonly threadId: string | null;
    readonly event: unknown;
  }>;
}

const [fixturePath, outDir] = process.argv.slice(2);
if (!fixturePath || !outDir) {
  console.error("usage: provider-event-log-parity.ts <fixture.json> <outDir>");
  process.exit(2);
}
const fixture = JSON.parse(NodeFS.readFileSync(fixturePath, "utf8")) as Fixture;

const pinnedClock: Clock.Clock = {
  currentTimeMillisUnsafe: () => fixture.clockMs,
  currentTimeMillis: Effect.succeed(fixture.clockMs),
  monotonicTimeNanosUnsafe: () => BigInt(fixture.clockMs) * 1_000_000n,
  monotonicTimeNanos: Effect.succeed(BigInt(fixture.clockMs) * 1_000_000n),
  currentTimeNanosUnsafe: () => BigInt(fixture.clockMs) * 1_000_000n,
  currentTimeNanos: Effect.succeed(BigInt(fixture.clockMs) * 1_000_000n),
  sleep: () => Effect.never,
};

const program = Effect.gen(function* () {
  const store = yield* makeEventNdjsonLogStore(NodePath.join(outDir, "events.log"), fixture.options);
  for (const record of fixture.records) {
    yield* store.logger(record.stream).write(record.event, record.threadId as never);
  }
  yield* store.close();
});

await Effect.runPromise(program.pipe(Effect.provideService(Clock.Clock, pinnedClock)));
