import "server-only";
import os from "node:os";
import path from "node:path";
import { config, type BotConfig } from "../config";
import { PALETTE } from "../config-schema";
import { PROJECTS } from "../projects";
import type { Provider } from "./target";

/**
 * Your team: the main agent (its name is `agent.name`, its folder `agent.home`) and the
 * bots of `agent.bots`, named agents with a role, each on your Claude or your ChatGPT
 * (Codex) subscription. Each bot has its own folder next to the main one, with its own
 * personality and memory; they share what they know about you and their skills.
 */

const expand = (p: string) => p.replace(/^~(?=$|[/\\])/, os.homedir());

export const agentHome = () => path.resolve(expand(config().agent.home));

/** The main agent's name ("zenith" unless you renamed it). */
export const agentName = () => config().agent.name.trim() || "zenith";

/** Ids a bot can't take: they already name a destination. */
const RESERVED = new Set(["life", "zenith", "all"]);

export type Bot = BotConfig & { color: string; provider: Provider; home: string };

/** The enabled bots whose id is free, with a color, a provider and a folder. */
export function bots(): Bot[] {
  const c = config();
  const taken = new Set([...RESERVED, ...PROJECTS.map((p) => p.id)]);
  const root = path.join(path.dirname(agentHome()), "bots");
  return c.agent.bots
    .filter((b) => b.enabled && !taken.has(b.id))
    .map((b, i) => ({
      ...b,
      color: b.color ?? PALETTE[(i + 3) % PALETTE.length][0],
      provider: b.provider ?? c.agent.provider,
      home: path.join(root, b.id),
    }));
}

export const botById = (id: string | null | undefined) => (id ? bots().find((b) => b.id === id) ?? null : null);

/** The model a request should use, when the config names one for this agent and provider. */
export function configuredModel(bot: Bot | null, provider: Provider): string | undefined {
  if (bot?.model && provider === bot.provider) return bot.model;
  const c = config().agent;
  return c.model && provider === c.provider ? c.model : undefined;
}
