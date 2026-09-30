import "server-only";
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { appendFile, mkdir, readFile, rename, rm } from "node:fs/promises";
import path from "node:path";
import { config } from "./config";
import { writeJson } from "./agent/files";

/**
 * zenith updates itself from GitHub. Every six hours (and ten minutes after it starts)
 * it fetches its branch's upstream; when there is something new and nothing of yours
 * could be lost — no uncommitted change to a tracked file, a fast-forward only — it:
 *
 * 1. fast-forwards the checkout and reinstalls dependencies if the lockfile changed;
 * 2. builds the new version next to the running one (.next-update), so the app keeps
 *    working meanwhile; a failed build puts the checkout back where it was;
 * 3. waits until no agent is working in zenith code (a restart would cut them off);
 * 4. swaps the builds (the old one stays in .next-previous), rebuilds zenith code if it
 *    changed, and exits: launchd starts the new version.
 *
 * Only the installed app (a launchd job, `updates.auto`) does it on its own; anywhere
 * else, Settings shows what's new and does it on demand, and asks you to restart.
 */

const ROOT = process.cwd();
const FILE = path.join(ROOT, ".data", "update.json");
const LOG = path.join(ROOT, ".data", "update.log");
const CHECK_EVERY = 6 * 3600e3;
const IDLE_POLL = 2 * 60e3;

export type UpdateState = {
  /** idle: up to date · available: something new · updating: building · waiting: for agents to finish · restart: ready, restart zenith · error */
  state: "idle" | "available" | "updating" | "waiting" | "restart" | "error";
  step?: string;
  checkedAt?: string;
  current?: { sha: string; date: string; subject: string };
  upstream?: string;
  behind?: number;
  incoming?: { sha: string; subject: string }[];
  blocked?: string | null;
  error?: string | null;
  updatedAt?: string;
};

const g = globalThis as { __zenithUpdate?: { busy: boolean; timer?: NodeJS.Timeout } };
const mem = (g.__zenithUpdate ??= { busy: false });

/**
 * Whether exiting brings zenith back: the installed app, launched by launchd (parent pid 1)
 * which relaunches it, or any supervisor that says so (ZENITH_SUPERVISED=1: pm2, systemd…).
 */
export const selfRestarts = () => process.env.NODE_ENV === "production" && (process.ppid === 1 || process.env.ZENITH_SUPERVISED === "1");

function run(cmd: string, args: string[], env: Record<string, string> = {}): Promise<{ code: number; out: string }> {
  return new Promise((resolve) => {
    const child = spawn(cmd, args, { cwd: ROOT, env: { ...process.env, ...env }, stdio: ["ignore", "pipe", "pipe"] });
    let out = "";
    const take = (d: Buffer) => {
      out += d.toString();
      if (out.length > 200_000) out = out.slice(-100_000);
    };
    child.stdout.on("data", take);
    child.stderr.on("data", take);
    child.on("error", (e) => resolve({ code: 1, out: String(e) }));
    child.on("close", (code) => resolve({ code: code ?? 1, out }));
  });
}

const git = async (...args: string[]) => {
  const r = await run("git", args);
  if (r.code) throw new Error(`git ${args[0]}: ${r.out.trim().split("\n").at(-1)}`);
  return r.out.trim();
};

async function log(line: string) {
  await mkdir(path.dirname(LOG), { recursive: true });
  await appendFile(LOG, `[${new Date().toISOString()}] ${line}\n`).catch(() => {});
}

export async function updateState(): Promise<UpdateState> {
  try {
    return JSON.parse(await readFile(FILE, "utf8")) as UpdateState;
  } catch {
    return { state: "idle" };
  }
}

async function save(patch: Partial<UpdateState>): Promise<UpdateState> {
  const next = { ...(await updateState()), ...patch, updatedAt: new Date().toISOString() };
  await writeJson(FILE, next);
  return next;
}

async function current() {
  const [sha, date, subject] = (await git("log", "-1", "--format=%h%n%cI%n%s")).split("\n");
  return { sha, date, subject };
}

/** Why an update can't be applied here, or null. */
async function blocker(): Promise<string | null> {
  if (!existsSync(path.join(ROOT, ".git"))) return "not a git checkout";
  const dirty = await git("status", "--porcelain", "--untracked-files=no");
  if (dirty) return `local changes: ${dirty.split("\n").length} file(s)`;
  const upstream = await git("rev-parse", "--abbrev-ref", "@{u}").catch(() => "");
  if (!upstream) return "no upstream branch";
  const ff = await run("git", ["merge-base", "--is-ancestor", "HEAD", "@{u}"]);
  if (ff.code) return "the local branch has commits that aren't upstream";
  return null;
}

