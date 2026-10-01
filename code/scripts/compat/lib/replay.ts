/**
 * Replays the client side of a recording against another backend and records what it answers,
 * in the same format and with the same connection names, so `diff.ts` can compare the two.
 *
 * - WebSocket: the recorded Requests and Interrupts are re-sent with the same request ids; recorded
 *   Acks and Pings are dropped and the replayer Acks every Chunk itself.
 * - Causality: before each client event, the replayer waits (bounded) until the backend has sent at
 *   least as many stream values / Exits as the recording had at that point, so requests that
 *   depended on earlier answers are not sent too early.
 * - Substitution: server-generated values (pairing credentials, session cookies, bearer tokens,
 *   ws tickets, ids) differ between runs. The replayer learns old→new pairs by walking each
 *   recorded answer in parallel with the replayed one, and rewrites later client frames with
 *   them. The banner credential is mapped through the recording's meta `bootstrapCredential`.
 *   Recordings made with redaction (`<redacted>`) fall back to the latest credential seen.
 */
import { http as httpRequest } from "./client.ts";
import type { RecordedEvent, Scrub } from "./recording.ts";
import { RecordingWriter, frameFromText } from "./recording.ts";
import { WebSocket, rawDataToString, type RawData } from "./ws.ts";

export interface ReplayTarget {
  readonly httpUrl: string;
  readonly wsUrl: string;
  /** The target's banner credential; replaces the recording's `bootstrapCredential`. */
  readonly bootstrapCredential?: string;
}

export interface ReplayOptions {
  readonly events: ReadonlyArray<RecordedEvent>;
  readonly target: ReplayTarget;
  readonly outFile?: string;
  /** Replacements applied to the written replay (same as the capture's, see machineScrubs). */
  readonly scrub?: ReadonlyArray<Scrub>;
  /** Initial old→new string substitutions (e.g. a workspace path). */
  readonly substitutions?: Readonly<Record<string, string>>;
  /** Path aliases written into the replay's meta (for normalization). */
  readonly pathAliases?: Readonly<Record<string, string>>;
  readonly causalTimeoutMs?: number;
  readonly settleMs?: number;
  readonly log?: (line: string) => void;
}

export interface ReplayResult {
  readonly events: Array<RecordedEvent>;
  readonly warnings: Array<string>;
}

interface Progress {
  values: number;
  exited: boolean;
}

const looksLikeToken = (value: string) =>
  value.length >= 8 &&
  value.length <= 8192 &&
  !/\s/.test(value) &&
  !/^\d{4}-\d{2}-\d{2}T/.test(value) &&
  value !== "<redacted>";

const HEADER_SKIP = new Set([
  "host",
  "content-length",
  "connection",
  "accept-encoding",
  "transfer-encoding",
  "keep-alive",
]);

