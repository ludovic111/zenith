/**
 * The validating, recording proxy. It sits between a client (apps/web in a browser, the black-box
 * tests, the replayer) and a backend (TS or Rust), forwards everything unchanged — status,
 * headers, cookies, bodies, WebSocket upgrades and frames — and on the side:
 *
 *  - decodes every server→client `/ws` frame against the contracts (validator.ts);
 *  - decodes every API HTTP response against its EnvironmentHttpApi endpoint schema;
 *  - optionally records everything as JSONL (recording.ts).
 *
 * Non-API GETs can be served from a static directory (`staticDir`), so the proxy can front a
 * backend that does not serve the SPA yet (the Rust one) with a prebuilt apps/web bundle.
 */
import * as NodeFS from "node:fs";
import * as NodeHttp from "node:http";
import type * as NodeNet from "node:net";
import * as NodePath from "node:path";
import * as NodeZlib from "node:zlib";
import { RecordingWriter, frameFromText, type Scrub } from "./recording.ts";
import {
  isApiPath,
  validateHttpExchange,
  WsConnectionValidator,
  type Issue,
  type ValidatorOptions,
} from "./validator.ts";
import { WebSocket, WebSocketServer, rawDataToString, type RawData } from "./ws.ts";

export interface ProxyOptions {
  /** Backend origin, e.g. http://127.0.0.1:3773 */
  readonly target: string;
  readonly host?: string;
  /** 0 picks a free port. */
  readonly port?: number;
  readonly validate?: boolean;
  readonly strict?: boolean;
  /** JSONL file to record into. */
  readonly recordFile?: string;
  /** Keep recorded events in memory (`proxy.recording.events`). */
  readonly recordInMemory?: boolean;
  readonly redact?: boolean;
  /** Literal replacements applied to recorded lines (see recording.ts `machineScrubs`). */
  readonly scrub?: ReadonlyArray<Scrub>;
  /** Serve non-API GET/HEAD requests from this directory (SPA fallback to index.html). */
  readonly staticDir?: string;
  /** Called for every issue as it is found. */
  readonly onIssue?: (issue: Issue) => void;
  /** Max body bytes kept for validation/recording (bodies are always forwarded in full). */
  readonly maxBodyBytes?: number;
}

export interface RunningProxy {
  readonly url: string;
  readonly wsUrl: string;
  readonly port: number;
  readonly issues: Array<Issue>;
  readonly recording: RecordingWriter | undefined;
  readonly stats: {
    http: number;
    ws: number;
    wsClientFrames: number;
    wsServerFrames: number;
    endpoints: Map<string, number>;
    rpcTags: Map<string, number>;
  };
  /** Open WebSocket validators, by connection id (e.g. to list open streams). */
  readonly connections: Map<string, WsConnectionValidator>;
  close(): Promise<void>;
}

const HOP_BY_HOP = new Set([
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade",
]);

const WS_HANDSHAKE_HEADERS = new Set([
  "connection",
  "upgrade",
  "sec-websocket-key",
  "sec-websocket-version",
  "sec-websocket-extensions",
  "sec-websocket-protocol",
  "content-length",
]);

const MIME: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json",
  ".webmanifest": "application/manifest+json",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
  ".woff2": "font/woff2",
  ".wasm": "application/wasm",
  ".txt": "text/plain; charset=utf-8",
};

const decompress = (body: Buffer, encoding: string | undefined): Buffer => {
  try {
    switch ((encoding ?? "").toLowerCase()) {
      case "gzip":
        return NodeZlib.gunzipSync(body);
      case "deflate":
        return NodeZlib.inflateSync(body);
      case "br":
        return NodeZlib.brotliDecompressSync(body);
      default:
        return body;
    }
  } catch {
    return body;
  }
};

const bodyForRecord = (
  text: string | undefined,
  contentType: string | undefined,
  bytes: number,
) => {
  if (text === undefined || bytes === 0) return undefined;
  if ((contentType ?? "").includes("json")) {
    try {
      return JSON.parse(text);
    } catch {
      return text;
    }
  }
  if ((contentType ?? "").startsWith("text/") || (contentType ?? "").includes("form-urlencoded")) {
    return text.length > 4096 ? { omitted: bytes } : text;
  }
  return { omitted: bytes };
};

