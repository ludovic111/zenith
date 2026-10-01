/**
 * Frame-level validators. They hold no sockets: feed them frames (live from the proxy, or from a
 * recording) and they return issues.
 *
 * Severity:
 *  - error:   the web client would fail to decode this, or it breaks the protocol (Defect frame,
 *             Chunk for a unary RPC, unknown request id, success value on a stream Exit, …).
 *  - warning: accepted by the client but suspicious (an Exit with a `Die` cause, an undeclared
 *             HTTP status, a client payload that does not decode, a non-canonical encoding in
 *             strict mode).
 */
import {
  defectCodec,
  findHttpEndpoint,
  getRpcSpec,
  ZenithEmbedJson,
  type RpcSpec,
} from "./contracts.ts";
import { jsonDiff, preview, toPlainJson } from "./json.ts";

export type Severity = "error" | "warning";

export interface Issue {
  readonly severity: Severity;
  readonly conn: string;
  readonly kind: string;
  readonly message: string;
  readonly tag?: string;
  readonly requestId?: unknown;
  readonly frame?: string;
}

export interface ValidatorOptions {
  /** Re-encode every decoded value and flag differences from the raw JSON (extra keys, …). */
  readonly strict?: boolean;
}

interface PendingRequest {
  readonly tag: string;
  readonly spec: RpcSpec | undefined;
  readonly rawId: unknown;
  chunks: number;
  exited: boolean;
  interrupted: boolean;
}

const idKey = (id: unknown) => `${typeof id}:${String(id)}`;

const canonicalIssue = (
  codec: { encode: (value: unknown) => unknown },
  decoded: unknown,
  raw: unknown,
): string | undefined => {
  const reencoded = toPlainJson(codec.encode(decoded));
  if (reencoded === undefined) return undefined;
  const diffs = jsonDiff(raw, reencoded, "$", [], 5);
  if (diffs.length === 0) return undefined;
  return diffs
    .map((d) => `${d.path}: wire ${preview(d.left, 80)} vs canonical ${preview(d.right, 80)}`)
    .join("; ");
};

export class WsConnectionValidator {
  readonly conn: string;
  readonly options: ValidatorOptions;
  private readonly requests = new Map<string, PendingRequest>();
  readonly stats = { clientFrames: 0, serverFrames: 0, requests: 0, chunks: 0, exits: 0 };

  constructor(conn: string, options: ValidatorOptions = {}) {
    this.conn = conn;
    this.options = options;
  }

  /** The RPC tag of a request id seen on this connection. */
  tagOf(requestId: unknown): string | undefined {
    return this.requests.get(idKey(requestId))?.tag;
  }

  onClientFrame(text: string): Array<Issue> {
    this.stats.clientFrames++;
    const issues: Array<Issue> = [];
    const parsed = this.parse(text, "client", issues);
    if (parsed === undefined) return issues;
    for (const message of Array.isArray(parsed) ? parsed : [parsed]) {
      this.onClientMessage(message, text, issues);
    }
    return issues;
  }

  onServerFrame(text: string): Array<Issue> {
    this.stats.serverFrames++;
    const issues: Array<Issue> = [];
    const parsed = this.parse(text, "server", issues);
    if (parsed === undefined) return issues;
    if (Array.isArray(parsed)) {
      issues.push(this.issue("warning", "batched-frame", "server sent an array of messages", text));
    }
    for (const message of Array.isArray(parsed) ? parsed : [parsed]) {
      this.onServerMessage(message, text, issues);
    }
    return issues;
  }

  /** Requests that never got an Exit (normal for long-lived streams at disconnect). */
  openRequests(): Array<{ tag: string; requestId: unknown; chunks: number }> {
    return [...this.requests.values()]
      .filter((r) => !r.exited && !r.interrupted)
      .map((r) => ({ tag: r.tag, requestId: r.rawId, chunks: r.chunks }));
  }

  private parse(text: string, side: string, issues: Array<Issue>): unknown {
    try {
      return JSON.parse(text);
    } catch {
      issues.push(this.issue("error", `${side}-invalid-json`, "frame is not JSON", text));
      return undefined;
    }
  }

  private issue(
    severity: Severity,
    kind: string,
    message: string,
    frame?: string,
    extra?: { tag?: string | undefined; requestId?: unknown },
  ): Issue {
    return {
      severity,
      conn: this.conn,
      kind,
      message,
      ...(extra?.tag !== undefined ? { tag: extra.tag } : {}),
      ...(extra?.requestId !== undefined ? { requestId: extra.requestId } : {}),
      ...(frame !== undefined ? { frame: preview(frame, 400) } : {}),
    };
  }

