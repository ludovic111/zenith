import type { Project } from "@/lib/projects";
import { uptime } from "@/lib/sources/uptime";
import { deployments, traffic } from "@/lib/sources/railway";
import { source } from "@/lib/source";
import { ago, compact, dateTime, nf } from "@/lib/format";
import { l10n, tr } from "@/lib/i18n";
import { Panel } from "@/components/z/panel";
import { Gate } from "@/components/z/gate";
import { Status, deployHealth, deployLabel } from "@/components/z/status";
import { UptimeStrip } from "@/components/charts/uptime-strip";
import { Line } from "@/components/charts/line";
import { HBars } from "@/components/charts/hbars";
import { Bars } from "@/components/charts/bars";

/** Uptime measured from this computer, once a minute. Hidden when the project has no probes. */
export async function UptimePanel({ project: p }: { project: Project }) {
  const probes = (await uptime()).filter((u) => u.project === p.id);
  if (!probes.length) return null;
  return (
    <Panel kicker={tr("Disponibilité", "Uptime")} title={tr("Ça répond ?", "Is it up?")} accent={p.glow}>
      <div className="space-y-6">
        {probes.map((u) => (
          <div key={u.url}>
            <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
              <div className="flex items-center gap-3">
                <Status health={u.up == null ? "unknown" : u.up ? "up" : "down"} />
                <span className="font-mono text-xs text-ink-2">{u.label}</span>
              </div>
              <div className="flex gap-4 font-mono text-xs text-ink-3">
                <span>
                  <span className="text-ink">{u.last?.ms ?? "—"}</span> ms
                </span>
                <span>
                  {tr("moy.", "avg")} <span className="text-ink">{u.avg ?? "—"}</span> ms
                </span>
                <span>
                  <span className="text-ink">{u.ratio == null ? "—" : nf(u.ratio * 100, 1)}</span>{tr(" %", "%")}
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

/** Railway deployments and HTTP traffic. Hidden without a `railway` block. */
export async function DeployPanel({ project: p }: { project: Project }) {
  if (!p.railway) return null;
  const [deps, http] = await Promise.all([source(() => deployments(p)), source(() => traffic(p))]);
  return (
    <Panel kicker="Railway" title={tr("Déploiements & trafic", "Deploys & traffic")} accent={p.glow}>
      <Gate src={deps}>
        {(list) => (
          <ol className="space-y-3">
            {list.slice(0, 5).map((d, i) => (
              <li key={d.id} className="flex items-start gap-3">
                <div className="flex flex-col items-center pt-1">
                  <span className="size-2 rounded-full" style={{ background: i === 0 ? p.glow : "rgb(255 255 255 / .2)" }} />
                  {i < Math.min(4, list.length - 1) && <span className="mt-1 h-7 w-px bg-white/10" />}
                </div>
                <div className="min-w-0 flex-1">
                  <div className="flex items-center justify-between gap-3">
                    <Status health={deployHealth(d.status)} label={deployLabel(d.status)} />
                    <span className="shrink-0 text-xs text-ink-3" title={dateTime(d.createdAt)}>{ago(d.createdAt)}</span>
                  </div>
                  <div className="truncate text-sm text-ink-2">{d.meta?.commitMessage?.split("\n")[0] ?? tr("Déploiement manuel", "Manual deploy")}</div>
                </div>
              </li>
            ))}
          </ol>
        )}
      </Gate>
      {http.ok && http.data && (
        <div className="mt-6 border-t border-line pt-5">
          <div className="mb-4 grid grid-cols-3 gap-3">
            <Mini label={tr("Requêtes", "Requests")} value={compact(http.data.requests)} />
            <Mini label={tr("IP uniques", "Unique IPs")} value={compact(http.data.visitors)} />
            <Mini label={tr("Médiane", "Median")} value={http.data.p50 == null ? "—" : `${http.data.p50} ms`} />
          </div>
          <p className="mb-3 text-xs text-ink-3">
            {tr(
              `${nf(http.data.requests)} dernières requêtes du déploiement actif, depuis ${ago(http.data.since)}.`,
              `Last ${nf(http.data.requests)} requests of the live deployment, since ${ago(http.data.since)}.`,
            )}
            {http.data.codes.server > 0 && <span className="text-bad">{tr(` ${http.data.codes.server} erreur(s) 5xx.`, ` ${http.data.codes.server} 5xx error(s).`)}</span>}
          </p>
          <HBars rows={http.data.topPaths.slice(0, 6).map((r) => ({ label: <code className="font-mono text-xs">{r.path}</code>, value: r.count, key: r.path }))} color={p.color} />
        </div>
      )}
      {!http.ok && http.error && <p className="mt-4 text-xs text-ink-3">{tr("Trafic indisponible : ", "Traffic unavailable: ")}{http.error}</p>}
    </Panel>
  );
}

function Mini({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-2xl border border-line bg-black/20 px-3 py-2.5">
      <div className="text-[10px] uppercase tracking-[0.16em] text-ink-3">{label}</div>
      <div className="mt-0.5 font-display text-lg tabular">{value}</div>
    </div>
  );
}

/** Requests per hour, rebuilt from the Railway HTTP log of the live deployment. Hidden without a `railway` block. */
export async function TrafficPanel({ project: p, title }: { project: Project; title?: string }) {
  if (!p.railway) return null;
  const http = await source(() => traffic(p));
  return (
    <Panel kicker={tr("Visiteurs", "Visitors")} title={title ?? tr("Trafic par heure", "Traffic per hour")} accent={p.glow}>
      <Gate src={http}>
        {(t) => {
          if (!t || !t.times.length) return <p className="text-sm text-ink-3">{tr("Aucune requête dans le journal du déploiement actif.", "No requests in the live deployment's log.")}</p>;
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
              <div className="mb-4 flex flex-wrap items-baseline gap-x-6 gap-y-2">
                <span><span className="font-display text-3xl tabular">{compact(t.requests)}</span> <span className="text-sm text-ink-3">{tr("requêtes", "requests")}</span></span>
                <span><span className="font-display text-3xl tabular">{compact(t.visitors)}</span> <span className="text-sm text-ink-3">{tr("IP uniques", "unique IPs")}</span></span>
              </div>
              <Bars data={buckets.map((b, i) => ({ label: fmt.format(b.t), value: b.v, incomplete: i === n - 1 }))} color={p.color} height={140} />
            </>
          );
        }}
      </Gate>
    </Panel>
  );
}
