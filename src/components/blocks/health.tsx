import { ArrowUpRight } from "lucide-react";
import type { Project } from "@/lib/projects";
import { uptime } from "@/lib/sources/uptime";
import { deployments, traffic } from "@/lib/sources/railway";
import { source } from "@/lib/source";
import { ago, compact, dateTime, nf } from "@/lib/format";
import { l10n, plural, tr } from "@/lib/i18n";
import { agentUi } from "@/lib/agent/ui";
import { Panel } from "@/components/z/panel";
import { Gate } from "@/components/z/gate";
import { Status, deployHealth, deployLabel } from "@/components/z/status";
import { AskButton } from "@/components/agent/ask-button";
import { UptimeStrip } from "@/components/charts/uptime-strip";
import { Line } from "@/components/charts/line";
import { HBars } from "@/components/charts/hbars";
import { Bars } from "@/components/charts/bars";

const canAsk = (p: Project) => {
  const ui = agentUi();
  return ui.enabled && ui.targets.some((t) => t.id === p.id);
};

const railwayUrl = (p: Project) => (p.railway ? `https://railway.com/project/${p.railway.projectId}/service/${p.railway.serviceId}` : null);

/** Uptime measured from this computer, once a minute. Hidden when the project has no probes. */
export async function UptimePanel({ project: p }: { project: Project }) {
  const probes = (await uptime()).filter((u) => u.project === p.id);
  if (!probes.length) return null;
  return (
    <Panel title={tr("Disponibilité", "Uptime")} action={tr("mesurée chaque minute", "checked every minute")}>
      <div className="space-y-5">
        {probes.map((u) => (
          <div key={u.url}>
            <div className="mb-2 flex flex-wrap items-center justify-between gap-x-3 gap-y-1">
              <div className="flex min-w-0 items-center gap-2">
                <Status health={u.up == null ? "unknown" : u.up ? "up" : "down"} label={u.label} />
              </div>
              <div className="flex gap-3 text-xs text-ink-3 tabular">
                <span>
                  <span className="font-medium text-ink">{u.last?.ms ?? "—"}</span> ms
                </span>
                <span>
                  {tr("moy.", "avg")} <span className="font-medium text-ink">{u.avg ?? "—"}</span> ms
                </span>
                <span>
                  <span className="font-medium text-ink">{u.ratio == null ? "—" : nf(u.ratio * 100, 1)}</span>
                  {tr(" %", "%")}
                </span>
              </div>
            </div>
            <UptimeStrip samples={u.samples} />
            <div className="mt-3">
              <Line points={u.samples.map((s) => ({ t: s.t, v: s.ms }))} color={p.color} height={56} axis />
            </div>
          </div>
        ))}
      </div>
    </Panel>
  );
}

/** Railway deployments and a summary of the live deployment's HTTP log. Hidden without a `railway` block. */
export async function DeployPanel({ project: p, paths = true }: { project: Project; paths?: boolean }) {
  if (!p.railway) return null;
  const [deps, http] = await Promise.all([source(() => deployments(p)), source(() => traffic(p))]);
  const url = railwayUrl(p);
  return (
    <Panel
      title={tr("Déploiements", "Deploys")}
      action={
        url && (
          <a href={url} target="_blank" rel="noopener noreferrer" className="inline-flex items-center gap-1 hover:text-ink">
            Railway <ArrowUpRight className="size-3" />
          </a>
        )
      }
      bodyClassName="px-0 pb-2 pt-1"
    >
      <Gate src={deps}>
        {(list) =>
          list.length ? (
            <ul>
              {list.slice(0, 5).map((d) => (
                <li key={d.id} className="flex h-10 items-center gap-3 px-4">
                  <span className="w-24 shrink-0">
                    <Status health={deployHealth(d.status)} label={deployLabel(d.status)} />
                  </span>
                  <span className="min-w-0 flex-1 truncate text-[13px] text-ink-2">{d.meta?.commitMessage?.split("\n")[0] ?? tr("Déploiement manuel", "Manual deploy")}</span>
                  <span className="shrink-0 text-xs text-ink-3" title={dateTime(d.createdAt)}>
                    {ago(d.createdAt)}
                  </span>
                </li>
              ))}
            </ul>
          ) : (
            <p className="px-4 py-2 text-[13px] text-ink-3">{tr("Aucun déploiement.", "No deploys.")}</p>
          )
        }
      </Gate>
      {paths && http.ok && http.data && (
        <div className="mx-4 mt-2 border-t border-line pb-2 pt-4">
          <div className="mb-3 flex items-baseline justify-between gap-3">
            <h3 className="text-xs font-medium text-ink-3">{tr("Pages les plus demandées", "Most requested paths")}</h3>
            <span className="text-xs text-ink-3 tabular">
              {tr(`médiane ${http.data.p50 ?? "—"} ms`, `median ${http.data.p50 ?? "—"} ms`)}
              {http.data.codes.server > 0 && (
                <span className="text-bad"> · {http.data.codes.server} {plural(http.data.codes.server, ["erreur 5xx", "erreurs 5xx"], ["5xx error", "5xx errors"])}</span>
              )}
            </span>
          </div>
          <HBars rows={http.data.topPaths.slice(0, 6).map((r) => ({ label: <code className="font-mono text-xs">{r.path}</code>, value: r.count, key: r.path }))} color={p.color} />
        </div>
      )}
      {paths && !http.ok && http.error && (
        <p className="px-4 pb-2 pt-3 text-xs text-ink-3">
          {tr("Trafic indisponible : ", "Traffic unavailable: ")}
          {http.error}
        </p>
      )}
    </Panel>
  );
}