  private onClientMessage(message: unknown, text: string, issues: Array<Issue>) {
    const m = message as Record<string, unknown>;
    switch (m?._tag) {
      case "Request": {
        const tag = String(m.tag);
        const spec = getRpcSpec(tag);
        this.stats.requests++;
        this.requests.set(idKey(m.id), {
          tag,
          spec,
          rawId: m.id,
          chunks: 0,
          exited: false,
          interrupted: false,
        });
        if (!spec) {
          issues.push(
            this.issue("warning", "client-unknown-tag", `client called unknown RPC ${tag}`, text, {
              tag,
              requestId: m.id,
            }),
          );
          return;
        }
        const decoded = spec.payload.decode(m.payload);
        if (!decoded.ok) {
          issues.push(
            this.issue("warning", "client-payload", decoded.message, text, {
              tag,
              requestId: m.id,
            }),
          );
        }
        return;
      }
      case "Interrupt": {
        const req = this.requests.get(idKey(m.requestId));
        if (req) req.interrupted = true;
        return;
      }
      case "Ack":
      case "Ping":
      case "Eof":
        return;
      default:
        issues.push(
          this.issue(
            "warning",
            "client-unknown-message",
            `unknown client _tag ${String(m?._tag)}`,
            text,
          ),
        );
    }
  }

  private lookup(requestId: unknown, text: string, issues: Array<Issue>) {
    const req = this.requests.get(idKey(requestId));
    if (req) return req;
    // Same id with another JSON type (e.g. "3" for 3): the client keys a Map by the raw value.
    for (const candidate of this.requests.values()) {
      if (String(candidate.rawId) === String(requestId)) {
        issues.push(
          this.issue(
            "error",
            "request-id-type",
            `requestId ${JSON.stringify(requestId)} does not echo the request id's JSON type (${typeof candidate.rawId})`,
            text,
            { tag: candidate.tag, requestId },
          ),
        );
        return candidate;
      }
    }
    issues.push(
      this.issue(
        "error",
        "unknown-request-id",
        `no request with id ${JSON.stringify(requestId)}`,
        text,
        {
          requestId,
        },
      ),
    );
    return undefined;
  }

  private onServerMessage(message: unknown, text: string, issues: Array<Issue>) {
    const m = message as Record<string, unknown>;
    switch (m?._tag) {
      case "Pong":
        return;
      case "Defect": {
        const decoded = defectCodec.decode(m.defect);
        issues.push(
          this.issue(
            "error",
            "defect-frame",
            `Defect frame (fails every pending request): ${preview(decoded.ok ? decoded.value : m.defect, 200)}`,
            text,
          ),
        );
        return;
      }
      case "Chunk": {
        this.stats.chunks++;
        const req = this.lookup(m.requestId, text, issues);
        if (!req || !req.spec) return;
        const extra = { tag: req.tag, requestId: m.requestId };
        if (!req.spec.chunk) {
          issues.push(this.issue("error", "chunk-for-unary", "Chunk for a unary RPC", text, extra));
          return;
        }
        if (req.exited) {
          issues.push(this.issue("error", "chunk-after-exit", "Chunk after Exit", text, extra));
        }
        req.chunks++;
        const decoded = req.spec.chunk.decode(m.values);
        if (!decoded.ok) {
          issues.push(this.issue("error", "chunk-schema", decoded.message, text, extra));
        } else if (this.options.strict) {
          const diff = canonicalIssue(req.spec.chunk, decoded.value, m.values);
          if (diff) issues.push(this.issue("warning", "chunk-non-canonical", diff, text, extra));
        }
        return;
      }
      case "Exit": {
        this.stats.exits++;
        const req = this.requests.get(idKey(m.requestId));
        if (!req) {
          // The client forgets interrupted ids; any other unknown id is an error.
          this.lookup(m.requestId, text, issues);
          return;
        }
        const extra = { tag: req.tag, requestId: m.requestId };
        if (req.exited) issues.push(this.issue("error", "double-exit", "second Exit", text, extra));
        req.exited = true;
        if (!req.spec) return;
        const exit = m.exit as Record<string, unknown> | undefined;
        if (req.spec.isStream && exit?._tag === "Success" && exit.value !== null) {
          issues.push(
            this.issue(
              "error",
              "stream-exit-value",
              "a stream's success Exit must carry null",
              text,
              extra,
            ),
          );
        }
        const decoded = req.spec.exit.decode(exit);
        if (!decoded.ok) {
          if (!(req.interrupted && isInterruptOnly(exit))) {
            issues.push(this.issue("error", "exit-schema", decoded.message, text, extra));
          }
        } else if (this.options.strict) {
          const diff = canonicalIssue(req.spec.exit, decoded.value, exit);
          if (diff) issues.push(this.issue("warning", "exit-non-canonical", diff, text, extra));
        }
        if (exit?._tag === "Failure" && Array.isArray(exit.cause)) {
          for (const reason of exit.cause as Array<Record<string, unknown>>) {
            if (reason?._tag === "Die") {
              issues.push(
                this.issue(
                  "warning",
                  "exit-die",
                  `Die: ${preview(reason.defect, 200)}`,
                  text,
                  extra,
                ),
              );
            }
          }
        }
        return;
      }
      default:
        issues.push(
          this.issue(
            "error",
            "server-unknown-message",
            `unknown server _tag ${String(m?._tag)}`,
            text,
          ),
        );
    }
  }
}

