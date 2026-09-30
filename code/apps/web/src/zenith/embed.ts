/**
 * zenith embedding: when this app runs inside the zenith dashboard (an iframe),
 * it asks the parent page for one-time pairing tokens, takes project focus and
 * navigation requests from it, and publishes its projects and threads so
 * zenith's own sidebar can list them (the app's sidebar is then hidden). Only
 * parents listed by the server (/zenith/embed.json, from
 * ZENITH_CODE_PARENT_ORIGINS) are ever talked to or listened to.
 */

export const ZENITH_MESSAGE = {
  pairRequest: "zenith-code:pair-request",
  pairToken: "zenith-code:pair-token",
  pairError: "zenith-code:pair-error",
  openProject: "zenith-code:open-project",
  ready: "zenith-code:ready",
  /** parent → app: `{ ownSidebar: boolean }`, whether the app shows its own sidebar. */
  chrome: "zenith-code:chrome",
  /** parent → app: a `ZenithNavigateRequest`. */
  navigate: "zenith-code:navigate",
  /** app → parent: a `ZenithSidebarSnapshot`, whenever it changes. */
  sidebar: "zenith-code:sidebar",
  /** app → parent: `{ zoom: boolean }`, a press in the title bar (zenith.app moves the window). */
  drag: "zenith-code:drag",
} as const;

export type ZenithThreadSection = "pinned" | "active" | "snoozed" | "settled";

export type ZenithThreadStatus =
  | "approval"
  | "input"
  | "working"
  | "connecting"
  | "plan"
  | "monitoring"
  | "completed"
  | "failed";

export interface ZenithSidebarProject {
  readonly environmentId: string;
  readonly id: string;
  readonly title: string;
  readonly workspaceRoot: string;
}

export interface ZenithSidebarThread {
  readonly environmentId: string;
  readonly id: string;
  readonly projectId: string;
  readonly title: string;
  readonly branch: string | null;
  readonly section: ZenithThreadSection;
  readonly status: ZenithThreadStatus | null;
  readonly activityAt: string;
}

export interface ZenithSidebarSnapshot {
  readonly projects: ReadonlyArray<ZenithSidebarProject>;
  /** Sidebar order: pinned, active, snoozed, settled. */
  readonly threads: ReadonlyArray<ZenithSidebarThread>;
  /** `environmentId:threadId` of the open thread, if any. */
  readonly activeThread: string | null;
  readonly pathname: string;
}

/**
 * An in-app path the parent may open (zenith mirrors the app's path in its own URL, so
 * back/forward and reloads land on the same thread). Plain same-origin paths only, never
 * the pairing page.
 */
export function isZenithAppPath(value: unknown): value is string {
  return (
    typeof value === "string" &&
    /^\/(?!\/)[A-Za-z0-9\-._~%/:@]*$/.test(value) &&
    !/^\/pair(\/|$)/.test(value)
  );
}

export type ZenithNavigateRequest =
  | { readonly to: "thread"; readonly environmentId: string; readonly threadId: string }
  | { readonly to: "new-thread"; readonly environmentId: string; readonly projectId: string }
  | { readonly to: "path"; readonly path: string }
  | { readonly to: "palette"; readonly open?: "add-project" | "new-thread-in" };

const EMBED_CONFIG_PATH = "/zenith/embed.json";
const PENDING_PROJECT_KEY = "zenith:open-project";
const PROJECT_SEARCH_PARAM = "zenithProject";
const CHROME_KEY = "zenith:chrome";
const CHROME_SEARCH_PARAM = "zenithChrome";

export function isEmbedded(): boolean {
  try {
    return window.self !== window.top;
  } catch {
    // Cross-origin top frame: we are definitely framed.
    return true;
  }
}

let parentOriginsPromise: Promise<ReadonlyArray<string>> | null = null;

