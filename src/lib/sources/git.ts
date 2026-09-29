import "server-only";
import { execFile } from "node:child_process";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { promisify } from "node:util";
import { PROJECTS, projectDir, type ProjectId } from "../projects";
import { cached } from "../source";

const run = promisify(execFile);
const SEP = "\x1f";

export type Commit = { project: ProjectId; hash: string; at: number; subject: string; author: string };

export type LocalRepo = {
  project: ProjectId;
  branch: string;
  dirty: number;
  ahead: number;
  commits: Commit[];
};

async function git(dir: string, args: string[]) {
  const { stdout } = await run("git", ["-C", dir, ...args], { timeout: 10000, maxBuffer: 16 * 1024 * 1024 });
  return stdout;
}

function dirOf(id: ProjectId) {
  const p = PROJECTS.find((x) => x.id === id);
  const dir = p ? projectDir(p) : null;
  if (!dir) throw new Error(`No local folder for ${id} (set "dir" in zenith.config.json)`);
  return dir;
}

/** Local repository state: branch, uncommitted files, commits of the last 26 weeks. */
export const localRepo = (id: ProjectId) =>
  cached(`git:${id}`, 60, async (): Promise<LocalRepo> => {
    const dir = dirOf(id);
    const [branch, status, log, ahead] = await Promise.all([
      git(dir, ["rev-parse", "--abbrev-ref", "HEAD"]),
      git(dir, ["status", "--porcelain"]),
      git(dir, ["log", "--all", "--no-merges", "--since=26 weeks ago", `--format=%H${SEP}%at${SEP}%s${SEP}%an`]),
      git(dir, ["rev-list", "--count", "@{upstream}..HEAD"]).catch(() => "0"),
    ]);
    return {
      project: id,
      branch: branch.trim(),
      dirty: status.split("\n").filter(Boolean).length,
      ahead: Number(ahead.trim()) || 0,
      // --all voit aussi les copies rebasées d'un même commit : on garde une seule fois (date d'auteur, sujet).
      commits: [
        ...new Map(
          log
            .split("\n")
            .filter(Boolean)
            .map((line) => {
              const [hash, at, subject, author] = line.split(SEP);
              return { project: id, hash, at: Number(at) * 1000, subject, author };
            })
            .map((c) => [`${c.at}:${c.subject}`, c] as const),
        ).values(),
      ],
    };
  });

export async function allLocalRepos() {
  const settled = await Promise.allSettled(PROJECTS.filter((p) => p.dir).map((p) => localRepo(p.id)));
  return settled.flatMap((s) => (s.status === "fulfilled" ? [s.value] : []));
}

/** Reads a file of a project's repository (app version, Cargo.toml…). */
export async function readProjectFile(id: ProjectId, rel: string) {
  return readFile(path.join(dirOf(id), rel), "utf8");
}

const TOML_VERSION = /^version\s*=\s*"([^"]+)"/m;

/**
 * The project's version: from `version` in zenith.config.json ({ file, json } or
 * { file, pattern }), else package.json, Cargo.toml or pyproject.toml, whichever exists.
 */
export async function projectVersion(id: ProjectId): Promise<string | null> {
  const p = PROJECTS.find((x) => x.id === id);
  if (!p?.dir) return null;
  const fromJson = (raw: string, key: string) => key.split(".").reduce<unknown>((o, k) => (o as Record<string, unknown> | undefined)?.[k], JSON.parse(raw));
  try {
    if (p.version) {
      const raw = await readProjectFile(id, p.version.file);
      if (p.version.json) return String(fromJson(raw, p.version.json) ?? "") || null;
      return new RegExp(p.version.pattern ?? TOML_VERSION.source, "m").exec(raw)?.[1] ?? null;
    }
  } catch {
    return null;
  }
  for (const [file, read] of [
    ["package.json", (raw: string) => fromJson(raw, "version")],
    ["Cargo.toml", (raw: string) => TOML_VERSION.exec(raw)?.[1]],
    ["pyproject.toml", (raw: string) => TOML_VERSION.exec(raw)?.[1]],
  ] as const) {
    try {
      const v = read(await readProjectFile(id, file));
      if (v) return String(v);
    } catch {}
  }
  return null;
}
