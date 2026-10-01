/**
 * Normalization for record/replay diffs. Two runs of the same session never produce byte-equal
 * traffic: ids, timestamps, tokens, ports and paths differ. A Normalizer rewrites one side's
 * values into stable placeholders; one Normalizer per side, fed in event order, so equal
 * *structure* (the 3rd new uuid is the same entity on both sides) survives normalization.
 *
 *   uuids               → "<uuid:N>"     (N = first-seen order on that side; also inside strings)
 *   ISO timestamps      → "<ts>"
 *   epoch-ms numbers    → "<epoch>"      (keys ending in At/Ms/Time, iat/exp, > 1e12)
 *   secrets             → "<secret>"     (credential, access_token, ticket, … ; cookie values)
 *   trace/span ids      → "<trace>"
 *   127.0.0.1:<port>    → "127.0.0.1:<port>", t3_session_<port>_<hash> → "t3_session_<port>_<hash>"
 *   path aliases        → "<home>", "<workspace>", …  (literal prefixes, from recording meta)
 *   sequences           → "<seq:N>"      (mode "ordinal", default) or rebased (mode "rebase") or kept ("exact")
 *   volatile keys       → "<volatile>"   (pid, durations, sizes of logs, … ; see VOLATILE_KEYS)
 */
import { SECRET_KEYS } from "./recording.ts";

const UUID = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi;
const ISO = /\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})/g;
const HTTP_DATE = /\b(?:Mon|Tue|Wed|Thu|Fri|Sat|Sun), \d{2} \w{3} \d{4} \d{2}:\d{2}:\d{2} GMT\b/g;
const HOST_PORT = /\b(127\.0\.0\.1|localhost|\[::1\]):\d{2,5}\b/g;
const COOKIE_NAME = /t3_session_\d+_[0-9a-f]+/g;
const PAIRING_CREDENTIAL = /^[23456789ABCDEFGHJKLMNPQRSTUVWXYZ]{12}$/;

const SEQUENCE_KEYS = new Set([
  "sequence",
  "snapshotSequence",
  "afterSequence",
  "fromSequenceExclusive",
  "toSequence",
  "latestSequence",
]);

/** Keys whose values legitimately differ between runs of the same session. */
export const VOLATILE_KEYS = new Set([
  "traceId",
  "fiberId",
  "spanId",
  "pid",
  "ppid",
  "uptimeMs",
  "durationMs",
  "elapsedMs",
  "expires_in",
  "serverEpoch",
  "cpuPercent",
  "memoryBytes",
  "rssBytes",
  "sizeBytes",
  "totalBytes",
  "freeBytes",
  "usedBytes",
  "loadAverage",
  "samples",
]);

export type SequenceMode = "ordinal" | "rebase" | "exact";

export interface NormalizeOptions {
  /** Literal string prefixes replaced by a placeholder, e.g. {"/tmp/x/home": "<home>"}. */
  readonly pathAliases?: Readonly<Record<string, string>>;
  readonly sequences?: SequenceMode;
  /** Extra keys to blank (in addition to VOLATILE_KEYS). */
  readonly volatileKeys?: ReadonlySet<string>;
  /** Keep VOLATILE_KEYS as they are (only the structural normalizations apply). */
  readonly keepVolatile?: boolean;
}

export class Normalizer {
  private readonly uuids = new Map<string, string>();
  private readonly sequences = new Map<number, string>();
  private sequenceBase: number | undefined;
  private readonly options: NormalizeOptions;
  private readonly aliases: Array<[string, string]>;

  constructor(options: NormalizeOptions = {}) {
    this.options = options;
    this.aliases = Object.entries(options.pathAliases ?? {})
      .filter(([from]) => from.length > 0)
      .sort((a, b) => b[0].length - a[0].length);
  }

  normalize(value: unknown, key?: string): unknown {
    if (key !== undefined) {
      if (SECRET_KEYS.has(key) && typeof value === "string") return "<secret>";
      if (
        !this.options.keepVolatile &&
        (VOLATILE_KEYS.has(key) || this.options.volatileKeys?.has(key))
      ) {
        return value === undefined ? value : "<volatile>";
      }
      if (SEQUENCE_KEYS.has(key) && typeof value === "number") return this.sequence(value);
      if (typeof value === "number" && value > 1e12 && /(At|Ms|Time|^iat|^exp)$/.test(key)) {
        return "<epoch>";
      }
      if (key === "cookie" || key === "set-cookie") {
        return typeof value === "string"
          ? this.string(value.replace(/=([^;\s]+)/, "=<secret>"))
          : Array.isArray(value)
            ? value.map((v) => this.normalize(v, key))
            : value;
      }
      if (key === "authorization" && typeof value === "string") {
        return value.replace(/^(\S+)\s+.+$/, "$1 <secret>");
      }
    }
    if (typeof value === "string") return this.string(value);
    if (Array.isArray(value)) return value.map((item) => this.normalize(item));
    if (value !== null && typeof value === "object") {
      const out: Record<string, unknown> = {};
      for (const [k, v] of Object.entries(value)) out[k] = this.normalize(v, k);
      return out;
    }
    return value;
  }

  private sequence(value: number): unknown {
    switch (this.options.sequences ?? "ordinal") {
      case "exact":
        return value;
      case "rebase":
        this.sequenceBase ??= value;
        return `<seq+${value - this.sequenceBase}>`;
      default: {
        let label = this.sequences.get(value);
        if (!label) {
          label = `<seq:${this.sequences.size}>`;
          this.sequences.set(value, label);
        }
        return label;
      }
    }
  }

  private string(value: string): string {
    let out = value;
    for (const [from, to] of this.aliases) {
      if (out.includes(from)) out = out.split(from).join(to);
    }
    if (PAIRING_CREDENTIAL.test(out)) return "<secret>";
    out = out.replace(UUID, (match) => {
      const lower = match.toLowerCase();
      let label = this.uuids.get(lower);
      if (!label) {
        label = `<uuid:${this.uuids.size}>`;
        this.uuids.set(lower, label);
      }
      return label;
    });
    out = out.replace(ISO, "<ts>");
    out = out.replace(HTTP_DATE, "<http-date>");
    // Signed capability URLs (assets, uploads) embed paths and an HMAC.
    out = out.replace(/(\/api\/(?:assets|attachments\/upload))\/[A-Za-z0-9_.~%-]+/g, "$1/<signed>");
    out = out.replace(COOKIE_NAME, "t3_session_<port>_<hash>");
    out = out.replace(HOST_PORT, "$1:<port>");
    out = out.replace(/([#?&](?:token|wsTicket)=)[^&#\s"]+/g, "$1<secret>");
    return out;
  }
}
