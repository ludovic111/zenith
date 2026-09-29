import "server-only";
import { readdir, readFile, stat } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { l10n, tr } from "../i18n";
import { cached, getJson } from "../source";

export type Window = { label: string; percentUsed: number; resetsAt: string | null };
export type PlanUsage = { plan: string; windows: Window[]; capturedAt: string; live: boolean };

const CODEX_DIR = process.env.CODEX_HOME ?? path.join(os.homedir(), ".codex");

/** Codex limits: Codex writes its quota state into each session; read the most recent one. */
export const codexPlan = () =>
  cached("plan:codex", 60, async (): Promise<PlanUsage | null> => {
    const files: { f: string; t: number }[] = [];
    const walk = async (dir: string): Promise<void> => {
      for (const e of await readdir(dir, { withFileTypes: true }).catch(() => [])) {
        const p = path.join(dir, e.name);
        if (e.isDirectory()) await walk(p);
        else if (e.name.endsWith(".jsonl")) files.push({ f: p, t: (await stat(p)).mtimeMs });
      }
    };
    await walk(path.join(CODEX_DIR, "sessions"));
    for (const { f } of files.sort((a, b) => b.t - a.t).slice(0, 5)) {
      const raw = await readFile(f, "utf8");
      const i = raw.lastIndexOf('"rate_limits":');
      if (i < 0) continue;
      const line = raw.slice(raw.lastIndexOf("\n", i) + 1, raw.indexOf("\n", i) === -1 ? undefined : raw.indexOf("\n", i));
      try {
        const d = JSON.parse(line);
        const rl = d.payload?.rate_limits ?? d.payload?.info?.rate_limits;
        if (!rl) continue;
        const win = (w: { used_percent: number; window_minutes: number; resets_at: number } | null, fallback: string): Window | null =>
          w ? { label: w.window_minutes >= 10080 ? tr("Semaine", "Week") : w.window_minutes >= 300 ? tr("5 heures", "5 hours") : fallback, percentUsed: w.used_percent, resetsAt: new Date(w.resets_at * 1000).toISOString() } : null;
        return {
          plan: `ChatGPT ${String(rl.plan_type ?? "").replace(/^./, (c: string) => c.toUpperCase())}`,
          windows: [win(rl.primary, tr("Fenêtre courte", "Short window")), win(rl.secondary, tr("Fenêtre longue", "Long window"))].filter((w): w is Window => !!w),
          capturedAt: d.timestamp ?? new Date().toISOString(),
          live: true,
        };
      } catch {}
    }
    return null;
  });

/**
 * Claude limits: no local file exposes them, so Claude reads them and writes
 * .data/claude-plan.json (ask Claude "update my Claude limits in zenith").
 */
export const claudePlan = () =>
  cached("plan:claude", 30, async (): Promise<PlanUsage | null> => {
    const raw = await readFile(path.join(process.cwd(), ".data", "claude-plan.json"), "utf8").catch(() => null);
    return raw ? { ...(JSON.parse(raw) as PlanUsage), live: false } : null;
  });

/** Rough value of one unit in USD, used only when the rates API does not answer. */
const FALLBACK_USD: Record<string, number> = { USD: 1, EUR: 1.1, CHF: 1.15, GBP: 1.3, CAD: 0.73, AUD: 0.66, JPY: 0.0068, SEK: 0.095, NOK: 0.093, DKK: 0.15, PLN: 0.25 };

export type Rates = {
  /** Your currency (config `currency`). */
  base: string;
  /** Value of one unit of each currency in `base`: `amount * toBase[currency]`. */
  toBase: Record<string, number>;
  /** @deprecated Same object as `toBase` (named when the base was always CHF). */
  toCHF: Record<string, number>;
  /** Date of the ECB rates, null when falling back to built-in approximations. */
  date: string | null;
};

/** Today's exchange rates (ECB via Frankfurter), to bring every amount to your currency. */
export const fx = () => {
  const base = l10n().currency.toUpperCase();
  return cached(`fx:${base}`, 12 * 3600, async (): Promise<Rates> => {
    const res = await getJson<{ rates: Record<string, number>; date: string }>(`https://api.frankfurter.dev/v1/latest?base=${encodeURIComponent(base)}`).catch(() => null);
    const toBase: Record<string, number> = { [base]: 1 };
    if (res?.rates) {
      // Frankfurter gives units of X per 1 base: invert.
      for (const [cur, r] of Object.entries(res.rates)) if (r) toBase[cur] = 1 / r;
    } else if (FALLBACK_USD[base]) {
      // Careful fallback when the API does not answer (or does not know this currency).
      for (const [cur, usd] of Object.entries(FALLBACK_USD)) toBase[cur] = usd / FALLBACK_USD[base];
    }
    return { base, toBase, toCHF: toBase, date: res?.date ?? null };
  });
};
