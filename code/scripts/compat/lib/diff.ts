/**
 * Per-request comparison of two recordings (record vs replay, TS vs Rust, TS vs TS).
 *
 * Each recording is reduced to "request views":
 *   - WebSocket RPC: key `<conn> <tag>#<ordinal of that tag on the conn>`; the stream values are
 *     flattened across Chunk frames (batching differs legitimately), plus the Exit (or "open").
 *   - HTTP: key `HTTP <METHOD> <endpoint or path>#<ordinal>`; status, body and a few headers.
 * Values are normalized (normalize.ts) before comparison.
 */
import { findHttpEndpoint } from "./contracts.ts";
import { jsonDiff, preview, type JsonDifference } from "./json.ts";
import { Normalizer, type NormalizeOptions } from "./normalize.ts";
import type { RecordedEvent } from "./recording.ts";

export interface RequestView {
  readonly key: string;
  readonly kind: "rpc" | "http";
  readonly request: unknown;
  readonly values: Array<unknown>;
  exit: unknown;
  open: boolean;
}

const COMPARED_HEADERS = [
  "content-type",
  "cache-control",
  "access-control-allow-origin",
  "access-control-allow-credentials",
  "access-control-allow-methods",
  "access-control-allow-headers",
  "content-security-policy",
  "set-cookie",
];

/** Form-urlencoded bodies (`/oauth/token`) compare field by field, so secrets in them normalize. */
const formToObject = (body: unknown): unknown =>
  typeof body === "string" && /^[\w.%+-]+=[^\s]*(&[\w.%+-]+=[^\s]*)*$/.test(body)
    ? Object.fromEntries(new URLSearchParams(body))
    : body;

/** Path aliases a recording declares in its meta events (`{"pathAliases": {...}}`). */
export const pathAliasesOf = (events: ReadonlyArray<RecordedEvent>): Record<string, string> => {
  const aliases: Record<string, string> = {};
  for (const event of events) {
    if (event.dir !== "meta") continue;
    const meta = event.frame as { pathAliases?: Record<string, string> };
    Object.assign(aliases, meta.pathAliases ?? {});
  }
  return aliases;
};

export const requestViews = (
  events: ReadonlyArray<RecordedEvent>,
  options: NormalizeOptions = {},
): Map<string, RequestView> => {
  const normalizer = new Normalizer({
    ...options,
    pathAliases: { ...pathAliasesOf(events), ...options.pathAliases },
  });
  const views = new Map<string, RequestView>();
  const wsRequests = new Map<string, RequestView>(); // `${conn}:${typeof id}:${id}` → view
  const tagOrdinals = new Map<string, number>();
  const httpOrdinals = new Map<string, number>();
  const httpPending = new Map<string, RequestView>();

  for (const event of events) {
    if (
      event.dir === "issue" ||
      event.dir === "meta" ||
      event.dir === "open" ||
      event.dir === "close"
    ) {
      continue;
    }
    const raw = event.frame as Record<string, unknown>;
    if (event.conn.startsWith("http")) {
      if (event.dir === "c2s") {
        const url = new URL(String(raw.url), "http://x.invalid");
        const endpoint =
          findHttpEndpoint(String(raw.method), url.pathname)?.spec.name ?? url.pathname;
        const base = `HTTP ${String(raw.method)} ${endpoint}`;
        const ordinal = httpOrdinals.get(base) ?? 0;
        httpOrdinals.set(base, ordinal + 1);
        const view: RequestView = {
          key: `${base}#${ordinal}`,
          kind: "http",
          request: normalizer.normalize({ url: raw.url, body: formToObject(raw.body) }),
          values: [],
          exit: undefined,
          open: true,
        };
        views.set(view.key, view);
        httpPending.set(event.conn, view);
      } else {
        const view = httpPending.get(event.conn);
        if (!view) continue;
        const headers = (raw.headers ?? {}) as Record<string, unknown>;
        const kept: Record<string, unknown> = {};
        for (const name of COMPARED_HEADERS)
          if (headers[name] !== undefined) kept[name] = headers[name];
        view.exit = normalizer.normalize({ status: raw.status, headers: kept, body: raw.body });
        view.open = false;
      }
      continue;
    }
    // WebSocket frames (possibly batched arrays).
    for (const message of (Array.isArray(raw) ? raw : [raw]) as Array<Record<string, unknown>>) {
      const id = event.dir === "c2s" ? message.id : message.requestId;
      const reqKey = `${event.conn}:${typeof id}:${String(id)}`;
      if (event.dir === "c2s" && message._tag === "Request") {
        const base = `${event.conn} ${String(message.tag)}`;
        const ordinal = tagOrdinals.get(base) ?? 0;
        tagOrdinals.set(base, ordinal + 1);
        const view: RequestView = {
          key: `${base}#${ordinal}`,
          kind: "rpc",
          request: normalizer.normalize(message.payload),
          values: [],
          exit: undefined,
          open: true,
        };
        views.set(view.key, view);
        wsRequests.set(reqKey, view);
      } else if (event.dir === "s2c" && message._tag === "Chunk") {
        const view = wsRequests.get(reqKey);
        if (view) view.values.push(...(normalizer.normalize(message.values) as Array<unknown>));
      } else if (event.dir === "s2c" && message._tag === "Exit") {
        const view = wsRequests.get(reqKey);
        if (view) {
          view.exit = normalizer.normalize(message.exit);
          view.open = false;
        }
      }
    }
  }
  return views;
};

