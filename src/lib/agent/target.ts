/**
 * Where a request to zenith goes: one of your projects when it names one (its agent works
 * in that folder), else zenith's own folder, where the agent sees your whole life and can
 * hand work to project agents. Pure, so the ask bar shows the destination as you type.
 */

export const LIFE = "life";

export type AgentTarget = {
  id: string;
  name: string;
  glow: string;
  emoji?: string;
  /** Lowercase words that name it: id, name, folder, repository, single-word identity names. */
  aliases: string[];
};

export type Provider = "claude" | "codex";

const fold = (s: string) =>
  s
    .toLowerCase()
    .normalize("NFD")
    .replace(/[̀-ͯ]/g, "");

const escape = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

/** Aliases worth matching: long enough, without spaces. */
export function aliasesOf(...names: (string | null | undefined)[]): string[] {
  const out = new Set<string>();
  for (const n of names) {
    if (!n) continue;
    const f = fold(n.trim());
    if (f.length >= 3 && !/\s/.test(f)) out.add(f);
  }
  return [...out];
}

/**
 * The target a sentence is about. `@id` at the start forces it; otherwise the single
 * project it names; several projects, or none, go to your life agent.
 */
export function guessTarget(text: string, targets: AgentTarget[]): string {
  const t = fold(text);
  const forced = /^@([a-z0-9-]+)/.exec(t.trim());
  if (forced) {
    const hit = targets.find((x) => x.id === forced[1] || x.aliases.includes(forced[1]));
    if (hit) return hit.id;
  }
  const named = targets.filter(
    (x) => x.id !== LIFE && x.aliases.some((a) => new RegExp(`(^|[^a-z0-9])${escape(a)}($|[^a-z0-9])`).test(t)),
  );
  return named.length === 1 ? named[0].id : LIFE;
}

/** The request without a leading `@target`. */
export const stripTarget = (text: string) => text.replace(/^\s*@[\w-]+\s*/, "").trim();
