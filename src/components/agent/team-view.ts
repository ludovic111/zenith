import "server-only";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { config } from "@/lib/config";
import { tr } from "@/lib/i18n";
import { askLog } from "@/lib/agent/ask";
import { gatewayStatus } from "@/lib/agent/gateway";
import { skills } from "@/lib/agent/skills";
import { agentHome, agentName, bots, configuredModel } from "@/lib/agent/team";
import { agentUi } from "@/lib/agent/ui";
import type { ActivityView, MemberView, SkillView } from "./team";

const tilde = (p: string) => (process.env.HOME && p.startsWith(process.env.HOME) ? `~${p.slice(process.env.HOME.length)}` : p);

/** Lines of a memory file that say something (not its title, notes or blank lines). */
async function memoryLines(file: string) {
  const body = await readFile(file, "utf8").catch(() => "");
  return body.split("\n").filter((l) => /^\s*[-*\d]/.test(l)).length;
}

/** The team as the AI agents page shows it: the main agent first, then the bots. */
export async function teamView(): Promise<MemberView[]> {
  const c = config().agent;
  const main: MemberView = {
    id: "life",
    name: agentName(),
    emoji: "✦",
    color: "#D9A21B",
    provider: c.provider,
    model: configuredModel(null, c.provider) ?? null,
    role: tr("L'agent principal : ta vie, tes messages, ton agenda. Il confie le reste à l'équipe.", "The main agent: your life, messages and calendar. It hands the rest to the team."),
    home: tilde(agentHome()),
    memory: await memoryLines(path.join(agentHome(), "MEMORY.md")),
    main: true,
  };
  const team = await Promise.all(
    bots().map(async (b): Promise<MemberView> => ({
      id: b.id,
      name: b.name,
      emoji: b.emoji ?? null,
      color: b.color,
      provider: b.provider,
      model: configuredModel(b, b.provider) ?? null,
      role: b.role,
      home: tilde(b.home),
      memory: await memoryLines(path.join(b.home, "MEMORY.md")),
      main: false,
    })),
  );
  return [main, ...team];
}

export async function skillViews(): Promise<SkillView[]> {
  return (await skills()).map((s) => ({ id: s.id, description: s.description, file: tilde(s.file) }));
}

/** The latest requests zenith made, from anywhere (Dots' "Activity"). */
export async function activityViews(limit = 12): Promise<ActivityView[]> {
  const names = Object.fromEntries(agentUi().targets.map((t) => [t.id, t.name]));
  return (await askLog()).slice(0, limit).map((e) => ({
    at: e.at,
    source: e.source,
    target: names[e.target] ?? e.target,
    title: e.title,
    threadId: e.threadId,
    environmentId: e.environmentId,
  }));
}

export { gatewayStatus };
