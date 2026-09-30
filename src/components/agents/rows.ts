import "server-only";
import { PROJECTS } from "@/lib/projects";
import { isLive, type Session } from "@/lib/sources/agents";
import type { SessionRow } from "./session-list";

export function toRows(list: Session[]): SessionRow[] {
  return list.map((s) => {
    // zenith's own sessions get a neutral dot, unless it is listed as a project.
    const p = PROJECTS.find((x) => x.id === s.project) ?? (s.project === "zenith" ? { name: "zenith", color: "var(--ink-3)" } : undefined);
    return {
      agent: s.agent,
      id: s.id,
      title: s.title,
      projectName: p?.name ?? null,
      projectColor: p?.color ?? null,
      branch: s.branch,
      start: s.start,
      end: s.end,
      turns: s.turns,
      model: s.model,
      costUSD: s.costUSD,
      tokens: s.tokens,
      linesAdded: s.linesAdded,
      linesRemoved: s.linesRemoved,
      prs: s.prs,
      subagents: s.subagents,
      live: isLive(s),
      resume: s.resume,
    };
  });
}
