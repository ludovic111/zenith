import "server-only";
import { execFile } from "node:child_process";
import { existsSync } from "node:fs";
import { readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { PROJECTS, projectDir } from "../projects";
import { BRAND } from "./brand";
import { CODE_BIN, CODE_ROOT, codeHome } from "./paths";

/** Runs the zenith code CLI against our state directory (it works while the server runs). */
function cli(args: string[], timeoutMs = 20_000): Promise<{ code: number; stdout: string; stderr: string }> {
  return new Promise((resolve) => {
    execFile(
      process.execPath,
      [CODE_BIN, ...args, "--base-dir", codeHome()],
      { cwd: CODE_ROOT, timeout: timeoutMs, maxBuffer: 4 * 1024 * 1024, env: { ...process.env, NODE_ENV: "production" } },
      (error, stdout, stderr) => {
        const code = error ? (typeof error.code === "number" ? error.code : 1) : 0;
        resolve({ code, stdout, stderr });
      },
    );
  });
}

/** A fresh one-time pairing token for the embedded client (owner scopes, 2 minutes). */
export async function mintPairingToken(): Promise<string> {
  const { code, stdout, stderr } = await cli(["auth", "pairing", "create", "--ttl", "2m", "--admin", "--label", BRAND, "--json"]);
  if (code !== 0) throw new Error(stderr.trim().split("\n").at(-1) || `exit ${code}`);
  const credential = (JSON.parse(stdout) as { credential?: unknown }).credential;
  if (typeof credential !== "string" || !credential) throw new Error("no credential in CLI output");
  return credential;
}

const REGISTERED = () => path.join(codeHome(), "zenith-projects.json");

/**
 * Adds the dashboard itself and every configured project folder as zenith code projects.
 * Each folder is added once: projects the user later removes in zenith code stay removed.
 */
export async function registerProjects(log: (message: string) => void = () => {}) {
  const wanted = [
    { name: BRAND, dir: process.cwd() },
    ...PROJECTS.map((p) => ({ name: p.name, dir: projectDir(p) })).filter((p): p is { name: string; dir: string } => p.dir !== null),
  ];
  const done = new Set<string>(await readFile(REGISTERED(), "utf8").then((raw) => JSON.parse(raw) as string[]).catch(() => []));
  let changed = false;
  for (const { name, dir } of wanted) {
    if (done.has(dir) || !existsSync(dir)) continue;
    const { code, stdout, stderr } = await cli(["project", "add", dir, "--title", name]);
    if (code === 0 || /already exists/i.test(stderr + stdout)) {
      done.add(dir);
      changed = true;
      log(`project ${code === 0 ? "added" : "already there"}: ${name}`);
    } else {
      const reason = (stderr + stdout).split("\n").find((l) => /error/i.test(l))?.trim();
      log(`project add failed for ${name}: ${reason ?? `exit ${code}`}`);
    }
  }
  if (changed) await writeFile(REGISTERED(), JSON.stringify([...done], null, 2));
}
