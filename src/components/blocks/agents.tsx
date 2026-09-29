import Link from "next/link";
import type { Project } from "@/lib/projects";
import { source } from "@/lib/source";
import { tr } from "@/lib/i18n";
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
      kicker={tr("Agents IA", "AI agents")}
      title={tr("Qui bosse dessus", "Who's working on it")}
      accent={p.glow}
      action={
        <Link href="/agents" className="text-xs text-ink-3 hover:text-ink">
          {tr("Tout voir →", "See all →")}
        </Link>
      }
    >
      <Gate src={all}>
        {(list) => {
          const mine = list.filter((s) => s.project === p.id);
          const live = mine.filter(isLive).length;
          const cost = mine.filter((s) => s.end > Date.now() - 30 * 864e5).reduce((a, s) => a + (s.costUSD ?? 0), 0);
          return (
            <>
              <div className="mb-2 flex flex-wrap gap-x-5 gap-y-1 text-xs text-ink-3">
                <span><span className="font-mono text-ink">{mine.length}</span> sessions</span>
                <span><span className="font-mono text-ink">{live}</span> {tr("en cours", "live")}</span>
                {cost > 0 && (
                  <span>
                    <span className="font-mono text-ink">{tr(`${cost.toFixed(2)} $`, `$${cost.toFixed(2)}`)}</span> {tr("Claude sur 30 j", "Claude over 30 d")}
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