/** Allowed parent origins, fetched once from the server. */
export function zenithParentOrigins(): Promise<ReadonlyArray<string>> {
  parentOriginsPromise ??= fetch(EMBED_CONFIG_PATH, { cache: "no-store", credentials: "omit" })
    .then((response) => (response.ok ? response.json() : null))
    .then((body: unknown) => {
      const origins = (body as { parentOrigins?: unknown } | null)?.parentOrigins;
      return Array.isArray(origins)
        ? origins.filter((origin): origin is string => typeof origin === "string")
        : [];
    })
    .catch(() => []);
  return parentOriginsPromise;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

/** True when the message comes from our direct parent and an allowed origin. */
export function isTrustedParentMessage(
  event: MessageEvent,
  origins: ReadonlyArray<string>,
): event is MessageEvent<Record<string, unknown>> {
  return event.source === window.parent && origins.includes(event.origin) && isRecord(event.data);
}

/** Post to the parent, once per allowed origin (the browser drops non-matching targets). */
export function postToZenith(message: Record<string, unknown>, origins: ReadonlyArray<string>) {
  for (const origin of origins) window.parent.postMessage(message, origin);
}

export type EmbeddedPairingResult =
  | { readonly kind: "token"; readonly token: string }
  | { readonly kind: "error"; readonly message: string }
  | { readonly kind: "unavailable" };

/** Ask the zenith parent page for a fresh one-time pairing token. */
export async function requestEmbeddedPairingToken(
  timeoutMs = 15_000,
): Promise<EmbeddedPairingResult> {
  if (!isEmbedded()) return { kind: "unavailable" };
  const origins = await zenithParentOrigins();
  if (origins.length === 0) return { kind: "unavailable" };

  return new Promise((resolve) => {
    const finish = (result: EmbeddedPairingResult) => {
      window.removeEventListener("message", onMessage);
      window.clearTimeout(timer);
      resolve(result);
    };
    const onMessage = (event: MessageEvent) => {
      if (!isTrustedParentMessage(event, origins)) return;
      const { type, token, error } = event.data;
      if (type === ZENITH_MESSAGE.pairToken && typeof token === "string" && token.length > 0) {
        finish({ kind: "token", token });
      } else if (type === ZENITH_MESSAGE.pairError) {
        finish({ kind: "error", message: typeof error === "string" ? error : "Pairing failed." });
      }
    };
    const timer = window.setTimeout(() => finish({ kind: "unavailable" }), timeoutMs);
    window.addEventListener("message", onMessage);
    postToZenith({ type: ZENITH_MESSAGE.pairRequest }, origins);
  });
}

/**
 * `?zenithProject=/abs/path` asks to focus a project. The request survives the
 * pairing redirect in sessionStorage; the param is stripped from the URL.
 */
export function captureProjectFocusRequest(): void {
  try {
    const url = new URL(window.location.href);
    const path = url.searchParams.get(PROJECT_SEARCH_PARAM);
    const chrome = url.searchParams.get(CHROME_SEARCH_PARAM);
    if (!path && !chrome) return;
    if (path) window.sessionStorage.setItem(PENDING_PROJECT_KEY, path);
    if (chrome) window.sessionStorage.setItem(CHROME_KEY, chrome);
    url.searchParams.delete(PROJECT_SEARCH_PARAM);
    url.searchParams.delete(CHROME_SEARCH_PARAM);
    window.history.replaceState(window.history.state, "", url.toString());
  } catch {
    // Storage can be unavailable; the focus request is best-effort.
  }
}

/**
 * Whether the app draws its own thread sidebar. Inside zenith, the dashboard's
 * sidebar lists the threads instead (`?zenithChrome=bare` on first load, then
 * `zenith-code:chrome` messages as zenith's layout changes).
 */
// Read lazily: main.tsx captures `?zenithChrome=` after this module loads.
let ownSidebar: boolean | null = null;
const ownSidebarListeners = new Set<() => void>();

function readInitialOwnSidebar(): boolean {
  if (typeof window === "undefined" || !isEmbedded()) return true;
  try {
    return window.sessionStorage.getItem(CHROME_KEY) !== "bare";
  } catch {
    return true;
  }
}

export function readOwnSidebar(): boolean {
  ownSidebar ??= readInitialOwnSidebar();
  return ownSidebar;
}

export function setOwnSidebar(value: boolean): void {
  if (value === readOwnSidebar()) return;
  ownSidebar = value;
  try {
    window.sessionStorage.setItem(CHROME_KEY, value ? "full" : "bare");
  } catch {
    // Best-effort.
  }
  for (const listener of ownSidebarListeners) listener();
}

export function subscribeOwnSidebar(listener: () => void): () => void {
  ownSidebarListeners.add(listener);
  return () => ownSidebarListeners.delete(listener);
}

export function parseNavigateRequest(value: unknown): ZenithNavigateRequest | null {
  if (!isRecord(value)) return null;
  const str = (key: string) => (typeof value[key] === "string" ? (value[key] as string) : null);
  switch (value.to) {
    case "thread": {
      const environmentId = str("environmentId");
      const threadId = str("threadId");
      return environmentId && threadId ? { to: "thread", environmentId, threadId } : null;
    }
    case "new-thread": {
      const environmentId = str("environmentId");
      const projectId = str("projectId");
      return environmentId && projectId ? { to: "new-thread", environmentId, projectId } : null;
    }
    case "path":
      return isZenithAppPath(value.path) ? { to: "path", path: value.path } : null;
    case "palette": {
      const open =
        value.open === "add-project" || value.open === "new-thread-in" ? value.open : undefined;
      return open ? { to: "palette", open } : { to: "palette" };
    }
    default:
      return null;
  }
}

export function setPendingProjectFocus(path: string): void {
  try {
    window.sessionStorage.setItem(PENDING_PROJECT_KEY, path);
  } catch {
    // Best-effort.
  }
}

export function readPendingProjectFocus(): string | null {
  try {
    return window.sessionStorage.getItem(PENDING_PROJECT_KEY);
  } catch {
    return null;
  }
}

export function clearPendingProjectFocus(): void {
  try {
    window.sessionStorage.removeItem(PENDING_PROJECT_KEY);
  } catch {
    // Best-effort.
  }
}

/** Compare workspace paths loosely: trailing slashes and case on macOS volumes. */
export function sameWorkspacePath(a: string, b: string): boolean {
  const normalize = (value: string) => value.replace(/[\\/]+$/, "").toLowerCase();
  return normalize(a) === normalize(b);
}
