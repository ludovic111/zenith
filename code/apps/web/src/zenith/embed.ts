/**
 * zenith embedding: when this app runs inside the zenith dashboard (an iframe),
 * it asks the parent page for one-time pairing tokens and takes project focus
 * requests from it. Only parents listed by the server (/zenith/embed.json,
 * from ZENITH_CODE_PARENT_ORIGINS) are ever talked to or listened to.
 */

export const ZENITH_MESSAGE = {
  pairRequest: "zenith-code:pair-request",
  pairToken: "zenith-code:pair-token",
  pairError: "zenith-code:pair-error",
  openProject: "zenith-code:open-project",
  ready: "zenith-code:ready",
} as const;

const EMBED_CONFIG_PATH = "/zenith/embed.json";
const PENDING_PROJECT_KEY = "zenith:open-project";
const PROJECT_SEARCH_PARAM = "zenithProject";

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
    if (!path) return;
    window.sessionStorage.setItem(PENDING_PROJECT_KEY, path);
    url.searchParams.delete(PROJECT_SEARCH_PARAM);
    window.history.replaceState(window.history.state, "", url.toString());
  } catch {
    // Storage can be unavailable; the focus request is best-effort.
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
