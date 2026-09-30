import "server-only";
import { existsSync } from "node:fs";
import { readdir, readFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { config } from "./config";

/**
 * What the welcome can guess, so a newcomer mostly says yes: the projects in their code
 * folder (git repositories, with their GitHub repository, description and site), and
 * which coding agents this Mac has (Claude Code, Codex).
 */

export type FoundProject = { id: string; name: string; dir: string; repo: string | null; site: string | null; tagline: string };

const slug = (s: string) =>
  s
    .toLowerCase()
    .normalize("NFD")
    .replace(/[̀-ͯ]/g, "")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 32) || "projet";

const pretty = (folder: string) => folder.replace(/[-_]+/g, " ").replace(/\b\w/g, (c) => c.toUpperCase());

/** "owner/name" of the GitHub remote, if any. */
async function githubRepo(dir: string): Promise<string | null> {
  const cfg = await readFile(path.join(dir, ".git", "config"), "utf8").catch(() => "");
  const m = /url\s*=\s*(?:https:\/\/github\.com\/|git@github\.com:)([\w.-]+\/[\w.-]+?)(?:\.git)?\s*$/m.exec(cfg);
  return m ? m[1] : null;
}

async function describe(dir: string): Promise<{ tagline: string; site: string | null; name: string | null }> {
  try {
    const pkg = JSON.parse(await readFile(path.join(dir, "package.json"), "utf8")) as { description?: string; homepage?: string; name?: string; displayName?: string };
    if (pkg.description || pkg.homepage)
      return { tagline: (pkg.description ?? "").slice(0, 90), site: pkg.homepage?.startsWith("http") ? pkg.homepage : null, name: pkg.displayName ?? null };
  } catch {}
  const readme = await readFile(path.join(dir, "README.md"), "utf8").catch(() => "");
  const title = /^#\s+(.+)$/m.exec(readme)?.[1]?.replace(/[*_`[\]]/g, "").trim() ?? null;
  const line = readme
    .split("\n")
    .map((l) => l.trim())
    .find((l) => l && !l.startsWith("#") && !l.startsWith("!") && !l.startsWith("<") && !l.startsWith("[") && !l.startsWith(">") && l.length > 12);
  return { tagline: (line ?? "").replace(/[*_`]/g, "").slice(0, 90), site: null, name: title && title.length <= 40 ? title : null };
}

/** Git repositories directly under `root` (zenith itself aside). */
export async function scanProjects(root: string): Promise<FoundProject[]> {
  const entries = await readdir(root, { withFileTypes: true }).catch(() => []);
  const seen = new Set<string>();
  const out: FoundProject[] = [];
  for (const e of entries) {
    if (!e.isDirectory() || e.name.startsWith(".") || e.name === "node_modules") continue;
    const dir = path.join(root, e.name);
    // zenith itself (this checkout, or another clone of it) isn't one of your projects.
    if (!existsSync(path.join(dir, ".git")) || path.resolve(dir) === process.cwd() || existsSync(path.join(dir, "scripts", "mcp", "zenith-mcp.mjs"))) continue;
    const [repo, d] = await Promise.all([githubRepo(dir), describe(dir)]);
    let id = slug(e.name);
    while (seen.has(id)) id = `${id}-2`;
    seen.add(id);
    out.push({ id, name: d.name ?? pretty(e.name), dir: e.name, repo, site: d.site, tagline: d.tagline });
  }
  return out.sort((a, b) => a.name.localeCompare(b.name));
}

/** Only folders in your home can be scanned. */
export function scanRoot(input?: string | null): string | null {
  const root = path.resolve((input || config().projectsRoot).replace(/^~(?=$|\/)/, os.homedir()));
  return root === os.homedir() || root.startsWith(os.homedir() + path.sep) ? root : null;
}

const onPath = (bin: string) => (process.env.PATH ?? "").split(path.delimiter).some((d) => d && existsSync(path.join(d, bin)));

/** Which agents can run here: Claude Code (the CLI) and Codex (its CLI, or the one inside ChatGPT.app). */
export function detectAgents() {
  const home = os.homedir();
  const claude = onPath("claude") || existsSync(path.join(home, ".local", "bin", "claude")) || existsSync(path.join(home, ".claude"));
  const codex = onPath("codex") || existsSync("/Applications/ChatGPT.app/Contents/Resources/codex-cli") || existsSync(path.join(home, ".codex"));
  return { claude, codex };
}

/** Currency most likely for a locale's region. */
export function currencyFor(locale: string): string {
  const region = locale.split("-")[1]?.toUpperCase();
  const map: Record<string, string> = { CH: "CHF", GB: "GBP", US: "USD", CA: "CAD", AU: "AUD", JP: "JPY", SE: "SEK", NO: "NOK", DK: "DKK", PL: "PLN" };
  if (region && map[region]) return map[region];
  if (region && ["FR", "DE", "ES", "IT", "BE", "NL", "PT", "AT", "IE", "FI", "LU", "GR"].includes(region)) return "EUR";
  return "USD";
}

/** First run: no config yet, or one without you or your projects in it. */
export function needsWelcome(): boolean {
  const c = config();
  return !c.meta.found || (!c.owner.name && !c.projects.length);
}
