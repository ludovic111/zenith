import Link from "next/link";
import type { Project } from "@/lib/projects";
import { source } from "@/lib/source";
import { plural, tr } from "@/lib/i18n";
import { sessions, isLive } from "@/lib/sources/agents";
import { Panel } from "@/components/z/panel";
import { Gate } from "@/components/z/gate";
import { SessionList } from "@/components/agents/session-list";
import { toRows } from "@/components/agents/rows";

/** The Claude Code and Codex sessions opened on this project. */
export async function AgentsPanel({ project: p }: { project: Project }) {
  const all = await source(sessions);
  return (
    <Panel
      title={tr("Sessions d'agents", "Agent sessions")}
      action={
        <Link href="/agents" className="hover:text-ink">
          {tr("Tout voir", "See all")}
        </Link>
      }
    >
      <Gate src={all}>
        {(list) => {
          const mine = list.filter((s) => s.project === p.id);
          if (!mine.length) return <p className="text-[13px] text-ink-3">{tr("Aucune session Claude Code ou Codex sur ce projet.", "No Claude Code or Codex session on this project.")}</p>;
          const live = mine.filter(isLive).length;
          const cost = mine.filter((s) => s.end > Date.now() - 30 * 864e5).reduce((a, s) => a + (s.costUSD ?? 0), 0);
          return (
            <>
              <div className="mb-2 flex flex-wrap gap-x-4 gap-y-1 text-xs text-ink-3">
                <span>
                  <span className="font-medium text-ink tabular">{mine.length}</span> {plural(mine.length, ["session", "sessions"], ["session", "sessions"])}
                </span>
                <span>
                  <span className="font-medium text-ink tabular">{live}</span> {tr("en cours", "live")}
                </span>
                {cost > 0 && (
                  <span>
                    <span className="font-medium text-ink tabular">{tr(`${cost.toFixed(2)} $`, `$${cost.toFixed(2)}`)}</span> {tr("de Claude sur 30 j", "of Claude over 30 d")}
                  </span>
                )}
              </div>
              <SessionList rows={toRows(mine.slice(0, 6))} compact />
            </>
          );
        }}
      </Gate>
    </Panel>
  );
}
