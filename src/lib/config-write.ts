import "server-only";
import { copyFile, mkdir, readdir, readFile, rename, unlink, writeFile } from "node:fs/promises";
import path from "node:path";
import { CONFIG_FILE, ConfigSchema, reloadConfig } from "./config";
import { serial } from "./agent/files";

/**
 * Writing zenith.config.json from the app (the welcome and the settings). A change is a
 * set of dotted paths (`{ "owner.name": "Ana", "agent.bots": [...] }`; null removes a
 * key), applied to the file as you wrote it — defaults aren't copied in. The result must
 * pass the schema before anything is written; the previous file is kept in
 * .data/config-backups (the last 20). Then zenith reloads it and refreshes what depends
 * on it, without a restart.
 */

const BACKUPS = path.join(process.cwd(), ".data", "config-backups");
const SCHEMA_URL = "./zenith.schema.json";

export async function rawConfig(): Promise<Record<string, unknown>> {
  try {
    const v = JSON.parse(await readFile(CONFIG_FILE, "utf8"));
    return v && typeof v === "object" && !Array.isArray(v) ? v : {};
  } catch {
    return {};
  }
}

function setPath(obj: Record<string, unknown>, dotted: string, value: unknown) {
  const keys = dotted.split(".");
  let at = obj;
  for (const k of keys.slice(0, -1)) {
    const next = at[k];
    if (!next || typeof next !== "object" || Array.isArray(next)) at[k] = {};
    at = at[k] as Record<string, unknown>;
  }
  const last = keys.at(-1)!;
  if (value === null || value === undefined) delete at[last];
  else at[last] = value;
}

export type ConfigIssue = { path: string; message: string };

/** Applies the change, or returns why it can't be (nothing is written then). */
export function updateConfig(set: Record<string, unknown>): Promise<{ ok: true } | { ok: false; issues: ConfigIssue[] }> {
  return serial(CONFIG_FILE, async () => {
    const raw = await rawConfig();
    for (const [k, v] of Object.entries(set)) {
      if (!/^[A-Za-z$][\w$]*(\.[A-Za-z$][\w$]*)*$/.test(k)) return { ok: false as const, issues: [{ path: k, message: "invalid key" }] };
      setPath(raw, k, v);
    }
    if (!raw.$schema && CONFIG_FILE.startsWith(process.cwd())) raw.$schema = path.relative(path.dirname(CONFIG_FILE), path.join(process.cwd(), SCHEMA_URL)) || SCHEMA_URL;
    const parsed = ConfigSchema.safeParse(raw);
    if (!parsed.success) return { ok: false as const, issues: parsed.error.issues.map((i) => ({ path: i.path.join("."), message: i.message })) };

    await mkdir(path.dirname(CONFIG_FILE), { recursive: true });
    await mkdir(BACKUPS, { recursive: true });
    await copyFile(CONFIG_FILE, path.join(BACKUPS, `zenith.config.${Date.now()}.json`)).catch(() => {});
    const old = (await readdir(BACKUPS).catch(() => [] as string[])).filter((f) => f.startsWith("zenith.config.")).sort();
    for (const f of old.slice(0, Math.max(0, old.length - 20))) await unlink(path.join(BACKUPS, f)).catch(() => {});
    const tmp = `${CONFIG_FILE}.${process.pid}.tmp`;
    await writeFile(tmp, JSON.stringify(raw, null, 2) + "\n");
    await rename(tmp, CONFIG_FILE);

    reloadConfig();
    await afterChange();
    return { ok: true as const };
  });
}

/** What depends on the config, brought up to date: caches, agent folders, routines, the brief. */
async function afterChange() {
  const g = globalThis as { __zenithCache?: Map<string, unknown> };
  g.__zenithCache?.clear();
  const [{ config }, { ensureTeam }, { startRoutines }, { startGateway }, { writeContext }] = await Promise.all([
    import("./config"),
    import("./agent/workspace"),
    import("./agent/routines"),
    import("./agent/gateway"),
    import("./context"),
  ]);
  if (config().agent.enabled) {
    await ensureTeam().catch((e) => console.error("[zenith] agent folders:", e));
    startRoutines();
    startGateway();
  }
  writeContext().catch(() => {});
}
