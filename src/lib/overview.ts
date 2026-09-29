import "server-only";
import { PROJECTS } from "./projects";
import { source } from "./source";
import { tr } from "./i18n";
import { allLocalRepos } from "./sources/git";
import { deployments } from "./sources/railway";
import { AGENTS, sessions } from "./sources/agents";
import { collect, type Event } from "./extensions";

export type { Event } from "./extensions";

/** Everything that happened lately, all sources together, newest first. */
export async function events(limit = 24): Promise<Event[]> {
  const [repos, deps, agents, extra] = await Promise.all([
    allLocalRepos(),
    Promise.all(PROJECTS.filter((p) => p.railway).map((p) => source(() => deployments(p)).then((r) => ({ p, r })))),
    source(sessions),
    collect((e) => e.events),
  ]);
  const known = new Set(PROJECTS.map((p) => p.id));
  const out: Event[] = [...extra];
  if (agents.ok)
    for (const a of agents.data.slice(0, 12))
      if (a.project && known.has(a.project))
        out.push({ project: a.project, at: a.end, kind: "agent", text: `${AGENTS[a.agent].name} · ${a.title}`, href: a.prs.at(-1)?.url });
  for (const r of repos) {
    const repo = PROJECTS.find((p) => p.id === r.project)?.repo;
    for (const c of r.commits.slice(0, 10)) out.push({ project: r.project, at: c.at, kind: "commit", text: c.subject, href: repo ? `https://github.com/${repo}/commit/${c.hash}` : undefined });
  }
  for (const { p, r } of deps)
    if (r.ok)
      for (const d of r.data.slice(0, 3))
        out.push({
          project: p.id,
          at: new Date(d.createdAt).getTime(),
          kind: "deploy",
          text: `${d.status === "SUCCESS" ? tr("Déploiement réussi", "Deploy succeeded") : `${tr("Déploiement", "Deploy")} ${d.status.toLowerCase()}`}${d.meta?.commitMessage ? ` · ${d.meta.commitMessage.split("\n")[0]}` : ""}`,
        });
  return out
    .filter((e) => known.has(e.project) && Number.isFinite(e.at))
    .sort((a, b) => b.at - a.at)
    .slice(0, limit);
}

/** Commits per day and per project over 26 weeks (for the heatmap). */
export async function activity(days = 182) {
  const repos = await allLocalRepos();
  const start = new Date();
  start.setUTCHours(0, 0, 0, 0);
  const t0 = start.getTime() - (days - 1) * 864e5;
  const grid = Array.from({ length: days }, (_, i) => ({ t: t0 + i * 864e5, by: {} as Record<string, number> }));
  for (const r of repos)
    for (const c of r.commits) {
      const i = Math.floor((c.at - t0) / 864e5);
      if (i >= 0 && i < days) grid[i].by[r.project] = (grid[i].by[r.project] ?? 0) + 1;
    }
  return grid.map((g) => ({
    t: g.t,
    total: Object.values(g.by).reduce((a, b) => a + b, 0),
    parts: PROJECTS.map((p) => ({ name: p.name, color: p.color, value: g.by[p.id] ?? 0 })),
  }));
}
