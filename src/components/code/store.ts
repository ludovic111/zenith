"use client";

import { useSyncExternalStore } from "react";
import type { CodeStatus } from "@/lib/code/manager";
import type { CodeTarget } from "@/lib/code/target";

/**
 * zenith code, as the rest of zenith sees it: its server status, and the projects and
 * threads the embedded app publishes (code/apps/web/src/zenith/embed.ts). The iframe
 * itself lives in <CodeHost>, mounted once in the root layout; everything else (the
 * sidebar, ⌘K, project pages) reads this store and talks to the app through it.
 */

// Messages exchanged with the embedded app (code/apps/web/src/zenith/embed.ts).
export const MSG = {
  pairRequest: "zenith-code:pair-request",
  pairToken: "zenith-code:pair-token",
  pairError: "zenith-code:pair-error",
  openProject: "zenith-code:open-project",
  ready: "zenith-code:ready",
  chrome: "zenith-code:chrome",
  navigate: "zenith-code:navigate",
  sidebar: "zenith-code:sidebar",
} as const;

export type CodeThreadStatus = "approval" | "input" | "working" | "connecting" | "plan" | "monitoring" | "completed" | "failed";

export type CodeProject = { environmentId: string; id: string; title: string; workspaceRoot: string };

export type CodeThread = {
  environmentId: string;
  id: string;
  projectId: string;
  title: string;
  branch: string | null;
  section: "pinned" | "active" | "snoozed" | "settled";
  status: CodeThreadStatus | null;
  activityAt: string;
};

export type CodeSnapshot = {
  projects: CodeProject[];
  threads: CodeThread[];
  /** `environmentId:threadId` of the thread open in the app. */
  activeThread: string | null;
  /** The app's own path, e.g. `/<environmentId>/<threadId>` or `/settings/general`. */
  pathname: string;
};

export type CodeNavigate =
  | { to: "thread"; environmentId: string; threadId: string }
  | { to: "new-thread"; environmentId: string; projectId: string }
  | { to: "path"; path: string }
  | { to: "palette"; open?: "add-project" | "new-thread-in" };

type State = {
  status: CodeStatus | null;
  snapshot: CodeSnapshot | null;
  /** The app is signed in and listening. */
  ready: boolean;
  /** A project to focus, from `/code?project=<id>`. */
  target: CodeTarget | null;
};

let state: State = { status: null, snapshot: null, ready: false, target: null };
const listeners = new Set<() => void>();
let poster: ((message: Record<string, unknown>) => void) | null = null;
let queue: Record<string, unknown>[] = [];

function set(patch: Partial<State>) {
  state = { ...state, ...patch };
  for (const l of listeners) l();
}

export const codeStore = {
  get: () => state,
  set,
  subscribe(listener: () => void) {
    listeners.add(listener);
    return () => listeners.delete(listener);
  },
  /** Called by <CodeHost> when the app is (or stops being) ready to receive messages. */
  attach(post: ((message: Record<string, unknown>) => void) | null) {
    poster = post;
    set({ ready: !!post });
    if (!post) return;
    const pending = queue;
    queue = [];
    for (const m of pending) post(m);
  },
};

const SERVER_STATE: State = { status: null, snapshot: null, ready: false, target: null };

export function useCode(): State {
  return useSyncExternalStore(codeStore.subscribe, codeStore.get, () => SERVER_STATE);
}

/** Post to the app now, or as soon as it is ready. */
export function sendToCode(message: Record<string, unknown>) {
  if (poster && state.ready) poster(message);
  else queue.push(message);
}

export function codeNavigate(request: CodeNavigate) {
  sendToCode({ type: MSG.navigate, request });
  focusCode();
}

/** Keyboard focus into the app (a thread's composer, its command palette). */
export function focusCode() {
  window.dispatchEvent(new Event("zenith:code-focus"));
}

export const threadKey = (t: Pick<CodeThread, "environmentId" | "id">) => `${t.environmentId}:${t.id}`;

/** zenith URL of a path inside the app: `/` → `/code`, `/a/b` → `/code/a/b`. */
export const codeHref = (appPath: string) => (appPath === "/" || !appPath ? "/code" : `/code${appPath}`);

export const threadHref = (t: Pick<CodeThread, "environmentId" | "id">) =>
  codeHref(`/${encodeURIComponent(t.environmentId)}/${encodeURIComponent(t.id)}`);

/** The app path of a zenith URL under /code, or null elsewhere (`/code` alone → ""). */
export function appPathOf(pathname: string): string | null {
  if (pathname === "/code") return "";
  return pathname.startsWith("/code/") ? pathname.slice("/code".length) : null;
}

/** Same folder, give or take a trailing slash and case (macOS volumes). */
export function sameDir(a: string, b: string) {
  const n = (s: string) => s.replace(/[\\/]+$/, "").toLowerCase();
  return n(a) === n(b);
}

/** Threads needing the user first; then by the app's own order. */
export const NEEDS_YOU: ReadonlySet<CodeThreadStatus> = new Set(["approval", "input", "plan", "failed"]);

export const STATUS_STYLE: Record<CodeThreadStatus, { dot: string; pulse: boolean; label: () => [string, string] }> = {
  approval: { dot: "#fcd34d", pulse: false, label: () => ["Approbation requise", "Pending approval"] },
  input: { dot: "#a5b4fc", pulse: false, label: () => ["Attend ta réponse", "Awaiting input"] },
  working: { dot: "#7dd3fc", pulse: true, label: () => ["En cours", "Working"] },
  connecting: { dot: "#7dd3fc", pulse: true, label: () => ["Connexion…", "Connecting"] },
  plan: { dot: "#c4b5fd", pulse: false, label: () => ["Plan prêt", "Plan ready"] },
  monitoring: { dot: "#7dd3fc", pulse: false, label: () => ["Surveille", "Monitoring"] },
  completed: { dot: "#6ee7b7", pulse: false, label: () => ["Terminé", "Completed"] },
  failed: { dot: "#fb7185", pulse: false, label: () => ["Échec", "Failed"] },
};

const PRIORITY: Record<CodeThreadStatus, number> = {
  failed: 7,
  approval: 6,
  input: 5,
  working: 4,
  connecting: 4,
  plan: 3,
  monitoring: 2,
  completed: 1,
};

/** The most urgent status among some threads, for a project or the whole app. */
export function topStatus(threads: CodeThread[]): CodeThreadStatus | null {
  let top: CodeThreadStatus | null = null;
  for (const t of threads) if (t.status && (!top || PRIORITY[t.status] > PRIORITY[top])) top = t.status;
  return top;
}
