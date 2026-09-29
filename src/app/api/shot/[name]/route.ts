import { readFile } from "node:fs/promises";
import path from "node:path";
import { PROJECTS, PROJECTS_ROOT, findProject, projectDir } from "@/lib/projects";

const TYPES: Record<string, string> = {
  ".webp": "image/webp",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".jpeg": "image/jpeg",
  ".gif": "image/gif",
  ".avif": "image/avif",
};

/** Where a project's `shot` may live: under projectsRoot, else (bare file name) in any project's shots/ folder. */
function candidates(shot: string) {
  const home = shot.replace(/^~(?=$|\/)/, process.env.HOME ?? "~");
  const out = [path.resolve(PROJECTS_ROOT, home)];
  if (!shot.includes("/"))
    for (const p of PROJECTS) {
      const dir = projectDir(p);
      if (dir) out.push(path.join(dir, "shots", shot));
    }
  return out;
}

/**
 * Serves a project's screenshot (`shot` in zenith.config.json), read fresh from disk.
 * The URL names the project (/api/shot/<id>), so only configured files can be served.
 */
export async function GET(_: Request, ctx: { params: Promise<{ name: string }> }) {
  const { name } = await ctx.params;
  const shot = findProject(name.replace(/\.[a-z]+$/i, ""))?.shot;
  const type = shot ? TYPES[path.extname(shot).toLowerCase()] : undefined;
  if (!shot || !type) return new Response("Not found", { status: 404 });
  for (const file of candidates(shot)) {
    try {
      return new Response(await readFile(file), { headers: { "Content-Type": type, "Cache-Control": "public, max-age=3600" } });
    } catch {}
  }
  return new Response("Not found", { status: 404 });
}