export const startProxy = (options: ProxyOptions): Promise<RunningProxy> => {
  const target = new URL(options.target);
  const validate = options.validate ?? true;
  const validatorOptions: ValidatorOptions = { strict: options.strict ?? false };
  const maxBody = options.maxBodyBytes ?? 32 * 1024 * 1024;
  const recording =
    options.recordFile || options.recordInMemory
      ? new RecordingWriter(options.recordFile, {
          redact: options.redact ?? true,
          keepInMemory: options.recordInMemory ?? false,
          ...(options.scrub ? { scrub: options.scrub } : {}),
        })
      : undefined;
  const issues: Array<Issue> = [];
  const connections = new Map<string, WsConnectionValidator>();
  const stats: RunningProxy["stats"] = {
    http: 0,
    ws: 0,
    wsClientFrames: 0,
    wsServerFrames: 0,
    endpoints: new Map(),
    rpcTags: new Map(),
  };
  const sockets = new Set<NodeNet.Socket>();
  const liveWs = new Set<WebSocket>();
  let httpSeq = 0;
  let wsSeq = 0;

  const report = (found: ReadonlyArray<Issue>) => {
    for (const issue of found) {
      issues.push(issue);
      recording?.write(issue.conn, "issue", issue);
      options.onIssue?.(issue);
    }
  };
  const bump = (map: Map<string, number>, key: string) => map.set(key, (map.get(key) ?? 0) + 1);

  const serveStatic = (
    req: NodeHttp.IncomingMessage,
    res: NodeHttp.ServerResponse,
    pathname: string,
  ) => {
    const root = NodePath.resolve(options.staticDir!);
    let file = NodePath.resolve(root, `.${decodeURIComponent(pathname)}`);
    if (!file.startsWith(root)) {
      res.writeHead(400).end();
      return;
    }
    if (!NodeFS.existsSync(file) || NodeFS.statSync(file).isDirectory()) {
      const index = NodePath.join(file, "index.html");
      file = NodeFS.existsSync(index) ? index : NodePath.join(root, "index.html");
    }
    const body = NodeFS.readFileSync(file);
    res.writeHead(200, {
      "content-type": MIME[NodePath.extname(file)] ?? "application/octet-stream",
      "content-length": body.length,
      "cache-control": file.endsWith(".html") ? "no-cache" : "public, max-age=60",
    });
    res.end(req.method === "HEAD" ? undefined : body);
  };

  const server = NodeHttp.createServer((req, res) => {
    const url = new URL(req.url ?? "/", "http://proxy.invalid");
    const api = isApiPath(url.pathname);
    if (options.staticDir && !api && (req.method === "GET" || req.method === "HEAD")) {
      serveStatic(req, res, url.pathname);
      return;
    }
    stats.http++;
    const conn = `http${++httpSeq}`;
    const headers: NodeHttp.OutgoingHttpHeaders = {};
    for (const [key, value] of Object.entries(req.headers)) {
      if (!HOP_BY_HOP.has(key) && value !== undefined) headers[key] = value;
    }
    const requestChunks: Array<Buffer> = [];
    let requestBytes = 0;
    const upstream = NodeHttp.request(
      {
        protocol: target.protocol,
        hostname: target.hostname,
        port: target.port,
        method: req.method,
        path: req.url,
        headers,
      },
      (upstreamRes) => {
        const responseHeaders: NodeHttp.OutgoingHttpHeaders = {};
        for (const [key, value] of Object.entries(upstreamRes.headers)) {
          if (!HOP_BY_HOP.has(key) && value !== undefined) responseHeaders[key] = value;
        }
        res.writeHead(upstreamRes.statusCode ?? 502, upstreamRes.statusMessage, responseHeaders);
        const responseChunks: Array<Buffer> = [];
        let responseBytes = 0;
        upstreamRes.on("data", (chunk: Buffer) => {
          if (api && responseBytes < maxBody) responseChunks.push(chunk);
          responseBytes += chunk.length;
          res.write(chunk);
        });
        upstreamRes.on("end", () => {
          res.end();
          if (!api) return;
          const requestBody =
            requestBytes > 0 ? Buffer.concat(requestChunks).toString("utf8") : undefined;
          const decoded = decompress(
            Buffer.concat(responseChunks),
            upstreamRes.headers["content-encoding"] as string | undefined,
          );
          const responseBody = decoded.length > 0 ? decoded.toString("utf8") : undefined;
          if (validate) {
            const { endpoint, issues: found } = validateHttpExchange(
              {
                conn,
                method: req.method ?? "GET",
                url: req.url ?? "/",
                requestContentType: req.headers["content-type"],
                requestBody,
                status: upstreamRes.statusCode ?? 0,
                responseContentType: upstreamRes.headers["content-type"],
                responseBody,
              },
              validatorOptions,
            );
            if (endpoint) bump(stats.endpoints, endpoint);
            report(found);
          }
          if (recording) {
            recording.write(conn, "c2s", {
              method: req.method,
              url: req.url,
              headers: req.headers,
              ...(requestBody !== undefined
                ? { body: bodyForRecord(requestBody, req.headers["content-type"], requestBytes) }
                : {}),
            });
            recording.write(conn, "s2c", {
              status: upstreamRes.statusCode,
              headers: upstreamRes.headers,
              ...(responseBody !== undefined
                ? {
                    body: bodyForRecord(
                      responseBody,
                      upstreamRes.headers["content-type"],
                      decoded.length,
                    ),
                  }
                : {}),
            });
          }
        });
        upstreamRes.on("error", () => res.destroy());
      },
    );
    upstream.on("error", (error) => {
      if (!res.headersSent) {
        res.writeHead(502, { "content-type": "text/plain" });
      }
      res.end(`compat proxy: backend unreachable: ${error.message}`);
    });
    req.on("data", (chunk: Buffer) => {
      if (requestBytes < maxBody) requestChunks.push(chunk);
      requestBytes += chunk.length;
      upstream.write(chunk);
    });
    req.on("end", () => upstream.end());
    req.on("error", () => upstream.destroy());
  });

  const wss = new WebSocketServer({ noServer: true, perMessageDeflate: true });

  server.on("upgrade", (req: NodeHttp.IncomingMessage, socket: NodeNet.Socket, head: Buffer) => {
    const conn = `ws${++wsSeq}`;
    stats.ws++;
    const url = new URL(req.url ?? "/", "http://proxy.invalid");
    const isRpcSocket = url.pathname === "/ws";
    const headers: Record<string, string> = {};
    for (const [key, value] of Object.entries(req.headers)) {
      if (!WS_HANDSHAKE_HEADERS.has(key) && typeof value === "string") headers[key] = value;
    }
    const targetWs = new URL(req.url ?? "/", options.target);
    targetWs.protocol = target.protocol === "https:" ? "wss:" : "ws:";
    // The original Host header is kept (like for HTTP), so the backend sees what the client sees.
    const backend = new WebSocket(targetWs.toString(), { headers, perMessageDeflate: true });
    liveWs.add(backend);
    recording?.write(conn, "open", {
      path: url.pathname,
      query: Object.fromEntries(url.searchParams),
      headers: req.headers,
    });
    let upgradeSetCookie: Array<string> | undefined;
    backend.on("upgrade", (response: NodeHttp.IncomingMessage) => {
      const setCookie = response.headers["set-cookie"];
      if (setCookie) upgradeSetCookie = setCookie;
    });
    backend.on("unexpected-response", (_request: unknown, response: NodeHttp.IncomingMessage) => {
      const chunks: Array<Buffer> = [];
      response.on("data", (chunk: Buffer) => chunks.push(chunk));
      response.on("end", () => {
        const body = Buffer.concat(chunks);
        const lines = [`HTTP/1.1 ${response.statusCode} ${response.statusMessage ?? ""}`];
        for (const [key, value] of Object.entries(response.headers)) {
          if (key === "transfer-encoding" || key === "connection" || key === "content-length")
            continue;
          for (const v of Array.isArray(value) ? value : [value]) lines.push(`${key}: ${v}`);
        }
        lines.push(`content-length: ${body.length}`, "connection: close", "", "");
        socket.end(Buffer.concat([Buffer.from(lines.join("\r\n")), body]));
        recording?.write(conn, "close", {
          by: "backend",
          rejectedStatus: response.statusCode,
          body: body.toString("utf8").slice(0, 2000),
        });
      });
    });
    backend.on("error", (error: Error) => {
      if (!socket.destroyed && !socket.writableEnded) {
        socket.end(`HTTP/1.1 502 Bad Gateway\r\nconnection: close\r\ncontent-length: 0\r\n\r\n`);
      }
      recording?.write(conn, "close", { by: "backend", error: error.message });
    });
    backend.on("open", () => {
      const onHeaders = (responseHeaders: Array<string>, request: NodeHttp.IncomingMessage) => {
        if (request === req && upgradeSetCookie) {
          for (const cookie of upgradeSetCookie) responseHeaders.push(`Set-Cookie: ${cookie}`);
        }
      };
      wss.on("headers", onHeaders);
      wss.handleUpgrade(req, socket, head, (client: WebSocket) => {
        wss.off("headers", onHeaders);
        liveWs.add(client);
        const validator =
          validate && isRpcSocket ? new WsConnectionValidator(conn, validatorOptions) : undefined;
        if (validator) connections.set(conn, validator);
        client.on("message", (data: RawData, isBinary: boolean) => {
          if (!isBinary) {
            const text = rawDataToString(data);
            stats.wsClientFrames++;
            if (validator) {
              report(validator.onClientFrame(text));
              const frame = frameFromText(text) as { _tag?: string; tag?: string };
              if (frame?._tag === "Request" && frame.tag) bump(stats.rpcTags, frame.tag);
            }
            recording?.write(conn, "c2s", frameFromText(text));
          }
          if (backend.readyState === WebSocket.OPEN) backend.send(data, { binary: isBinary });
        });
        backend.on("message", (data: RawData, isBinary: boolean) => {
          if (!isBinary) {
            const text = rawDataToString(data);
            stats.wsServerFrames++;
            if (validator) report(validator.onServerFrame(text));
            recording?.write(conn, "s2c", frameFromText(text));
          }
          if (client.readyState === WebSocket.OPEN) client.send(data, { binary: isBinary });
        });
        const sendableCode = (code: number) =>
          code === 1005 || code === 1006 || code === 1015 ? 1000 : code;
        client.on("close", (code: number, reason: Buffer) => {
          recording?.write(conn, "close", { by: "client", code, reason: reason.toString() });
          if (validator) {
            const open = validator.openRequests();
            if (open.length > 0) recording?.write(conn, "meta", { openAtClose: open });
          }
          connections.delete(conn);
          if (backend.readyState === WebSocket.OPEN) backend.close(sendableCode(code), reason);
          liveWs.delete(client);
        });
        backend.on("close", (code: number, reason: Buffer) => {
          recording?.write(conn, "close", { by: "backend", code, reason: reason.toString() });
          if (client.readyState === WebSocket.OPEN) client.close(sendableCode(code), reason);
          liveWs.delete(backend);
        });
        client.on("error", () => backend.terminate());
      });
    });
  });

  server.on("connection", (socket: NodeNet.Socket) => {
    sockets.add(socket);
    socket.on("close", () => sockets.delete(socket));
  });

  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(options.port ?? 0, options.host ?? "127.0.0.1", () => {
      const address = server.address() as NodeNet.AddressInfo;
      const host = options.host ?? "127.0.0.1";
      recording?.write("meta", "meta", {
        kind: "proxy",
        target: options.target,
        startedAt: new Date().toISOString(),
      });
      resolve({
        url: `http://${host}:${address.port}`,
        wsUrl: `ws://${host}:${address.port}/ws`,
        port: address.port,
        issues,
        recording,
        stats,
        connections,
        close: () =>
          new Promise<void>((done) => {
            for (const ws of liveWs) ws.terminate();
            for (const socket of sockets) socket.destroy();
            wss.close();
            server.close(() => {
              recording?.close();
              done();
            });
          }),
      });
    });
  });
};

export const formatIssue = (issue: Issue): string => {
  const where = [
    issue.conn,
    issue.tag,
    issue.requestId !== undefined ? `#${String(issue.requestId)}` : undefined,
  ]
    .filter(Boolean)
    .join(" ");
  return `[${issue.severity}] ${issue.kind} (${where}): ${issue.message.replace(/\n/g, "\n    ")}`;
};

export const summarizeIssues = (issues: ReadonlyArray<Issue>) => {
  const byKind = new Map<string, number>();
  for (const issue of issues) {
    const key = `${issue.severity}:${issue.kind}${issue.tag ? `:${issue.tag}` : ""}`;
    byKind.set(key, (byKind.get(key) ?? 0) + 1);
  }
  return {
    errors: issues.filter((i) => i.severity === "error").length,
    warnings: issues.filter((i) => i.severity === "warning").length,
    byKind: Object.fromEntries([...byKind].sort()),
  };
};
