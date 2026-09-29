import "server-only";

/** Résultat d'une source : soit des données, soit une raison lisible. */
export type Source<T> =
  | { ok: true; data: T }
  | { ok: false; missing?: string[]; error?: string };

export class MissingConfig extends Error {
  constructor(public vars: string[]) {
    super(`Variables manquantes : ${vars.join(", ")}`);
  }
}

export function need(...names: string[]): string[] {
  const missing = names.filter((n) => !process.env[n]);
  if (missing.length) throw new MissingConfig(missing);
  return names.map((n) => process.env[n]!);
}

const store = ((globalThis as { __zenithCache?: Map<string, { at: number; value: Promise<unknown> }> })
  .__zenithCache ??= new Map());

/** Mémoïse une promesse `ttl` secondes ; une erreur n'est jamais gardée. */
export function cached<T>(key: string, ttl: number, fn: () => Promise<T>): Promise<T> {
  const hit = store.get(key);
  if (hit && Date.now() - hit.at < ttl * 1000) return hit.value as Promise<T>;
  const value = fn();
  store.set(key, { at: Date.now(), value });
  value.catch(() => store.delete(key));
  return value;
}

export async function source<T>(fn: () => Promise<T>): Promise<Source<T>> {
  try {
    return { ok: true, data: await fn() };
  } catch (e) {
    if (e instanceof MissingConfig) return { ok: false, missing: e.vars };
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

export async function getJson<T>(url: string, init: RequestInit & { timeout?: number } = {}): Promise<T> {
  const res = await fetch(url, {
    ...init,
    cache: "no-store",
    signal: AbortSignal.timeout(init.timeout ?? 12000),
  });
  if (!res.ok) {
    const body = await res.text().catch(() => "");
    throw new Error(`${res.status} ${res.statusText}${body ? ` — ${body.slice(0, 160)}` : ""}`);
  }
  return res.json() as Promise<T>;
}
