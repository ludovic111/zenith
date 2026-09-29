import "server-only";
import { readdir, readFile, stat } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { config } from "../config";
import { PROJECTS, type ProjectId } from "../projects";
import { cached } from "../source";

const home = (p: string) => p.replace(/^~(?=$|\/)/, os.homedir());

/** Obsidian vault: OBSIDIAN_VAULT or `obsidian.vault`, else the one open in Obsidian, else `obsidian.fallback`. */
export const vaultPath = () =>
  cached("obsidian:path", 3600, async () => {
    const o = config().obsidian;
    if (process.env.OBSIDIAN_VAULT) return home(process.env.OBSIDIAN_VAULT);
    if (o.vault) return home(o.vault);
    try {
      const cfg = JSON.parse(await readFile(path.join(os.homedir(), "Library/Application Support/obsidian/obsidian.json"), "utf8"));
      const open = Object.values(cfg.vaults ?? {}).find((v) => (v as { open?: boolean }).open) as { path: string } | undefined;
      if (open) return open.path;
    } catch {}
    if (o.fallback) return home(o.fallback);
    throw new Error("No Obsidian vault found (open one in Obsidian, or set obsidian.vault in zenith.config.json)");
  });

/** Folder where zenith writes its own notes in the vault: nothing else is ever touched. */
export const EXPORT_DIR = config().obsidian.exportDir;

export type Note = {
  path: string;
  title: string;
  folder: string;
  modified: number;
  size: number;
  excerpt: string;
  tasks: { text: string; done: boolean }[];
  project: ProjectId | null;
  url: string;
};

/** Links a note to a project from its folder or name (`notes` regex of each project). */
const PROJECT_HINTS: [RegExp, ProjectId][] = PROJECTS.flatMap((p) => {
  try {
    return [[new RegExp(p.notes, "i"), p.id] as [RegExp, ProjectId]];
  } catch {
    return [];
  }
});

function tasksOf(body: string) {
  const out: { text: string; done: boolean }[] = [];
  for (const m of body.matchAll(/^\s*[-*]\s+\[( |x|X)\]\s+(.+)$/gm)) out.push({ text: m[2].trim(), done: m[1] !== " " });
  return out;
}

async function walk(dir: string, root: string, out: string[] = []) {
  for (const e of await readdir(dir, { withFileTypes: true }).catch(() => [])) {
    if (e.name.startsWith(".")) continue;
    const p = path.join(dir, e.name);
    if (e.isDirectory()) {
      if (path.relative(root, p) !== EXPORT_DIR) await walk(p, root, out);
    } else if (e.name.endsWith(".md")) out.push(p);
  }
  return out;
}

export const notes = () =>
  cached("obsidian:notes", 60, async () => {
    const root = await vaultPath();
    const vault = path.basename(root);
    const files = await walk(root, root);
    const list: Note[] = await Promise.all(
      files.map(async (f) => {
        const [body, st] = await Promise.all([readFile(f, "utf8"), stat(f)]);
        const rel = path.relative(root, f);
        const title = path.basename(f, ".md");
        const folder = path.dirname(rel) === "." ? "" : path.dirname(rel);
        const text = body.replace(/^---[\s\S]*?---\s*/, "");
        return {
          path: rel,
          title,
          folder,
          modified: st.mtimeMs,
          size: st.size,
          excerpt: text.replace(/[#>*`[\]|]/g, " ").replace(/\s+/g, " ").trim().slice(0, 220),
          tasks: tasksOf(text),
          project: PROJECT_HINTS.find(([re]) => re.test(rel))?.[1] ?? null,
          url: `obsidian://open?vault=${encodeURIComponent(vault)}&file=${encodeURIComponent(rel.replace(/\.md$/, ""))}`,
        };
      }),
    );
    return { root, vault, notes: list.sort((a, b) => b.modified - a.modified) };
  });

/**
 * La note Todo : chaque ligne non vide est une tâche (cases à cocher Markdown
 * comprises), dans l'ordre où tu les as écrites.
 */
export const todo = () =>
  cached("obsidian:todo", 60, async () => {
    const { notes: list } = await notes();
    const note = list.find((n) => /^todo$/i.test(n.title) && !n.folder);
    if (!note) return null;
    const root = await vaultPath();
    const body = await readFile(path.join(root, note.path), "utf8");
    const items = body
      .split("\n")
      .map((l) => l.trim())
      .filter((l) => l && !l.startsWith("#"))
      .map((l) => {
        const m = /^[-*]\s+\[( |x|X)\]\s+(.+)$/.exec(l);
        return m ? { text: m[2], done: m[1] !== " " } : { text: l.replace(/^[-*]\s+/, ""), done: false };
      });
    return { note, items };
  });