/** Fetches and tells what is new. */
export async function checkUpdate(): Promise<UpdateState> {
  if (!existsSync(path.join(ROOT, ".git"))) return save({ state: "idle", blocked: "not a git checkout", checkedAt: new Date().toISOString() });
  try {
    await git("fetch", "--quiet");
    const upstream = await git("rev-parse", "--abbrev-ref", "@{u}").catch(() => "");
    const behind = upstream ? Number(await git("rev-list", "--count", "HEAD..@{u}")) : 0;
    const incoming = behind
      ? (await git("log", "--format=%h%x09%s", "-30", "HEAD..@{u}")).split("\n").filter(Boolean).map((l) => ({ sha: l.split("\t")[0], subject: l.split("\t").slice(1).join("\t") }))
      : [];
    const prev = await updateState();
    const keep = prev.state === "updating" || prev.state === "waiting" || prev.state === "restart";
    return save({
      state: keep ? prev.state : behind ? "available" : "idle",
      checkedAt: new Date().toISOString(),
      current: await current(),
      upstream,
      behind,
      incoming,
      blocked: await blocker(),
      error: keep ? prev.error : null,
    });
  } catch (e) {
    return save({ checkedAt: new Date().toISOString(), error: e instanceof Error ? e.message : String(e) });
  }
}

/** Is any agent working in zenith code right now? */
async function agentsWorking(): Promise<boolean> {
  try {
    const { codeStatus } = await import("./code/manager");
    if (!codeStatus().running) return false;
    const { shell } = await import("./code/api");
    return (await shell()).threads.some((t) => !t.archivedAt && (t.latestTurn?.state === "running" || t.hasPendingApprovals || t.hasPendingUserInput));
  } catch {
    return false;
  }
}

/** Pulls, builds next to the running version, then restarts when the agents are done. */
export async function applyUpdate(): Promise<UpdateState> {
  if (mem.busy) return updateState();
  mem.busy = true;
  const from = (await git("rev-parse", "HEAD").catch(() => "")) || null;
  try {
    const why = await blocker();
    if (why) return save({ state: "error", blocked: why, error: why });
    await save({ state: "updating", step: "pull", error: null });
    await log(`update from ${from}`);
    const changed = (await git("diff", "--name-only", "HEAD", "@{u}")).split("\n").filter(Boolean);
    if (!changed.length) return save({ state: "idle", step: undefined, behind: 0, incoming: [] });
    await git("merge", "--ff-only", "--quiet", "@{u}");

    if (changed.some((f) => f === "package.json" || f === "package-lock.json")) {
      await save({ step: "install" });
      const r = await run("npm", ["install", "--no-audit", "--no-fund"]);
      await log(`npm install → ${r.code}\n${r.out.slice(-2000)}`);
      if (r.code) throw new Error("npm install failed");
    }

    await save({ step: "build" });
    await rm(path.join(ROOT, ".next-update"), { recursive: true, force: true });
    const b = await run(process.execPath, [path.join(ROOT, "node_modules", "next", "dist", "bin", "next"), "build"], { ZENITH_DIST_DIR: ".next-update", NODE_ENV: "production" });
    await log(`next build → ${b.code}\n${b.out.slice(-4000)}`);
    // Next may note the build folder in tsconfig.json: never leave the checkout dirty.
    await run("git", ["checkout", "--", "tsconfig.json", "next-env.d.ts"]);
    if (b.code) throw new Error("the build failed (see .data/update.log)");

    const code = changed.some((f) => f.startsWith("code/"));
    await save({ state: selfRestarts() ? "waiting" : "restart", step: code ? "code" : undefined, current: await current(), behind: 0, incoming: [] });
    if (selfRestarts()) void finishWhenIdle(code);
    return updateState();
  } catch (e) {
    const error = e instanceof Error ? e.message : String(e);
    await log(`failed: ${error}`);
    // Put the checkout back where it was: the running version keeps matching its sources.
    if (from) await run("git", ["reset", "--keep", from]);
    await rm(path.join(ROOT, ".next-update"), { recursive: true, force: true });
    return save({ state: "error", step: undefined, error, current: await current().catch(() => undefined) });
  } finally {
    mem.busy = false;
  }
}

/** Swaps the builds and exits once no agent is working; launchd starts the new version. */
async function finishWhenIdle(code: boolean) {
  while (await agentsWorking()) {
    await save({ state: "waiting" });
    await new Promise((r) => setTimeout(r, IDLE_POLL));
  }
  await save({ state: "updating", step: code ? "code" : "restart" });
  if (code && existsSync(path.join(ROOT, "code", "package.json"))) {
    const r = await run("npm", ["run", "code:build"]);
    await log(`code:build → ${r.code}\n${r.out.slice(-3000)}`);
  }
  const next = path.join(ROOT, ".next");
  const staged = path.join(ROOT, ".next-update");
  if (!existsSync(staged)) return save({ state: "error", error: "the new build is missing" });
  await rm(path.join(ROOT, ".next-previous"), { recursive: true, force: true });
  if (existsSync(next)) await rename(next, path.join(ROOT, ".next-previous"));
  await rename(staged, next);
  await save({ state: "idle", step: undefined });
  await log("restarting on the new version");
  process.exit(0);
}

/** A check at startup and every six hours; the installed app applies what it finds when `updates.auto`. */
export function startUpdater() {
  if (mem.timer) return;
  const tick = async () => {
    const s = await checkUpdate();
    if (s.state === "available" && !s.blocked && config().updates.auto && selfRestarts()) await applyUpdate();
  };
  // An update that was waiting for the agents when zenith stopped: it restarted on its own since.
  void updateState().then((s) => (s.state === "waiting" || s.state === "updating" ? save({ state: "idle", step: undefined }) : null));
  setTimeout(() => void tick(), 10 * 60e3);
  mem.timer = setInterval(() => void tick(), CHECK_EVERY);
}
