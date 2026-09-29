import "server-only";
import { readdir, readFile, stat } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { PROJECTS, PROJECTS_ROOT, projectDir, type ProjectId } from "../projects";
import { cached } from "../source";

export type Agent = "claude" | "codex";

export const AGENTS: Record<Agent, { name: string; color: string }> = {
  claude: { name: "Claude Code", color: "#D4724F" },
  codex: { name: "Codex", color: "#5B8DEF" },
};

export type Session = {
  agent: Agent;
  id: string;
  title: string;
  project: ProjectId | "zenith" | null;
  cwd: string;
  branch: string | null;
  start: number;
  end: number;
  turns: number;
  model: string | null;
  costUSD: number | null;
  tokens: number | null;
  linesAdded: number | null;
  linesRemoved: number | null;
  prs: { number: number; url: string }[];
  subagents: number;
  entrypoint: string | null;
  resume: string;
  /** Tranches de 15 min (epoch / 15 min) où la session a vraiment écrit quelque chose. */
  slots: number[];
};

const SLOT = 15 * 60e3;
/** Toutes les tranches de 15 min qui contiennent au moins un horodatage. */
function activitySlots(raw: string) {
  const set = new Set<number>();
  for (const m of raw.matchAll(/"timestamp":"([^"]+)"/g)) {
    const t = Date.parse(m[1]);
    if (Number.isFinite(t)) set.add(Math.floor(t / SLOT));
  }
  return [...set];
}

const CLAUDE_DIR = process.env.CLAUDE_HOME ?? path.join(os.homedir(), ".claude");
const CODEX_DIR = process.env.CODEX_HOME ?? path.join(os.homedir(), ".codex");

/** Folders attached to each project (worktrees included, plus `extraDirs`). */
const ROOTS: [string, Session["project"]][] = [
  ...PROJECTS.flatMap((p) => {
    const dir = projectDir(p);
    return [...(dir ? [dir] : []), ...p.extraDirs.map((d) => path.resolve(PROJECTS_ROOT, d))].map((d) => [d, p.id] as [string, ProjectId]);
  }),
  [process.cwd(), "zenith"],
];

function projectOf(cwd: string): Session["project"] {
  const hit = ROOTS.filter(([root]) => cwd === root || cwd.startsWith(root + "/")).sort((a, b) => b[0].length - a[0].length)[0];
  return hit?.[1] ?? null;
}

/** Ne parse que les lignes utiles : les transcriptions pèsent plusieurs Mo. */
function scan(raw: string, markers: string[]) {
  const out: Record<string, unknown>[] = [];
  for (const line of raw.split("\n")) if (markers.some((m) => line.includes(m))) {
    try {
      out.push(JSON.parse(line));
    } catch {}
  }
  return out;
}

const firstTimestamp = (raw: string) => /"timestamp":"([^"]+)"/.exec(raw)?.[1];
function lastTimestamp(raw: string) {
  const i = raw.lastIndexOf('"timestamp":"');
  return i < 0 ? undefined : raw.slice(i + 13, raw.indexOf('"', i + 13));
}

const memo = ((globalThis as { __zenithAgents?: Map<string, { key: string; s: Session | null }> }).__zenithAgents ??= new Map());

async function once(file: string, parse: (raw: string) => Session | null) {
  const st = await stat(file);
  const key = `${st.mtimeMs}:${st.size}`;
  const hit = memo.get(file);
  if (hit?.key === key) return hit.s;
  const s = parse(await readFile(file, "utf8"));
  memo.set(file, { key, s });
  return s;
}

type Line = Record<string, unknown> & { type?: string; message?: { model?: string; usage?: { output_tokens?: number } } };

function parseClaude(file: string, subagents: number) {
  return (raw: string): Session | null => {
    const lines = scan(raw, ['"type":"custom-title"', '"type":"cost-state"', '"type":"pr-link"', '"type":"last-prompt"', '"cwd":"', '"type":"assistant"']) as Line[];
    let title: string | null = null;
    let prompt: string | null = null;
    let cwd = "";
    let branch: string | null = null;
    let entrypoint: string | null = null;
    let model: string | null = null;
    let cost: Line | null = null;
    let turns = 0;
    let out = 0;
    const prs = new Map<number, string>();
    for (const l of lines) {
      if (l.type === "custom-title") title = String(l.customTitle ?? "") || title;
      else if (l.type === "last-prompt") prompt = String(l.lastPrompt ?? "") || prompt;
      else if (l.type === "cost-state") cost = l;
      else if (l.type === "pr-link") prs.set(Number(l.prNumber), String(l.prUrl));
      else if (l.type === "assistant") {
        model = l.message?.model ?? model;
        out += l.message?.usage?.output_tokens ?? 0;
      }
      if (typeof l.cwd === "string" && !cwd) cwd = l.cwd;
      if (typeof l.gitBranch === "string" && l.gitBranch) branch = l.gitBranch;
      if (typeof l.entrypoint === "string") entrypoint = l.entrypoint;
      if (l.type === "user" && (l as { turnOrigin?: string }).turnOrigin === "human") turns++;
    }
    const start = firstTimestamp(raw);
    const end = lastTimestamp(raw);
    if (!start || !end) return null;
    const id = path.basename(file, ".jsonl");
    const c = cost as (Line & { totalCostUSD?: number; totalLinesAdded?: number; totalLinesRemoved?: number }) | null;
    return {
      agent: "claude",
      id,
      title: title ?? prompt?.slice(0, 90) ?? "Session sans titre",
      project: projectOf(cwd),
      cwd,
      branch,
      start: new Date(start).getTime(),
      end: new Date(end).getTime(),
      turns,
      model,
      costUSD: c?.totalCostUSD ?? null,
      tokens: out || null,
      linesAdded: c?.totalLinesAdded ?? null,
      linesRemoved: c?.totalLinesRemoved ?? null,
      prs: [...prs.entries()].map(([number, url]) => ({ number, url })),
      subagents,
      entrypoint,
      resume: `cd ${JSON.stringify(cwd)} && claude --resume ${id}`,
      slots: activitySlots(raw),
    };
  };
}

