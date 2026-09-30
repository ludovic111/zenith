import type { Project } from "@/lib/projects";
import { source } from "@/lib/source";
import { ago } from "@/lib/format";
import { tr } from "@/lib/i18n";
import { uptime } from "@/lib/sources/uptime";
import { deployments, traffic } from "@/lib/sources/railway";
import { localRepo } from "@/lib/sources/git";
import { cn } from "@/lib/utils";
import { Stat } from "@/components/z/stat";

const COLS = ["", "xl:grid-cols-1", "xl:grid-cols-2", "xl:grid-cols-3", "xl:grid-cols-4", "xl:grid-cols-5", "xl:grid-cols-6"];
const MD = ["", "md:grid-cols-1", "md:grid-cols-2", "md:grid-cols-3"];

/** A project's key numbers, in one flat card. Up to six tiles per row. */
export function KpiStrip({ children, count = 6, className }: { children: React.ReactNode; count?: number; className?: string }) {
  return (
    <div className={cn("grid grid-cols-2 gap-x-6 gap-y-5 rounded-xl border border-line bg-surface px-5 py-4", MD[Math.min(3, count)], COLS[Math.min(6, count)], className)}>
      {children}
    </div>
  );
}

/**
 * A site's numbers: uptime and latency (probes), requests, visitors and last deploy (Railway),
 * commits (local repository). Only the tiles whose source is configured are shown.
 */
export async function SiteKpis({ project: p }: { project: Project }) {
  const [probes, http, deps, local] = await Promise.all([
    uptime(),
    p.railway ? source(() => traffic(p)) : null,
    p.railway ? source(() => deployments(p)) : null,
    p.dir ? source(() => localRepo(p.id)) : null,
  ]);
  const mine = probes.filter((u) => u.project === p.id);
  const ratios = mine.map((u) => u.ratio).filter((r): r is number => r != null);
  const ratio = mine.length && ratios.length === mine.length ? ratios.reduce((a, r) => a + r, 0) / ratios.length : null;
  const last = deps?.ok ? deps.data.find((d) => d.status === "SUCCESS") : undefined;
  const tiles: React.ReactNode[] = [];
  if (mine.length) {
    tiles.push(
      <Stat key="up" label={tr("Disponibilité", "Uptime")} value={ratio} format={{ style: "percent", maximumFractionDigits: 2 }} color={p.color} hint={tr(`${mine[0]?.samples.length ?? 0} mesures récentes`, `${mine[0]?.samples.length ?? 0} recent checks`)} />,
      <Stat key="ms" label={tr("Latence", "Latency")} value={mine[0]?.last?.ms ?? null} suffix=" ms" hint={mine[0]?.avg != null ? tr(`moyenne ${mine[0].avg} ms`, `average ${mine[0].avg} ms`) : undefined} />,
    );
  }
  if (http) {
    tiles.push(
      <Stat
        key="req"
        label={tr("Requêtes", "Requests")}
        value={http.ok && http.data ? http.data.requests : null}
        hint={http.ok ? (http.data ? tr(`depuis ${ago(http.data.since)}`, `since ${ago(http.data.since)}`) : tr("aucune", "none")) : tr("Railway à brancher", "Connect Railway")}
      />,
      <Stat key="ip" label={tr("IP uniques", "Unique IPs")} value={http.ok && http.data ? http.data.visitors : null} hint={tr("même fenêtre", "same window")} />,
    );
  }
  if (local) {
    tiles.push(
      <Stat
        key="commits"
        label={tr("Commits 30 j", "Commits 30 d")}
        value={local.ok ? local.data.commits.filter((c) => c.at > Date.now() - 30 * 864e5).length : null}
        hint={local.ok ? tr(`dernier ${ago(local.data.commits[0]?.at)}`, `last ${ago(local.data.commits[0]?.at)}`) : undefined}
      />,
    );
  }
  if (deps) {
    tiles.push(
      <Stat
        key="since"
        label={tr("En ligne depuis", "Live for")}
        value={last ? Math.max(0, Math.round((Date.now() - new Date(last.createdAt).getTime()) / 864e5)) : null}
        suffix={tr(" j", " d")}
        hint={tr("dernier déploiement réussi", "last successful deploy")}
      />,
    );
  }
  if (!tiles.length) return null;
  return <KpiStrip count={tiles.length}>{tiles}</KpiStrip>;
}