export interface RequestDiff {
  readonly key: string;
  readonly status: "same" | "different" | "only-left" | "only-right";
  readonly differences: Array<JsonDifference>;
  readonly note?: string;
}

export interface DiffOptions extends NormalizeOptions {
  /** Request keys to skip (regex). */
  readonly ignoreKeys?: RegExp;
  /** JSON paths to ignore inside a request (regex on `$.values[0].foo` style paths). */
  readonly ignorePaths?: RegExp;
}

export const diffRecordings = (
  left: ReadonlyArray<RecordedEvent>,
  right: ReadonlyArray<RecordedEvent>,
  options: DiffOptions = {},
): Array<RequestDiff> => {
  const a = requestViews(left, options);
  const b = requestViews(right, options);
  const keys = [...new Set([...a.keys(), ...b.keys()])];
  const out: Array<RequestDiff> = [];
  for (const key of keys) {
    if (options.ignoreKeys?.test(key)) continue;
    const l = a.get(key);
    const r = b.get(key);
    if (!l || !r) {
      out.push({ key, status: l ? "only-left" : "only-right", differences: [] });
      continue;
    }
    let note: string | undefined;
    let lv = l.values;
    let rv = r.values;
    if ((l.open || r.open) && lv.length !== rv.length) {
      // Long-lived streams are cut at an arbitrary point: compare the common prefix.
      const n = Math.min(lv.length, rv.length);
      note = `stream values: ${lv.length} vs ${rv.length} (compared the first ${n})`;
      lv = lv.slice(0, n);
      rv = rv.slice(0, n);
    }
    const differences = jsonDiff(
      { request: l.request, values: lv, exit: l.open || r.open ? undefined : l.exit },
      { request: r.request, values: rv, exit: l.open || r.open ? undefined : r.exit },
    ).filter((d) => !options.ignorePaths?.test(d.path));
    out.push({
      key,
      status: differences.length === 0 ? "same" : "different",
      differences,
      ...(note ? { note } : {}),
    });
  }
  return out;
};

export const formatDiffReport = (diffs: ReadonlyArray<RequestDiff>, maxPerRequest = 12): string => {
  const count = (status: RequestDiff["status"]) => diffs.filter((d) => d.status === status).length;
  const lines = [
    `${count("same")} same, ${count("different")} different, ${count("only-left")} only in left, ${count("only-right")} only in right`,
  ];
  for (const diff of diffs) {
    if (diff.status === "same") continue;
    const mark = { different: "≠", "only-left": "-", "only-right": "+", same: "=" }[diff.status];
    lines.push(`${mark} ${diff.key}${diff.note ? `  (${diff.note})` : ""}`);
    for (const d of diff.differences.slice(0, maxPerRequest)) {
      lines.push(`    ${d.path}: ${preview(d.left, 100)}  ≠  ${preview(d.right, 100)}`);
    }
    if (diff.differences.length > maxPerRequest) {
      lines.push(`    … ${diff.differences.length - maxPerRequest} more`);
    }
  }
  return lines.join("\n");
};
