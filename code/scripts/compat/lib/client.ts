/**
 * Black-box client helpers: plain `fetch` for HTTP and a frame-level Effect-RPC client over
 * WebSocket. They only depend on the wire protocol (plan §1.3), never on server internals, so the
 * same calls run against both backends.
 */
import { WebSocket, rawDataToString, type RawData } from "./ws.ts";

// ---------------------------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------------------------

export interface HttpResult<A = unknown> {
  readonly status: number;
  readonly headers: Record<string, string>;
  /** All Set-Cookie values (fetch folds them otherwise). */
  readonly setCookies: Array<string>;
  readonly text: string;
  readonly body: A;
}

export const http = async <A = unknown>(
  url: string,
  init: RequestInit & { json?: unknown; form?: Record<string, string> } = {},
): Promise<HttpResult<A>> => {
  const headers = new Headers(init.headers);
  let body = init.body;
  if (init.json !== undefined) {
    headers.set("content-type", "application/json");
    body = JSON.stringify(init.json);
  } else if (init.form !== undefined) {
    headers.set("content-type", "application/x-www-form-urlencoded");
    body = new URLSearchParams(init.form).toString();
  }
  const response = await fetch(url, { ...init, headers, body: body ?? null, redirect: "manual" });
  const text = await response.text();
  let parsed: unknown = undefined;
  try {
    parsed = text.length > 0 ? JSON.parse(text) : undefined;
  } catch {
    parsed = undefined;
  }
  const headerObject: Record<string, string> = {};
  response.headers.forEach((value, key) => {
    headerObject[key] = value;
  });
  return {
    status: response.status,
    headers: headerObject,
    setCookies: response.headers.getSetCookie(),
    text,
    body: parsed as A,
  };
};

/** `name=value` of a Set-Cookie header. */
export const cookiePair = (setCookie: string) => setCookie.split(";", 1)[0] ?? setCookie;

export const ADMIN_SCOPES = [
  "orchestration:read",
  "orchestration:operate",
  "terminal:operate",
  "review:write",
  "relay:read",
  "access:read",
  "access:write",
  "relay:write",
] as const;

export const bootstrapBrowserSession = async (
  httpUrl: string,
  credential: string,
  headers?: Record<string, string>,
) => {
  const result = await http<{
    authenticated: boolean;
    sessionMethod: string;
    expiresAt: string;
  }>(`${httpUrl}/api/auth/browser-session`, {
    method: "POST",
    json: { credential },
    ...(headers ? { headers } : {}),
  });
  const setCookie = result.setCookies[0];
  return { ...result, setCookie, cookie: setCookie ? cookiePair(setCookie) : undefined };
};

export const exchangeAccessToken = (
  httpUrl: string,
  credential: string,
  options: {
    scope?: string;
    headers?: Record<string, string>;
    clientMetadata?: { label?: string; deviceType?: string; os?: string };
  } = {},
) =>
  http<{
    access_token?: string;
    issued_token_type?: string;
    token_type?: string;
    expires_in?: number;
    scope?: string;
    _tag?: string;
    code?: string;
    reason?: string;
  }>(`${httpUrl}/oauth/token`, {
    method: "POST",
    ...(options.headers ? { headers: options.headers } : {}),
    form: {
      grant_type: "urn:ietf:params:oauth:grant-type:token-exchange",
      subject_token: credential,
      subject_token_type: "urn:t3:params:oauth:token-type:environment-bootstrap",
      requested_token_type: "urn:ietf:params:oauth:token-type:access_token",
      scope: options.scope ?? ADMIN_SCOPES.join(" "),
      ...(options.clientMetadata?.label ? { client_label: options.clientMetadata.label } : {}),
      ...(options.clientMetadata?.deviceType
        ? { client_device_type: options.clientMetadata.deviceType }
        : {}),
      ...(options.clientMetadata?.os ? { client_os: options.clientMetadata.os } : {}),
    },
  });

/** Mints a one-time pairing credential through the HTTP API (needs an access:write session). */
export const issuePairingCredential = async (
  httpUrl: string,
  adminCookie: string,
  input: { label?: string; scopes?: ReadonlyArray<string> } = {},
) => {
  const result = await http<{ id: string; credential: string; scopes: Array<string> }>(
    `${httpUrl}/api/auth/pairing-token`,
    { method: "POST", headers: { cookie: adminCookie }, json: input },
  );
  if (result.status !== 200) {
    throw new Error(`pairing-token failed: ${result.status} ${result.text}`);
  }
  return result.body;
};

// ---------------------------------------------------------------------------------------------
// WebSocket RPC (frame level)
// ---------------------------------------------------------------------------------------------

export type ExitEncoded =
  | { _tag: "Success"; value: unknown }
  | { _tag: "Failure"; cause: Array<{ _tag: string; error?: unknown; defect?: unknown }> };

export interface StreamHandle {
  readonly id: number;
  readonly tag: string;
  readonly values: Array<unknown>;
  exit: ExitEncoded | undefined;
  /** Resolves once at least `n` stream values arrived (rejects on Exit or timeout). */
  waitForValues(n: number, timeoutMs?: number): Promise<Array<unknown>>;
  /** Resolves with the Exit. */
  waitForExit(timeoutMs?: number): Promise<ExitEncoded>;
}

interface Pending {
  tag: string;
  values: Array<unknown>;
  exit: ExitEncoded | undefined;
  waiters: Array<() => void>;
}

export interface RpcConnectOptions {
  readonly cookie?: string;
  readonly headers?: Record<string, string>;
  /** Send an Ack after each Chunk (what the real client does). Default true. */
  readonly autoAck?: boolean;
}

/**
 * A minimal Effect-RPC JSON client: one JSON message per text frame, numeric request ids,
 * Ack after each Chunk. It keeps every received frame for wire-level assertions.
 */