/** Requests per hour, rebuilt from the Railway HTTP log of the live deployment, and the most requested paths
 * with `paths`. Hidden without a `railway` block. */
export async function TrafficPanel({ project: p, title, paths = false, className }: { project: Project; title?: string; paths?: boolean; className?: string }) {
  if (!p.railway) return null;
  const http = await source(() => traffic(p));
  const ask = canAsk(p) && http.ok && http.data;
  return (
    <Panel
      className={className}
      title={title ?? tr("Trafic par heure", "Traffic per hour")}
      action={
        ask && (
          <AskButton
            target={p.id}
            label={tr("Analyse le trafic", "Analyse the traffic")}
            prompt={tr(
              `Analyse le trafic récent de ${p.name} à partir des logs HTTP Railway du déploiement actif (chemins, codes, durées, IP, heures). Dis-moi d'où viennent les visites, ce qui est anormal (erreurs 5xx, robots, pics, pages lentes) et les 3 améliorations qui comptent. Ne modifie rien.`,
              `Analyse ${p.name}'s recent traffic from the live deployment's Railway HTTP logs (paths, status codes, durations, IPs, hours). Tell me where visits come from, what's abnormal (5xx errors, bots, spikes, slow pages) and the 3 improvements that matter. Don't change anything.`,
            )}
          />
        )
      }
    >
      <Gate src={http}>
        {(t) => {
          if (!t || !t.times.length) return <p className="text-[13px] text-ink-3">{tr("Aucune requête dans le journal du déploiement actif.", "No requests in the live deployment's log.")}</p>;
          const H = 3600e3;
          const end = Math.ceil(Date.now() / H) * H;
          const start = Math.max(Math.floor(t.since / H) * H, end - 48 * H);
          const n = Math.max(1, Math.round((end - start) / H));
          const buckets = Array.from({ length: n }, (_, i) => ({ t: start + i * H, v: 0 }));
          for (const x of t.times) {
            const i = Math.floor((x - start) / H);
            if (i >= 0 && i < n) buckets[i].v++;
          }
          const fmt = new Intl.DateTimeFormat(l10n().locale, { timeZone: l10n().timeZone, weekday: "short", hour: "2-digit" });
          return (
            <>
              <div className="mb-7 flex flex-wrap items-baseline gap-x-6 gap-y-1">
                <span className="text-xs text-ink-3">
                  <span className="text-2xl font-semibold tracking-tight text-ink tabular">{compact(t.requests)}</span> {tr("requêtes", "requests")}
                </span>
                <span className="text-xs text-ink-3">
                  <span className="text-2xl font-semibold tracking-tight text-ink tabular">{compact(t.visitors)}</span> {tr("IP uniques", "unique IPs")}
                </span>
                <span className="text-xs text-ink-3">{tr(`depuis ${ago(t.since)}`, `since ${ago(t.since)}`)}</span>
              </div>
              <Bars data={buckets.map((b, i) => ({ label: fmt.format(b.t), value: b.v, incomplete: i === n - 1 }))} color={p.color} height={140} />
              {paths && t.topPaths.length > 0 && (
                <div className="mt-5 border-t border-line pt-4">
                  <div className="mb-3 flex items-baseline justify-between gap-3">
                    <h3 className="text-xs font-medium text-ink-3">{tr("Pages les plus demandées", "Most requested paths")}</h3>
                    <span className="text-xs text-ink-3 tabular">
                      {tr(`médiane ${t.p50 ?? "—"} ms`, `median ${t.p50 ?? "—"} ms`)}
                      {t.codes.server > 0 && <span className="text-bad"> · {t.codes.server} {plural(t.codes.server, ["erreur 5xx", "erreurs 5xx"], ["5xx error", "5xx errors"])}</span>}
                    </span>
                  </div>
                  <HBars rows={t.topPaths.slice(0, 5).map((r) => ({ label: <code className="font-mono text-xs">{r.path}</code>, value: r.count, key: r.path }))} color={p.color} />
                </div>
              )}
            </>
          );
        }}
      </Gate>
    </Panel>
  );
}
