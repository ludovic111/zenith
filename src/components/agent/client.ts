"use client";

import type { Provider } from "@/lib/agent/target";

/** The browser side of "Ask zenith" and Now: same-origin JSON calls to zenith's own routes. */

export type AskReply = { environmentId: string; threadId: string; href: string; target: string };

async function post<T>(url: string, body: unknown): Promise<T> {
  const res = await fetch(url, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
  const data = (await res.json().catch(() => ({}))) as T & { error?: string };
  if (!res.ok) throw new Error(data.error ?? `HTTP ${res.status}`);
  return data;
}

export const askZenith = (body: { prompt?: string; target?: string; provider?: Provider; nowId?: string; source?: "bar" | "command" | "now" }) =>
  post<AskReply>("/api/agent", body);

export const markNow = (id: string, action: "done" | "snooze" | "restore", hours?: number) => post<{ ok: true }>("/api/now", { id, action, hours });

/** Opens the ask box from anywhere (⌘K, the sidebar), prefilled when given a text. */
export function openAsk(text = "") {
  window.dispatchEvent(new CustomEvent("zenith:ask", { detail: { text } }));
}