async function claudeSessions(): Promise<Session[]> {
  const base = path.join(CLAUDE_DIR, "projects");
  const dirs = await readdir(base).catch(() => [] as string[]);
  const files: { file: string; subagents: number }[] = [];
  for (const d of dirs) {
    const entries = await readdir(path.join(base, d)).catch(() => [] as string[]);
    for (const f of entries.filter((e) => e.endsWith(".jsonl"))) {
      const id = f.slice(0, -6);
      const subs = entries.includes(id) ? (await readdir(path.join(base, d, id, "subagents")).catch(() => [])).length : 0;
      files.push({ file: path.join(base, d, f), subagents: subs });
    }
  }
  const all = await Promise.all(files.map(({ file, subagents }) => once(file, parseClaude(file, subagents)).catch(() => null)));
  return all.filter((s): s is Session => !!s);
}

async function walk(dir: string): Promise<string[]> {
  const entries = await readdir(dir, { withFileTypes: true }).catch(() => []);
  const nested = await Promise.all(entries.map((e) => (e.isDirectory() ? walk(path.join(dir, e.name)) : Promise.resolve(e.name.endsWith(".jsonl") ? [path.join(dir, e.name)] : []))));
  return nested.flat();
}

function parseCodex(titles: Map<string, string>) {
  return (raw: string): Session | null => {
    const lines = scan(raw, ['"type":"session_meta"', '"type":"turn_context"', '"token_count"', '"user_message"']);
    let meta: Record<string, unknown> | null = null;
    let model: string | null = null;
    let tokens: number | null = null;
    let turns = 0;
    let firstPrompt: string | null = null;
    for (const l of lines) {
      const p = (l.payload ?? {}) as Record<string, unknown>;
      if (l.type === "session_meta" && !meta) meta = p;
      else if (l.type === "turn_context") model = (p.model as string) ?? model;
      else if (l.type === "event_msg" && p.type === "token_count") {
        const info = p.info as { total_token_usage?: { total_tokens?: number } } | null;
        tokens = info?.total_token_usage?.total_tokens ?? tokens;
      } else if (l.type === "event_msg" && p.type === "user_message") {
        turns++;
        firstPrompt ??= typeof p.message === "string" ? p.message : null;
      }
    }
    if (!meta) return null;
    const id = String(meta.id);
    const cwd = String(meta.cwd ?? "");
    const end = lastTimestamp(raw);
    const git = meta.git as { branch?: string } | undefined;
    return {
      agent: "codex",
      id,
      title: titles.get(id) ?? firstPrompt?.slice(0, 90) ?? "Session sans titre",
      project: projectOf(cwd),
      cwd,
      branch: git?.branch ?? null,
      start: new Date(String(meta.timestamp)).getTime(),
      end: end ? new Date(end).getTime() : new Date(String(meta.timestamp)).getTime(),
      turns,
      model,
      costUSD: null,
      tokens,
      linesAdded: null,
      linesRemoved: null,
      prs: [],
      subagents: 0,
      entrypoint: (meta.originator as string) ?? null,
      resume: `codex resume ${id}`,
      slots: activitySlots(raw),
    };
  };
}

async function codexSessions(): Promise<Session[]> {
  const titles = new Map<string, string>();
  const index = await readFile(path.join(CODEX_DIR, "session_index.jsonl"), "utf8").catch(() => "");
  for (const line of index.split("\n").filter(Boolean)) {
    try {
      const d = JSON.parse(line);
      titles.set(d.id, d.thread_name);
    } catch {}
  }
  const files = await walk(path.join(CODEX_DIR, "sessions"));
  // Le titre peut changer sans que le fichier change : on n'utilise pas le cache pour lui.
  const all = await Promise.all(files.map((f) => once(f, parseCodex(titles)).catch(() => null)));
  return all.filter((s): s is Session => !!s).map((s) => ({ ...s, title: titles.get(s.id) ?? s.title }));
}

/** Toutes les sessions d'agents de ce Mac, de la plus récente à la plus ancienne. */
export const sessions = () =>
  cached("agents:sessions", 20, async () => {
    const [a, b] = await Promise.all([claudeSessions(), codexSessions()]);
    // Codex peut répartir un même fil sur plusieurs fichiers : on les fusionne.
    const merged = new Map<string, Session>();
    for (const s of [...a, ...b]) {
      const key = `${s.agent}:${s.id}`;
      const prev = merged.get(key);
      merged.set(
        key,
        prev
          ? {
              ...prev,
              start: Math.min(prev.start, s.start),
              end: Math.max(prev.end, s.end),
              turns: prev.turns + s.turns,
              tokens: prev.tokens == null && s.tokens == null ? null : (prev.tokens ?? 0) + (s.tokens ?? 0),
              model: prev.model ?? s.model,
              slots: [...new Set([...prev.slots, ...s.slots])],
            }
          : s,
      );
    }
    return [...merged.values()].sort((x, y) => y.end - x.end);
  });

/** Une session est « en cours » si son fichier a bougé il y a moins de 3 minutes. */
export const isLive = (s: Session) => Date.now() - s.end < 3 * 60e3;
