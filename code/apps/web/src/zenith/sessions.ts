/**
 * zenith: `GET /api/zenith/sessions`, served by zenith code's Rust server
 * (`crates/zenith-code/crates/zc-sessions`). Every Claude Code and Codex session of the
 * machine, read from their own logs, with totals. The TypeScript server has no such
 * route: the page then says so calmly instead of failing.
 */
import { readDesktopPrimaryBearerToken } from "../environments/primary/desktopAuth";
import { resolvePrimaryEnvironmentHttpUrl } from "../environments/primary/target";

export const ZENITH_SESSIONS_PATH = "/api/zenith/sessions";

export type ZenithSessionAgent = "claude" | "codex";

export interface ZenithSession {
  readonly agent: ZenithSessionAgent;
  readonly id: string;
  readonly title: string;
  readonly project: { readonly id: string; readonly title: string } | null;
  readonly cwd: string;
  readonly branch: string | null;
  /** First and last writes, ms since 1970. */
  readonly start: number;
  readonly end: number;
  readonly turns: number;
  readonly model: string | null;
  readonly costUSD: number | null;
  readonly tokens: number | null;
  readonly linesAdded: number | null;
  readonly linesRemoved: number | null;
  readonly prs: ReadonlyArray<{ readonly number: number; readonly url: string }>;
  readonly subagents: number;
  readonly entrypoint: string | null;
  /** Shell command that reopens the session. */
  readonly resume: string;
  /** Time really spent writing (15-minute slots with activity). */
  readonly activeMs: number;
  readonly live: boolean;
}

export interface ZenithSessionTotals {
  readonly sessions: number;
  readonly live: number;
  readonly today: number;
  readonly costUSD: number;
  readonly week: {
    readonly sessions: number;
    readonly costUSD: number;
    readonly tokens: number;
    readonly linesAdded: number;
    readonly linesRemoved: number;
    readonly prs: number;
  };
  readonly perDay: ReadonlyArray<{
    readonly day: string;
    readonly claude: number;
    readonly codex: number;
  }>;
  readonly perProject: ReadonlyArray<{
    readonly project: { readonly id: string; readonly title: string } | null;
    readonly sessions: number;
    readonly costUSD: number;
  }>;
}

export interface ZenithSessionsOverview {
  readonly generatedAt: number;
  readonly timeZone: string;
  readonly sessions: ReadonlyArray<ZenithSession>;
  readonly totals: ZenithSessionTotals;
}

export type ZenithSessionsLoad =
  | { readonly status: "ok"; readonly data: ZenithSessionsOverview }
  /** The server has no sessions route (the TypeScript server). */
  | { readonly status: "unavailable" }
  | { readonly status: "error"; readonly message: string };

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null;

function isOverview(value: unknown): value is ZenithSessionsOverview {
  return (
    isRecord(value) &&
    Array.isArray(value.sessions) &&
    isRecord(value.totals) &&
    Array.isArray(value.totals.perDay) &&
    Array.isArray(value.totals.perProject) &&
    isRecord(value.totals.week)
  );
}

/**
 * What a response means. A route the server does not know is either a 404 or the web
 * app's own HTML (servers fall back to the SPA for unknown paths): both are "unavailable".
 */
export function classifyZenithSessionsResponse(input: {
  readonly status: number;
  readonly contentType: string | null;
  readonly body: unknown;
}): ZenithSessionsLoad {
  const json = input.contentType?.toLowerCase().includes("application/json") ?? false;
  if (input.status === 404 || input.status === 405 || input.status === 501) {
    return { status: "unavailable" };
  }
  if (input.status >= 200 && input.status < 300) {
    if (!json) return { status: "unavailable" };
    return isOverview(input.body)
      ? { status: "ok", data: input.body }
      : { status: "error", message: "The server answered sessions in an unknown format." };
  }
  if (input.status === 401) {
    return { status: "error", message: "This client is not signed in to the server." };
  }
  if (input.status === 403) {
    return { status: "error", message: "This client may not read sessions on this server." };
  }
  return { status: "error", message: `The server could not list sessions (${input.status}).` };
}

function isSameOrigin(url: string): boolean {
  return (
    typeof window !== "undefined" &&
    window.location.origin.startsWith("http") &&
    new URL(url).origin === window.location.origin
  );
}

/** Reads the sessions from the primary environment, authenticated like its other HTTP calls. */
export async function loadZenithSessions(options: {
  readonly limit?: number;
  readonly signal?: AbortSignal;
}): Promise<ZenithSessionsLoad> {
  let url: string;
  try {
    url = resolvePrimaryEnvironmentHttpUrl(ZENITH_SESSIONS_PATH, {
      limit: String(options.limit ?? 200),
      tz: Intl.DateTimeFormat().resolvedOptions().timeZone,
    });
  } catch {
    return { status: "unavailable" };
  }
  const sameOrigin = isSameOrigin(url);
  const bearer = sameOrigin ? null : await readDesktopPrimaryBearerToken();
  let response: Response;
  try {
    response = await fetch(url, {
      cache: "no-store",
      credentials: sameOrigin ? "include" : "omit",
      headers: bearer ? { authorization: `Bearer ${bearer}` } : {},
      ...(options.signal ? { signal: options.signal } : {}),
    });
  } catch (error) {
    if (options.signal?.aborted) throw error;
    return { status: "error", message: "Could not reach the server." };
  }
  const contentType = response.headers.get("content-type");
  const body: unknown = contentType?.includes("json")
    ? await response.json().catch(() => null)
    : null;
  return classifyZenithSessionsResponse({ status: response.status, contentType, body });
}
