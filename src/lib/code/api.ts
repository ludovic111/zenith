import "server-only";
import { randomUUID } from "node:crypto";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { issueSession } from "./cli";
import { codeHome, codeOrigin } from "./paths";

/**
 * zenith code's environment HTTP API, from zenith's server: read projects and threads,
 * dispatch orchestration commands (create a thread, start a turn). Authenticated with a
 * bearer session zenith issues itself through the CLI, kept in memory and renewed
 * before it expires or when the server stops accepting it.
 */

export type ModelSelection = { instanceId: string; model: string; options?: { id: string; value: unknown }[] };

export type ShellProject = { id: string; title: string; workspaceRoot: string; deletedAt?: string | null };

export type ShellThread = {
  id: string;
  projectId: string;
  title: string;
  modelSelection: ModelSelection | null;
  runtimeMode: string;
  archivedAt: string | null;
  createdAt: string;
  updatedAt: string;
  latestTurn: { state: string; completedAt: string | null } | null;
  session: { status: string; lastError: string | null } | null;
  hasPendingApprovals: boolean;
  hasPendingUserInput: boolean;
};

export type Shell = { projects: ShellProject[]; threads: ShellThread[] };

export type ThreadMessage = { id: string; role: "user" | "assistant" | "system"; text: string; createdAt?: string };

type Session = { token: string; expiresAt: number };
const g = globalThis as { __zenithCodeSession?: Promise<Session> | null };

/** Issues a new session, unless another caller already replaced `stale` with a fresh one. */
function renew(stale: Promise<Session> | null | undefined): Promise<Session> {
  const current = g.__zenithCodeSession;
  if (current && current !== stale) return current;
  const next = issueSession();
  g.__zenithCodeSession = next;
  next.catch(() => {
    if (g.__zenithCodeSession === next) g.__zenithCodeSession = null;
  });
  return next;
}

/** After a refused token: a fresh session, issued once however many calls were refused. */
async function renewAfter(refused: string): Promise<Session> {
  const current = g.__zenithCodeSession;
  const s = current ? await current.catch(() => null) : null;
  if (s && s.token !== refused) return s;
  return renew(current);
}

function session(): Promise<Session> {
  const current = g.__zenithCodeSession;
  if (!current) return renew(null);
  return current.then((s) => (s.expiresAt - Date.now() > 10 * 60_000 ? s : renew(current)));
}

async function call<T>(pathname: string, init: RequestInit = {}, retried = false): Promise<T> {
  const { token } = await session();
  const res = await fetch(`${codeOrigin()}${pathname}`, {
    ...init,
    cache: "no-store",
    signal: AbortSignal.timeout(15_000),
    headers: { ...(init.body ? { "content-type": "application/json" } : {}), authorization: `Bearer ${token}`, ...init.headers },
  });
  if (res.status === 401 && !retried) {
    await renewAfter(token);
    return call(pathname, init, true);
  }
  const body = await res.text();
  if (!res.ok) {
    let reason = `${res.status}`;
    try {
      const err = JSON.parse(body) as { reason?: string; code?: string };
      reason = err.reason ?? err.code ?? reason;
    } catch {}
    throw new Error(`zenith code: ${reason}`);
  }
  return (body ? JSON.parse(body) : null) as T;
}

/** Every project and thread (archived ones included, deleted ones not). */
export const shell = () => call<Shell>("/api/orchestration/shell");

export const threadDetail = (threadId: string) =>
  call<{ thread: { id: string; title: string; messages: ThreadMessage[]; latestTurn: ShellThread["latestTurn"]; session: ShellThread["session"] } }>(
    `/api/orchestration/threads/${encodeURIComponent(threadId)}`,
  ).then((r) => r.thread);

export function dispatch(command: Record<string, unknown>) {
  return call<{ sequence: number }>("/api/orchestration/dispatch", {
    method: "POST",
    body: JSON.stringify({ commandId: randomUUID(), ...command }),
  });
}

/** The id zenith code gives this machine's environment; it is part of every thread URL. */
export async function environmentId(): Promise<string> {
  return (await readFile(path.join(codeHome(), "userdata", "environment-id"), "utf8")).trim();
}

/** zenith code's server settings: default runtime mode, provider instances, default model. */
export async function codeSettings(): Promise<{
  defaultRuntimeMode?: string;
  defaultModelSelection?: ModelSelection | null;
  providerInstances?: Record<string, { driver?: string; enabled?: boolean }>;
  projectSettingsOverrides?: Record<string, { defaultRuntimeMode?: string }>;
}> {
  try {
    return JSON.parse(await readFile(path.join(codeHome(), "userdata", "settings.json"), "utf8"));
  } catch {
    return {};
  }
}
