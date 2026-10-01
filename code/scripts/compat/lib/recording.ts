/**
 * Recording format: JSON Lines, one event per line.
 *
 *   {"conn":"ws1","dir":"open","t":0,"frame":{"path":"/ws","query":{…},"headers":{…}}}
 *   {"conn":"ws1","dir":"c2s","t":12,"frame":{"_tag":"Request","id":0,"tag":"…",…}}
 *   {"conn":"ws1","dir":"s2c","t":30,"frame":{"_tag":"Chunk","requestId":0,"values":[…]}}
 *   {"conn":"ws1","dir":"close","t":9000,"frame":{"code":1000,"reason":"","by":"client"}}
 *   {"conn":"http3","dir":"c2s","t":5,"frame":{"method":"GET","url":"/api/auth/session","headers":{…},"body":…}}
 *   {"conn":"http3","dir":"s2c","t":7,"frame":{"status":200,"headers":{…},"body":{…}}}
 *   {"conn":"ws1","dir":"issue","t":31,"frame":{"severity":"error","kind":"chunk-schema",…}}
 *   {"conn":"meta","dir":"meta","t":0,"frame":{"backend":"ts","startedAt":"…",…}}
 *
 * `t` is milliseconds since the recording started. WebSocket frames are stored parsed (key order
 * is preserved), or as {"raw": "<text>"} when a frame is not JSON. HTTP bodies are parsed JSON when
 * the content type is JSON, a string otherwise, or {"omitted": <bytes>} when large or binary.
 */
import * as NodeFS from "node:fs";
import * as NodeOS from "node:os";

export type Direction = "c2s" | "s2c" | "open" | "close" | "issue" | "meta";

export interface RecordedEvent {
  readonly conn: string;
  readonly dir: Direction;
  readonly t: number;
  readonly frame: unknown;
}

export const readRecording = (file: string): Array<RecordedEvent> =>
  NodeFS.readFileSync(file, "utf8")
    .split("\n")
    .filter((line) => line.trim().length > 0)
    .map((line) => JSON.parse(line) as RecordedEvent);

export class RecordingWriter {
  private fd: number | undefined;
  private readonly start = Date.now();
  readonly events: Array<RecordedEvent> = [];
  readonly redact: boolean;

  private readonly scrub: ReadonlyArray<Scrub>;

  constructor(
    file: string | undefined,
    options: {
      redact?: boolean;
      keepInMemory?: boolean;
      /** Replacements applied to every written line (see `machineScrubs`). */
      scrub?: ReadonlyArray<Scrub>;
    } = {},
  ) {
    this.fd = file ? NodeFS.openSync(file, "w") : undefined;
    this.redact = options.redact ?? true;
    this.keepInMemory = options.keepInMemory ?? !file;
    this.scrub = options.scrub ?? [];
  }

  private readonly keepInMemory: boolean;

  write(conn: string, dir: Direction, frame: unknown) {
    let line = JSON.stringify({
      conn,
      dir,
      t: Date.now() - this.start,
      frame: this.redact ? redactValue(frame) : frame,
    });
    for (const [from, to] of this.scrub) {
      line = typeof from === "string" ? line.split(from).join(to) : line.replace(from, to);
    }
    if (this.keepInMemory) this.events.push(JSON.parse(line) as RecordedEvent);
    if (this.fd !== undefined) NodeFS.writeSync(this.fd, `${line}\n`);
  }

  /** Later writes (late socket close events, …) are dropped from the file. */
  close() {
    if (this.fd !== undefined) NodeFS.closeSync(this.fd);
    this.fd = undefined;
  }
}

export const frameFromText = (text: string): unknown => {
  try {
    return JSON.parse(text);
  } catch {
    return { raw: text };
  }
};

export const frameToText = (frame: unknown): string =>
  frame !== null && typeof frame === "object" && "raw" in frame && Object.keys(frame).length === 1
    ? String((frame as { raw: unknown }).raw)
    : JSON.stringify(frame);

// ---------------------------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------------------------

/** JSON keys whose string values are bearer secrets. */
export const SECRET_KEYS = new Set([
  "credential",
  "access_token",
  "accessToken",
  "sessionToken",
  "token",
  "ticket",
  "wsTicket",
  "subject_token",
  "proof",
  "secret",
  "privateKey",
  "apiKey",
  "password",
]);

const SECRET_HEADERS = new Set(["cookie", "set-cookie", "authorization", "dpop"]);

const redactUrlString = (value: string): string =>
  value
    .replace(/([#?&](?:token|wsTicket|ticket|credential)=)[^&#\s"]+/g, "$1<redacted>")
    .replace(/(t3_session[^=;\s]*=)[^;\s"]+/g, "$1<redacted>");

const redactHeader = (name: string, value: string): string => {
  switch (name) {
    case "cookie":
      return value.replace(/([^=;\s]+)=[^;]*/g, "$1=<redacted>");
    case "set-cookie":
      return value.replace(/^([^=;\s]+)=[^;]*/, "$1=<redacted>");
    case "authorization":
      return value.replace(/^(\S+)\s+.+$/, "$1 <redacted>");
    default:
      return "<redacted>";
  }
};

export const redactValue = (value: unknown, key?: string): unknown => {
  if (typeof value === "string") {
    if (key !== undefined && SECRET_KEYS.has(key)) return value.length > 0 ? "<redacted>" : value;
    if (key !== undefined && SECRET_HEADERS.has(key.toLowerCase())) {
      return redactHeader(key.toLowerCase(), value);
    }
    return redactUrlString(value);
  }
  if (Array.isArray(value)) return value.map((item) => redactValue(item, key));
  if (value !== null && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value)) out[k] = redactValue(v, k);
    return out;
  }
  return value;
};

/**
 * Replacements that keep the owner's machine out of committed recordings: home directory,
 * user name, host name, the descriptor's machine label ("<Name>’s Mac mini") with the name in
 * it, and e-mail addresses. Longest first.
 */
export type Scrub = readonly [string | RegExp, string];

export const machineScrubs = (labels: ReadonlyArray<string> = []): Array<Scrub> => {
  const pairs: Array<[string, string]> = [];
  const add = (from: string | undefined, to: string) => {
    if (from && from.length >= 3 && !pairs.some(([f]) => f === from)) pairs.push([from, to]);
  };
  add(NodeOS.homedir(), "/Users/compat");
  add(NodeOS.userInfo().username, "compat");
  add(NodeOS.hostname(), "compat-host");
  add(NodeOS.hostname().replace(/\.local$/, ""), "compat-host");
  for (const label of labels) {
    add(label, "Compat Machine");
    const owner = /^(.+?)[’']s\s/.exec(label)?.[1];
    add(owner, "Compat");
  }
  return [
    ...pairs.sort((a, b) => b[0].length - a[0].length),
    // Provider account e-mails (provider auth status) and any other address.
    [
      /[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}/g,
      "compat@example.invalid",
    ],
  ];
};