export const replayRecording = async (options: ReplayOptions): Promise<ReplayResult> => {
  const log = options.log ?? (() => {});
  const warnings: Array<string> = [];
  const warn = (message: string) => {
    warnings.push(message);
    log(`warning: ${message}`);
  };
  const writer = new RecordingWriter(options.outFile, {
    redact: false,
    keepInMemory: true,
    ...(options.scrub ? { scrub: options.scrub } : {}),
  });
  const causalTimeout = options.causalTimeoutMs ?? 5_000;
  const subs = new Map<string, string>(Object.entries(options.substitutions ?? {}));
  let latestCookie: string | undefined;
  let latestBearer: string | undefined;

  // ---- substitutions ------------------------------------------------------------------------
  const substitute = (value: unknown): unknown => {
    if (typeof value === "string") {
      const exact = subs.get(value);
      if (exact !== undefined) return exact;
      let out = value;
      for (const [from, to] of subs) {
        if (from.length >= 8 && out.includes(from)) out = out.split(from).join(to);
      }
      return out;
    }
    if (Array.isArray(value)) return value.map(substitute);
    if (value !== null && typeof value === "object") {
      const out: Record<string, unknown> = {};
      for (const [k, v] of Object.entries(value)) out[k] = substitute(v);
      return out;
    }
    return value;
  };
  const learn = (recorded: unknown, replayed: unknown) => {
    if (typeof recorded === "string" && typeof replayed === "string") {
      if (recorded !== replayed && looksLikeToken(recorded) && !subs.has(recorded)) {
        subs.set(recorded, replayed);
      }
      return;
    }
    if (Array.isArray(recorded) && Array.isArray(replayed)) {
      const n = Math.min(recorded.length, replayed.length);
      for (let i = 0; i < n; i++) learn(recorded[i], replayed[i]);
      return;
    }
    if (recorded && replayed && typeof recorded === "object" && typeof replayed === "object") {
      for (const [k, v] of Object.entries(recorded)) {
        if (k in (replayed as object)) learn(v, (replayed as Record<string, unknown>)[k]);
      }
    }
  };
  const learnCookie = (recorded: string | undefined, replayed: string | undefined) => {
    if (!replayed) return;
    const [rName, rValue] = (replayed.split(";", 1)[0] ?? "").split("=", 2);
    latestCookie = `${rName}=${rValue}`;
    if (!recorded) return;
    const [oName, oValue] = (recorded.split(";", 1)[0] ?? "").split("=", 2);
    if (oName && rName && oName !== rName) subs.set(oName, rName);
    if (oValue && rValue && oValue !== "<redacted>") subs.set(oValue, rValue);
  };

  // ---- recorded state, for causal waits and learning ---------------------------------------
  const recordedValues = new Map<string, { values: Array<unknown>; exit: unknown }>();
  const recordedHttp = new Map<
    string,
    { status?: number; headers?: Record<string, unknown>; body?: unknown }
  >();
  for (const event of options.events) {
    if (event.dir !== "s2c") continue;
    if (event.conn.startsWith("http")) {
      recordedHttp.set(event.conn, event.frame as never);
      continue;
    }
    for (const m of [event.frame].flat() as Array<Record<string, unknown>>) {
      const key = `${event.conn}:${String(m.requestId)}`;
      const entry = recordedValues.get(key) ?? { values: [], exit: undefined };
      if (m._tag === "Chunk") entry.values.push(...(m.values as Array<unknown>));
      if (m._tag === "Exit") entry.exit = m.exit;
      recordedValues.set(key, entry);
    }
  }
  const expected = new Map<string, Progress>(); // recorded progress so far
  const actual = new Map<string, Progress>(); // replayed progress
  let progressWaiters: Array<() => void> = [];
  const notify = () => {
    const waiters = progressWaiters;
    progressWaiters = [];
    for (const waiter of waiters) waiter();
  };
  const satisfied = () => {
    for (const [key, want] of expected) {
      const got = actual.get(key);
      if (!got) return false;
      if (want.exited && !got.exited) return false;
      if (!got.exited && got.values < want.values) return false;
    }
    return true;
  };
  const waitCausal = async (what: string) => {
    const deadline = Date.now() + causalTimeout;
    while (!satisfied()) {
      const remaining = deadline - Date.now();
      if (remaining <= 0) {
        const lagging = [...expected]
          .filter(([key, want]) => {
            const got = actual.get(key);
            return (
              !got || (want.exited && !got.exited) || (!got.exited && got.values < want.values)
            );
          })
          .map(([key]) => key);
        warn(`causal wait timed out before ${what}; lagging: ${lagging.join(", ")}`);
        // Do not wait for these again.
        for (const key of lagging) expected.delete(key);
        return;
      }
      await new Promise<void>((resolve) => {
        const timer = setTimeout(resolve, Math.min(remaining, 250));
        progressWaiters.push(() => {
          clearTimeout(timer);
          resolve();
        });
      });
    }
  };

  // ---- connections --------------------------------------------------------------------------
  const sockets = new Map<string, WebSocket>();
  const target = new URL(options.target.httpUrl);

  const openWs = async (event: RecordedEvent) => {
    const frame = event.frame as {
      path?: string;
      query?: Record<string, string>;
      headers?: Record<string, string>;
    };
    const url = new URL(frame.path ?? "/ws", options.target.wsUrl);
    for (const [k, v] of Object.entries(frame.query ?? {})) {
      url.searchParams.set(k, String(substitute(v)));
    }
    const headers: Record<string, string> = {};
    const recordedHeaders = frame.headers ?? {};
    for (const name of ["cookie", "authorization", "origin", "user-agent"]) {
      const value = recordedHeaders[name];
      if (typeof value !== "string") continue;
      const replaced = String(substitute(value));
      headers[name] = replaced.includes("<redacted>")
        ? name === "cookie"
          ? (latestCookie ?? "")
          : name === "authorization" && latestBearer
            ? `Bearer ${latestBearer}`
            : replaced
        : replaced;
    }
    writer.write(event.conn, "open", {
      path: url.pathname,
      query: Object.fromEntries(url.searchParams),
      headers,
    });
    const socket = new WebSocket(url.toString(), { headers, perMessageDeflate: true });
    await new Promise<void>((resolve) => {
      socket.once("open", () => resolve());
      socket.once("unexpected-response", (_req: unknown, res: { statusCode?: number }) => {
        writer.write(event.conn, "close", { by: "backend", rejectedStatus: res.statusCode });
        warn(`${event.conn}: upgrade rejected with ${res.statusCode}`);
        resolve();
      });
      socket.once("error", (error: Error) => {
        warn(`${event.conn}: ${error.message}`);
        resolve();
      });
    });
    if (socket.readyState !== WebSocket.OPEN) return;
    sockets.set(event.conn, socket);
    socket.on("message", (data: RawData) => {
      const text = rawDataToString(data);
      const parsed = frameFromText(text);
      writer.write(event.conn, "s2c", parsed);
      for (const m of [parsed].flat() as Array<Record<string, unknown>>) {
        const key = `${event.conn}:${String(m.requestId)}`;
        const got = actual.get(key) ?? { values: 0, exited: false };
        if (m._tag === "Chunk") {
          const recorded = recordedValues.get(key);
          const values = m.values as Array<unknown>;
          values.forEach((value, i) => learn(recorded?.values[got.values + i], value));
          got.values += values.length;
          socket.send(JSON.stringify({ _tag: "Ack", requestId: m.requestId }));
        } else if (m._tag === "Exit") {
          got.exited = true;
          learn(recordedValues.get(key)?.exit, m.exit);
        }
        actual.set(key, got);
      }
      notify();
    });
    socket.on("close", (code: number, reason: Buffer) => {
      writer.write(event.conn, "close", { by: "backend", code, reason: reason.toString() });
      sockets.delete(event.conn);
    });
  };

  const sendWs = (event: RecordedEvent) => {
    const socket = sockets.get(event.conn);
    const messages = [event.frame].flat() as Array<Record<string, unknown>>;
    const outgoing = messages.filter((m) => m._tag !== "Ack" && m._tag !== "Ping");
    if (outgoing.length === 0) return;
    if (!socket) {
      warn(`${event.conn}: dropped a client frame, socket not open`);
      return;
    }
    for (const m of outgoing) {
      const replaced = m._tag === "Request" ? { ...m, payload: substitute(m.payload) } : m;
      writer.write(event.conn, "c2s", replaced);
      socket.send(JSON.stringify(replaced));
    }
  };

  let mintedBootstrap = false;
  const sendHttp = async (event: RecordedEvent) => {
    const frame = event.frame as {
      method: string;
      url: string;
      headers?: Record<string, string | Array<string>>;
      body?: unknown;
    };
    const headers: Record<string, string> = {};
    for (const [key, value] of Object.entries(frame.headers ?? {})) {
      if (HEADER_SKIP.has(key) || typeof value !== "string") continue;
      let replaced = String(substitute(value));
      if (replaced.includes("<redacted>")) {
        if (key === "cookie" && latestCookie) replaced = latestCookie;
        else if (key === "authorization" && latestBearer) replaced = `Bearer ${latestBearer}`;
        else continue;
      }
      headers[key] = replaced;
    }
    let body = substitute(frame.body);
    const isCredentialExchange =
      frame.url.startsWith("/api/auth/browser-session") || frame.url.startsWith("/oauth/token");
    if (isCredentialExchange && JSON.stringify(body).includes("<redacted>")) {
      const fresh = !mintedBootstrap ? options.target.bootstrapCredential : undefined;
      mintedBootstrap = true;
      if (fresh) body = JSON.parse(JSON.stringify(body).replaceAll("<redacted>", fresh));
      else warn(`${event.conn}: redacted credential in ${frame.url} and none to substitute`);
    }
    const contentType = headers["content-type"] ?? "";
    const init: Parameters<typeof httpRequest>[1] = { method: frame.method, headers };
    if (body !== undefined && frame.method !== "GET" && frame.method !== "HEAD") {
      init.body =
        typeof body === "string"
          ? body
          : contentType.includes("form-urlencoded")
            ? new URLSearchParams(body as Record<string, string>).toString()
            : JSON.stringify(body);
    }
    const url = `${target.origin}${String(substitute(frame.url))}`;
    writer.write(event.conn, "c2s", {
      method: frame.method,
      url: String(substitute(frame.url)),
      headers,
      ...(body !== undefined ? { body } : {}),
    });
    const res = await httpRequest(url, init);
    const parsedBody =
      res.body !== undefined ? res.body : res.text.length > 0 ? res.text : undefined;
    writer.write(event.conn, "s2c", {
      status: res.status,
      headers: {
        ...res.headers,
        ...(res.setCookies.length > 0 ? { "set-cookie": res.setCookies } : {}),
      },
      ...(parsedBody !== undefined ? { body: parsedBody } : {}),
    });
    const recorded = recordedHttp.get(event.conn);
    learn(recorded?.body, res.body);
    const recordedSetCookie = recorded?.headers?.["set-cookie"];
    learnCookie(
      Array.isArray(recordedSetCookie)
        ? String(recordedSetCookie[0])
        : (recordedSetCookie as string | undefined),
      res.setCookies[0],
    );
    const token = (res.body as { access_token?: string } | undefined)?.access_token;
    if (token) latestBearer = token;
  };

  // ---- main loop ----------------------------------------------------------------------------
  const meta = options.events.find(
    (e) => e.dir === "meta" && (e.frame as { bootstrapCredential?: string }).bootstrapCredential,
  );
  const recordedBootstrap = (meta?.frame as { bootstrapCredential?: string } | undefined)
    ?.bootstrapCredential;
  if (recordedBootstrap && options.target.bootstrapCredential) {
    subs.set(recordedBootstrap, options.target.bootstrapCredential);
    mintedBootstrap = true;
  }
  writer.write("meta", "meta", {
    kind: "replay",
    target: options.target.httpUrl,
    startedAt: new Date().toISOString(),
    pathAliases: options.pathAliases ?? {},
  });

  for (const event of options.events) {
    switch (event.dir) {
      case "s2c": {
        if (event.conn.startsWith("http")) break;
        for (const m of [event.frame].flat() as Array<Record<string, unknown>>) {
          const key = `${event.conn}:${String(m.requestId)}`;
          const want = expected.get(key) ?? { values: 0, exited: false };
          if (m._tag === "Chunk") want.values += (m.values as Array<unknown>).length;
          if (m._tag === "Exit") want.exited = true;
          if (m._tag === "Chunk" || m._tag === "Exit") expected.set(key, want);
        }
        break;
      }
      case "open":
        if (!event.conn.startsWith("ws")) break;
        await waitCausal(`${event.conn} open`);
        await openWs(event);
        break;
      case "c2s":
        await waitCausal(`${event.conn} c2s`);
        if (event.conn.startsWith("http")) await sendHttp(event);
        else sendWs(event);
        break;
      case "close": {
        const frame = event.frame as { by?: string; code?: number };
        if (frame.by !== "client") break;
        await waitCausal(`${event.conn} close`);
        const socket = sockets.get(event.conn);
        if (socket) {
          writer.write(event.conn, "close", { by: "client", code: frame.code ?? 1000 });
          socket.close(
            frame.code &&
              frame.code >= 1000 &&
              frame.code < 5000 &&
              frame.code !== 1005 &&
              frame.code !== 1006
              ? frame.code
              : 1000,
          );
          sockets.delete(event.conn);
        }
        break;
      }
      default:
        break;
    }
  }
  await waitCausal("end of recording");
  await new Promise((r) => setTimeout(r, options.settleMs ?? 1_000));
  for (const [conn, socket] of sockets) {
    writer.write(conn, "close", { by: "client", code: 1000 });
    socket.close(1000);
  }
  await new Promise((r) => setTimeout(r, 100));
  writer.close();
  return { events: writer.events, warnings };
};
