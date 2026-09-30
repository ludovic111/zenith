import "server-only";
import type { ComponentType, ReactNode } from "react";
import type { Project } from "./projects";
import type { Source } from "./source";
import { BUILTIN } from "./integrations";
import { PERSO } from "@perso";

/**
 * Extensions add numbers, events and brief facts to the shared pages without the
 * pages knowing about them. Built-in integrations (RevenueCat, App Store, GitHub
 * releases…) switch on from zenith.config.json; your own live in perso/ at the root,
 * which git ignores and next.config.ts plugs in when it exists (docs/extensions.md).
 */

export type EventKind = "commit" | "signup" | "release" | "deploy" | "trade" | "agent" | "review" | "sale";

export type Event = { project: string; at: number; kind: EventKind; text: string; href?: string };

export type Kpi = {
  label: string;
  value: number | null;
  /** Project whose color tints the tile. */
  project?: string;
  format?: Intl.NumberFormatOptions;
  suffix?: string;
  hint?: string;
};

export type CardStat = { label: string; value: string };

export type MoneyRow = { label: string; value: number | null; currency: string; sign: 1 | -1; hint: string; project?: string };

export type SourceGroup = "projects" | "around" | "app" | "claude";

export type SourceRow = {
  group: SourceGroup;
  name: string;
  feeds: string;
  vars: string;
  how: ReactNode;
  url?: string;
  src: Source<unknown>;
  /** Env var that can be pasted from the settings page. */
  key?: string;
  placeholder?: string;
  secret?: boolean;
};

export type Extension = {
  id: string;
  /** Overview: KPI tiles. */
  kpis?: () => Promise<Kpi[]>;
  /** Overview: up to three numbers on a project's card. The first extension that answers wins. */
  card?: (p: Project) => Promise<CardStat[] | null>;
  /** Overview: ticker and feed. */
  events?: () => Promise<Event[]>;
  /** Overview: money in and out. */
  money?: () => Promise<MoneyRow[]>;
  /** Agent brief: one-line facts about a project. */
  facts?: (p: Project) => Promise<string[]>;
  /** Agent brief: extra Markdown for a project's document. */
  context?: (p: Project) => Promise<string>;
  /** Settings page: the data sources this extension reads. */
  sources?: () => Promise<SourceRow[]>;
  /** Env vars the settings page may write to .env.local. */
  keys?: string[];
  /** Whole pages, served at /<slug> (a project's `href` can point there). */
  pages?: { slug: string; title: string; Page: ComponentType }[];
  /** API routes, served at /api/perso/<path> (GET only, same-origin). */
  routes?: { path: string; GET: (request: Request) => Promise<Response> | Response }[];
};

export const EXTENSIONS: Extension[] = [...BUILTIN, ...PERSO];

/** Runs `pick` on every extension that has it; a failing extension is skipped, never fatal. */
export async function collect<T>(pick: (e: Extension) => (() => Promise<T[]>) | undefined): Promise<T[]> {
  const lists = await Promise.all(
    EXTENSIONS.map((e) => {
      const fn = pick(e);
      return fn ? fn().catch((err) => (console.error(`[zenith] ${e.id}:`, err), [] as T[])) : Promise.resolve([] as T[]);
    }),
  );
  return lists.flat();
}

export const extensionPage = (slug: string) => EXTENSIONS.flatMap((e) => e.pages ?? []).find((p) => p.slug === slug) ?? null;

export const extensionRoute = (path: string) => EXTENSIONS.flatMap((e) => e.routes ?? []).find((r) => r.path === path) ?? null;

/** Env vars that the settings page may write. */
export const WRITABLE_KEYS = () => new Set(["RAILWAY_TOKEN", "GITHUB_TOKEN", "REVENUECAT_API_KEY", "OPENROUTER_API_KEY", "TELEGRAM_BOT_TOKEN", ...EXTENSIONS.flatMap((e) => e.keys ?? [])]);