const isInterruptOnly = (exit: Record<string, unknown> | undefined) =>
  exit?._tag === "Failure" &&
  Array.isArray(exit.cause) &&
  (exit.cause as Array<Record<string, unknown>>).every((r) => r?._tag === "Interrupt");

// ---------------------------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------------------------

export interface HttpExchange {
  readonly conn: string;
  readonly method: string;
  readonly url: string;
  readonly requestContentType?: string | undefined;
  readonly requestBody?: string | undefined;
  readonly status: number;
  readonly responseContentType?: string | undefined;
  readonly responseBody?: string | undefined;
}

/** Paths whose traffic is API traffic (everything else is the static SPA). */
export const isApiPath = (pathname: string) =>
  pathname === "/ws" ||
  pathname.startsWith("/api/") ||
  pathname.startsWith("/oauth/") ||
  pathname.startsWith("/.well-known/") ||
  pathname.startsWith("/zenith/") ||
  pathname === "/mcp";

export const validateHttpExchange = (
  exchange: HttpExchange,
  options: ValidatorOptions = {},
): { endpoint: string | undefined; issues: Array<Issue> } => {
  const issues: Array<Issue> = [];
  const url = new URL(exchange.url, "http://proxy.invalid");
  const push = (severity: Severity, kind: string, message: string, frame?: string) =>
    issues.push({
      severity,
      conn: exchange.conn,
      kind,
      message: `${exchange.method} ${url.pathname} → ${exchange.status}: ${message}`,
      ...(frame !== undefined ? { frame: preview(frame, 400) } : {}),
    });

  if (exchange.method === "OPTIONS" || exchange.method === "HEAD") {
    return { endpoint: undefined, issues };
  }

  if (url.pathname === "/zenith/embed.json" && exchange.method === "GET") {
    const body = parseJsonBody(exchange.responseBody);
    const decoded = ZenithEmbedJson.decode(body);
    if (!decoded.ok) push("error", "http-response-schema", decoded.message, exchange.responseBody);
    return { endpoint: "zenith.embed", issues };
  }

  const found = findHttpEndpoint(exchange.method, url.pathname);
  if (!found) return { endpoint: undefined, issues };
  const { spec, params } = found;

  // Request side (client-caused problems are warnings).
  if (spec.params) {
    const decoded = spec.params.decode(params);
    if (!decoded.ok) push("warning", "http-request-params", decoded.message);
  }
  if (spec.payload.size > 0) {
    if (exchange.method === "GET") {
      const query = Object.fromEntries(url.searchParams);
      const codec = spec.payload.get("application/x-www-form-urlencoded");
      const decoded = codec?.decode(query);
      if (decoded && !decoded.ok) push("warning", "http-request-query", decoded.message);
    } else {
      const contentType = (exchange.requestContentType ?? "").split(";")[0]!.trim();
      const codec = spec.payload.get(contentType);
      if (!codec) {
        push(
          "warning",
          "http-request-content-type",
          `unexpected request content-type ${contentType}`,
        );
      } else {
        const body =
          contentType === "application/x-www-form-urlencoded"
            ? Object.fromEntries(new URLSearchParams(exchange.requestBody ?? ""))
            : parseJsonBody(exchange.requestBody);
        const decoded = codec.decode(body);
        if (!decoded.ok) push("warning", "http-request-body", decoded.message);
      }
    }
  }

  // Response side.
  const codec = spec.successes.get(exchange.status) ?? spec.errors.get(exchange.status);
  if (!codec) {
    push(
      exchange.status >= 500 ? "error" : "warning",
      "http-undeclared-status",
      `status not declared by ${spec.name} (declared: ${[...spec.successes.keys(), ...spec.errors.keys()].join(", ")})`,
      exchange.responseBody,
    );
    return { endpoint: spec.name, issues };
  }
  const body = parseJsonBody(exchange.responseBody);
  if (body === undefined && exchange.status !== 204) {
    push("error", "http-response-not-json", "response body is not JSON", exchange.responseBody);
    return { endpoint: spec.name, issues };
  }
  const decoded = codec.decode(body);
  if (!decoded.ok) {
    push("error", "http-response-schema", decoded.message, exchange.responseBody);
  } else if (options.strict) {
    const diff = canonicalIssue(codec, decoded.value, body);
    if (diff) push("warning", "http-response-non-canonical", diff);
  }
  return { endpoint: spec.name, issues };
};

const parseJsonBody = (body: string | undefined): unknown => {
  if (body === undefined || body.length === 0) return undefined;
  try {
    return JSON.parse(body);
  } catch {
    return undefined;
  }
};
