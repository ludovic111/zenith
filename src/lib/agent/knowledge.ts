import "server-only";
import { execFile } from "node:child_process";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { tr } from "../i18n";
import { shell, threadDetail } from "../code/api";
import { members } from "./talk";
import { agentHome } from "./team";
import { skillsDir } from "./skills";

/**
 * What the team knows, for the team: its recent conversations (to learn from), a search
 * across everything it wrote down and said (to remember), and a way to reach you (to be
 * proactive without being noisy).
 */

const same = (a: string, b: string) => a.replace(/[\\/]+$/, "").toLowerCase() === b.replace(/[\\/]+$/, "").toLowerCase();
const clip = (s: string, n: number) => (s.length > n ? `${s.slice(0, n).trimEnd()}…` : s);

export type Conversation = { agent: string; name: string; title: string; at: string; messages: { role: string; text: string }[] };

/** The team's conversations updated in the last `hours` (one agent's with `agent`), newest first. */
export async function history({ hours = 24, agent, limit = 30 }: { hours?: number; agent?: string; limit?: number }): Promise<Conversation[]> {
  const s = await shell();
  const since = Date.now() - hours * 3600e3;
  const team = members().filter((m) => !agent || m.id === agent);
  const threads = team.flatMap((m) => {
    const project = s.projects.find((p) => !p.deletedAt && same(p.workspaceRoot, m.home));
    return project ? s.threads.filter((t) => t.projectId === project.id && Date.parse(t.updatedAt) >= since).map((t) => ({ t, m })) : [];
  });
  threads.sort((a, b) => b.t.updatedAt.localeCompare(a.t.updatedAt));
  const out: Conversation[] = [];
  for (const { t, m } of threads.slice(0, limit)) {
    const d = await threadDetail(t.id).catch(() => null);
    if (!d) continue;
    out.push({
      agent: m.id,
      name: m.name,
      title: t.title,
      at: t.updatedAt,
      messages: d.messages.filter((x) => x.role !== "system" && x.text.trim()).slice(-14).map((x) => ({ role: x.role, text: clip(x.text.trim(), 1500) })),
    });
  }
  return out;
}

/** Every file the team keeps: profile, personalities, memories, skills, journal, ideas. */
async function knowledgeFiles(): Promise<{ file: string; label: string }[]> {
  const main = agentHome();
  const files = [
    { file: path.join(main, "USER.md"), label: "USER.md" },
    { file: path.join(main, "IMPROVE.md"), label: "IMPROVE.md" },
  ];
  for (const m of members()) {
    files.push({ file: path.join(m.home, "MEMORY.md"), label: `${m.name} · MEMORY.md` }, { file: path.join(m.home, "SOUL.md"), label: `${m.name} · SOUL.md` });
  }
  for (const d of await readdir(skillsDir(), { withFileTypes: true }).catch(() => [])) {
    if (d.isDirectory() && !d.name.startsWith(".")) files.push({ file: path.join(skillsDir(), d.name, "SKILL.md"), label: `skill ${d.name}` });
  }
  for (const f of (await readdir(path.join(main, "journal")).catch(() => [] as string[])).filter((f) => f.endsWith(".md")).sort().reverse().slice(0, 60)) {
    files.push({ file: path.join(main, "journal", f), label: `journal/${f}` });
  }
  return files;
}

const fold = (s: string) => s.toLowerCase().normalize("NFD").replace(/[̀-ͯ]/g, "");

/** Where the team wrote or said something about `query`: files first, then the last week's conversations. */
export async function recall(query: string): Promise<string> {
  const words = fold(query).split(/\s+/).filter((w) => w.length > 2);
  if (!words.length) return tr("Dis ce que tu cherches.", "Say what you are looking for.");
  const hit = (text: string) => words.every((w) => fold(text).includes(w)) || fold(text).includes(fold(query));
  const out: string[] = [];
  for (const { file, label } of await knowledgeFiles()) {
    const body = await readFile(file, "utf8").catch(() => "");
    const lines = body.split("\n").filter((l) => l.trim() && hit(l));
    if (lines.length) out.push(`## ${label}\n\n${lines.slice(0, 8).map((l) => `- ${clip(l.trim(), 300)}`).join("\n")}`);
  }
  const convs = await history({ hours: 24 * 7, limit: 60 }).catch(() => []);
  for (const c of convs) {
    const said = c.messages.filter((m) => hit(m.text));
    if (hit(c.title) || said.length) out.push(`## ${c.name} · ${c.title} (${c.at.slice(0, 10)})\n\n${said.slice(0, 3).map((m) => `- ${m.role}: ${clip(m.text, 400)}`).join("\n")}`);
    if (out.length > 25) break;
  }
  return out.length ? out.join("\n\n") : tr(`Rien sur « ${query} » dans ce que l'équipe sait.`, `Nothing about "${query}" in what the team knows.`);
}

/** A Mac notification, and Telegram when it is plugged in. */
export async function notify(text: string, from: string | null): Promise<{ mac: boolean; telegram: boolean }> {
  const name = from ? members().find((m) => m.id === from)?.name : null;
  const title = name ? `zenith · ${name}` : "zenith";
  const body = clip(text.replace(/\s+/g, " ").trim(), 240);
  const esc = (s: string) => s.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
  const mac = await new Promise<boolean>((resolve) =>
    execFile("osascript", ["-e", `display notification "${esc(body)}" with title "${esc(title)}"`], (e) => resolve(!e)),
  );
  const { sendToChats } = await import("./gateway");
  const telegram = await sendToChats(`${title}\n\n${text.trim()}`).catch(() => false);
  return { mac, telegram };
}
