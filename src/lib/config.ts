import "server-only";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { ConfigSchema as Schema, PALETTE } from "./config-schema";
import { setL10n } from "./i18n";
import type { Config, ProjectConfig } from "./config-schema";

export type { Config, ProjectConfig, IdentityConfig, SubscriptionConfig, BrandName, NetworkName, RoutineConfig } from "./config-schema";
export { ConfigSchema } from "./config-schema";

/**
 * Everything personal lives in one file: ZENITH_CONFIG, else perso/zenith.config.json,
 * else zenith.config.json next to package.json. Git ignores both. Without it, zenith
 * starts empty and the settings page explains how to fill it in.
 */

/** The file that holds your data. */
export const CONFIG_FILE = process.env.ZENITH_CONFIG
  ? path.resolve(process.env.ZENITH_CONFIG)
  : [path.join(process.cwd(), "perso", "zenith.config.json"), path.join(process.cwd(), "zenith.config.json")].find((f) => existsSync(f)) ??
    path.join(process.cwd(), "zenith.config.json");

export type LoadedConfig = Omit<Config, "timezone"> & {
  timezone: string;
  /** Where the config came from, and what went wrong reading it. */
  meta: { file: string; found: boolean; error: string | null };
  projects: (ProjectConfig & { color: string; glow: string; href: string; notes: string })[];
  projectsRoot: string;
};

function load(): LoadedConfig {
  let raw: unknown = {};
  let found = false;
  let error: string | null = null;
  try {
    raw = JSON.parse(readFileSync(CONFIG_FILE, "utf8"));
    found = true;
  } catch (e) {
    const code = (e as NodeJS.ErrnoException).code;
    if (code !== "ENOENT") error = e instanceof Error ? e.message : String(e);
  }
  let parsed = Schema.safeParse(raw);
  if (!parsed.success) {
    error = parsed.error.issues.map((i) => `${i.path.join(".")}: ${i.message}`).join(" · ");
    parsed = Schema.safeParse({});
  }
  const c = parsed.data!;
  const seen = new Set<string>();
  const projects = c.projects
    .filter((p) => !seen.has(p.id) && seen.add(p.id))
    .map((p, i) => {
      const [color, glow] = PALETTE[i % PALETTE.length];
      const esc = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
      return {
        ...p,
        color: p.color ?? color,
        glow: p.glow ?? p.color ?? glow,
        href: p.href ?? `/p/${p.id}`,
        notes: p.notes ?? `${esc(p.id)}|${esc(p.name)}`,
      };
    });
  const projectsRoot = c.projectsRoot ? path.resolve(c.projectsRoot.replace(/^~(?=$|\/)/, process.env.HOME ?? "~")) : (process.env.PROJECTS_ROOT ?? path.resolve(process.cwd(), ".."));
  const timezone = c.timezone ?? (Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC");
  const out: LoadedConfig = { ...c, timezone, projects, projectsRoot, meta: { file: CONFIG_FILE, found, error } };
  setL10n({ locale: c.locale, timeZone: timezone, currency: c.currency });
  return out;
}

const g = globalThis as { __zenithConfig?: LoadedConfig };

/** The configuration, read once per server start. Restart zenith after editing the file. */
export const config = (): LoadedConfig => (g.__zenithConfig ??= load());

/** Re-reads the file (settings page, tests). */
export function reloadConfig() {
  g.__zenithConfig = load();
  return g.__zenithConfig;
}

config();