export class RawRpcClient {
  readonly frames: Array<unknown> = [];
  private readonly socket: WebSocket;
  private readonly pending = new Map<string, Pending>();
  private nextId = 0;
  private readonly autoAck: boolean;
  closed: { code: number; reason: string } | undefined;
  private closeWaiters: Array<() => void> = [];

  private constructor(socket: WebSocket, autoAck: boolean) {
    this.socket = socket;
    this.autoAck = autoAck;
    socket.on("message", (data: RawData) => this.onMessage(rawDataToString(data)));
    socket.on("close", (code: number, reason: Buffer) => {
      this.closed = { code, reason: reason.toString() };
      for (const waiter of this.closeWaiters) waiter();
      for (const entry of this.pending.values()) for (const waiter of entry.waiters) waiter();
    });
  }

  static connect(url: string, options: RpcConnectOptions = {}): Promise<RawRpcClient> {
    return new Promise((resolve, reject) => {
      const headers: Record<string, string> = { ...options.headers };
      if (options.cookie) headers.cookie = options.cookie;
      const socket = new WebSocket(url, { headers, perMessageDeflate: true });
      socket.once("open", () => resolve(new RawRpcClient(socket, options.autoAck ?? true)));
      socket.once("unexpected-response", (_req: unknown, res: { statusCode?: number }) =>
        reject(new WsUpgradeRejected(res.statusCode ?? 0)),
      );
      socket.once("error", reject);
    });
  }

  send(message: unknown) {
    this.socket.send(JSON.stringify(message));
  }

  /** Sends a Request and returns its id. */
  request(tag: string, payload: unknown, id: number = this.nextId++): number {
    this.pending.set(String(id), { tag, values: [], exit: undefined, waiters: [] });
    this.send({ _tag: "Request", id, tag, payload, headers: [] });
    return id;
  }

  /** Unary call: resolves with the encoded Exit. */
  async call(tag: string, payload: unknown = {}, timeoutMs = 15_000): Promise<ExitEncoded> {
    return this.stream(tag, payload).waitForExit(timeoutMs);
  }

  /** Unary call that must succeed: resolves with the encoded success value. */
  async callOk<A = unknown>(tag: string, payload: unknown = {}, timeoutMs = 15_000): Promise<A> {
    const exit = await this.call(tag, payload, timeoutMs);
    if (exit._tag !== "Success") throw new Error(`${tag} failed: ${JSON.stringify(exit)}`);
    return exit.value as A;
  }

  stream(tag: string, payload: unknown = {}): StreamHandle {
    const id = this.request(tag, payload);
    const entry = this.pending.get(String(id))!;
    const wait = (done: () => boolean, timeoutMs: number, what: string) =>
      new Promise<void>((resolve, reject) => {
        if (done()) return resolve();
        const timer = setTimeout(() => {
          entry.waiters = entry.waiters.filter((w) => w !== check);
          reject(new Error(`${tag}#${id}: timed out waiting for ${what}`));
        }, timeoutMs);
        const check = () => {
          if (done()) {
            clearTimeout(timer);
            entry.waiters = entry.waiters.filter((w) => w !== check);
            resolve();
          } else if (this.closed) {
            clearTimeout(timer);
            reject(new Error(`${tag}#${id}: socket closed (${this.closed.code}) before ${what}`));
          }
        };
        entry.waiters.push(check);
      });
    const handle: StreamHandle = {
      id,
      tag,
      values: entry.values,
      get exit() {
        return entry.exit;
      },
      set exit(_value) {},
      waitForValues: async (n, timeoutMs = 15_000) => {
        await wait(
          () => entry.values.length >= n || entry.exit !== undefined,
          timeoutMs,
          `${n} values`,
        );
        if (entry.values.length < n) {
          throw new Error(
            `${tag}#${id}: exited after ${entry.values.length} values: ${JSON.stringify(entry.exit)}`,
          );
        }
        return entry.values;
      },
      waitForExit: async (timeoutMs = 15_000) => {
        await wait(() => entry.exit !== undefined, timeoutMs, "Exit");
        return entry.exit!;
      },
    };
    return handle;
  }

  interrupt(id: number) {
    this.send({ _tag: "Interrupt", requestId: id });
  }

  ping() {
    this.send({ _tag: "Ping" });
  }

  close(code = 1000): Promise<void> {
    return new Promise((resolve) => {
      if (this.closed) return resolve();
      this.closeWaiters.push(resolve);
      this.socket.close(code);
    });
  }

  private onMessage(text: string) {
    let message: Record<string, unknown>;
    try {
      message = JSON.parse(text);
    } catch {
      this.frames.push({ raw: text });
      return;
    }
    this.frames.push(message);
    const messages = Array.isArray(message) ? message : [message];
    for (const m of messages as Array<Record<string, unknown>>) {
      const entry = this.pending.get(String(m.requestId));
      if (m._tag === "Chunk" && entry) {
        entry.values.push(...(m.values as Array<unknown>));
        if (this.autoAck) this.send({ _tag: "Ack", requestId: m.requestId });
      } else if (m._tag === "Exit" && entry) {
        entry.exit = m.exit as ExitEncoded;
      } else if (m._tag === "Defect") {
        for (const e of this.pending.values()) {
          e.exit ??= { _tag: "Failure", cause: [{ _tag: "Die", defect: m.defect }] };
        }
      }
      for (const e of this.pending.values()) for (const waiter of e.waiters.slice()) waiter(); // waiters remove themselves
    }
  }
}

export class WsUpgradeRejected extends Error {
  readonly status: number;
  constructor(status: number) {
    super(`WebSocket upgrade rejected with HTTP ${status}`);
    this.status = status;
  }
}
