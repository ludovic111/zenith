import { Suspense } from "react";
import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { config } from "@/lib/config";
import { PROJECTS, findProject, projectDir, type Project } from "@/lib/projects";
import { source } from "@/lib/source";
import { plural, tr } from "@/lib/i18n";
import { ago, base, date } from "@/lib/format";
import { downloads } from "@/lib/integrations";
import { uptime } from "@/lib/sources/uptime";
import { releases } from "@/lib/sources/github";
import * as rc from "@/lib/sources/revenuecat";
import { ProjectHeader } from "@/components/blocks/project-header";
import { UptimePanel, DeployPanel, TrafficPanel } from "@/components/blocks/health";
import { CodePanel } from "@/components/blocks/code";
import { AgentsPanel } from "@/components/blocks/agents";
import { CodeThreadsPanel } from "@/components/code/threads-panel";
import { NotesPanel } from "@/components/blocks/notes";
import { SiteKpis } from "@/components/blocks/site-kpis";
import { ReleaseNotesButton, ReleaseRows } from "@/components/blocks/releases";
import { AppStorePanel } from "@/components/blocks/app-store";
import { IdentityCard } from "@/components/identity/identity-card";
import { Chip, Empty, Panel, SectionTitle, Skeleton } from "@/components/z/panel";
import { Gate } from "@/components/z/gate";
import { Stat } from "@/components/z/stat";
import { Bars } from "@/components/charts/bars";
import { cn } from "@/lib/utils";

export const dynamic = "force-dynamic";

type Props = { params: Promise<{ id: string }> };

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const p = findProject((await params).id);
  return p ? { title: p.name, description: p.tagline || undefined } : {};
}

/**
 * The page of any project listed in zenith.config.json. Every panel shows only when its
 * source is configured: probes, `railway`, `repo`/`dir`, `releases`, `appStore`, `revenuecat`…
 */
export default async function ProjectPage({ params }: Props) {
  const p = findProject((await params).id);
  if (!p) notFound();

  const probes = (await uptime()).filter((u) => u.project === p.id);
  const hasAbout = p.highlights.length > 0 || !!p.about || !!p.shot;
  const dir = config().code.enabled ? projectDir(p) : null;

  return (
    <>
      <ProjectHeader project={p}>
        <Suspense fallback={<Skeleton className="h-[86px]" />}>
          <SiteKpis project={p} />
        </Suspense>
      </ProjectHeader>

      {p.showcase && <Showcase project={p} />}

      {(p.railway || hasAbout) && (
        <>
          <SectionTitle>{tr("Aperçu", "Overview")}</SectionTitle>
          <div className={cn("grid gap-4", p.railway && hasAbout && "xl:grid-cols-3")}>
            {p.railway && (
              <div className={cn("min-w-0", hasAbout && "xl:col-span-2")}>
                <Suspense fallback={<Skeleton className="h-72" />}>
                  <TrafficPanel project={p} title={p.trafficTitle} paths className="h-full" />
                </Suspense>
              </div>
            )}
            {hasAbout && <About project={p} />}
          </div>
        </>
      )}

      {(p.revenuecat || p.appStore) && (
        <>
          <SectionTitle>{tr("Argent & avis", "Money & reviews")}</SectionTitle>
          <div className={cn("grid gap-4", p.revenuecat && p.appStore && "xl:grid-cols-[1fr_1.4fr]")}>
            {p.revenuecat && (
              <Suspense fallback={<Skeleton className="h-72" />}>
                <Revenue project={p} projectId={p.revenuecat.projectId} />
              </Suspense>
            )}
            {p.appStore && (
              <Suspense fallback={<Skeleton className="h-72" />}>
                <AppStorePanel project={p} />
              </Suspense>
            )}
          </div>
        </>
      )}

      {(probes.length > 0 || p.railway) && (
        <>
          <SectionTitle>{tr("Santé", "Health")}</SectionTitle>
          <div className={cn("grid gap-4", probes.length > 0 && p.railway && "xl:grid-cols-2")}>
            {probes.length > 0 && (
              <Suspense fallback={<Skeleton className="h-64" />}>
                <UptimePanel project={p} />
              </Suspense>
            )}
            {p.railway && (
              <Suspense fallback={<Skeleton className="h-64" />}>
                <DeployPanel project={p} paths={false} />
              </Suspense>
            )}
          </div>
        </>
      )}

      {(p.repo || p.dir) && (
        <>
          <SectionTitle>{tr("Code", "Code")}</SectionTitle>
          <div className="space-y-4">
            <Suspense fallback={<Skeleton className="h-80" />}>
              <CodePanel project={p} />
            </Suspense>
            {p.releases && p.repo && (
              <Suspense fallback={<Skeleton className="h-72" />}>
                <Releases project={p} repo={p.repo} />
              </Suspense>
            )}
            {dir && <CodeThreadsPanel dir={dir} />}
          </div>
        </>
      )}

      <SectionTitle>{tr("Agents & notes", "Agents & notes")}</SectionTitle>
      <div className="space-y-4">
        <Suspense fallback={<Skeleton className="h-48" />}>
          <AgentsPanel project={p} />
        </Suspense>
        <Suspense fallback={null}>
          <NotesPanel project={p} />
        </Suspense>
      </div>

      <SectionTitle>{tr("Identité", "Identity")}</SectionTitle>
      <Suspense fallback={<Skeleton className="h-80" />}>
        <IdentityCard project={p} full={false} />
      </Suspense>
    </>
  );
}

/** Highlights, a sentence and the screenshot. */
function About({ project: p }: { project: Project }) {
  return (
    <Panel title={tr("En bref", "At a glance")}>
      {p.shot && (
        // eslint-disable-next-line @next/next/no-img-element
        <img src={`/api/shot/${p.id}`} alt={tr(`Capture de ${p.name}`, `Screenshot of ${p.name}`)} className="mb-4 w-full rounded-lg border border-line" />
      )}
      {p.highlights.length > 0 && (
        <div className="flex flex-wrap gap-1.5">
          {p.highlights.map((h, i) => (
            <Chip key={h} color={i < 2 ? p.color : undefined}>
              {h}
            </Chip>
          ))}
        </div>
      )}
      {p.about && <p className={cn("text-[13px] leading-relaxed text-ink-2", p.highlights.length > 0 && "mt-3")}>{p.about}</p>}
    </Panel>
  );
}

/** Every project's screenshot, for a showcase site. */
function Showcase({ project: p }: { project: Project }) {
  const shots = PROJECTS.filter((x) => x.shot);
  if (!shots.length) return null;
  return (
    <>
      <SectionTitle>{tr(`Ce que ${p.name} montre`, `What ${p.name} shows`)}</SectionTitle>
      <div className="grid grid-cols-2 gap-4 md:grid-cols-3 xl:grid-cols-4">
        {shots.map((x) => (
          <a key={x.id} href={x.href} className="group overflow-hidden rounded-xl border border-line bg-surface transition-colors hover:bg-hover">
            {/* eslint-disable-next-line @next/next/no-img-element */}
            <img src={`/api/shot/${x.id}`} alt={tr(`Capture de ${x.name}`, `Screenshot of ${x.name}`)} className="aspect-[16/10] w-full border-b border-line object-cover object-top" />
            <div className="flex items-center gap-2 px-3 py-2 text-[13px] text-ink">
              <span className="size-2 shrink-0 rounded-full" style={{ background: x.color }} />
              <span className="truncate">{x.name}</span>
            </div>
          </a>
        ))}
      </div>
    </>
  );
}

/** GitHub releases: downloads per version and the latest releases. */
async function Releases({ project: p, repo }: { project: Project; repo: string }) {
  const rel = await source(() => releases(repo));
  const latest = rel.ok ? rel.data.find((r) => !r.prerelease)?.tag_name : undefined;
  return (
    <Panel title={tr("Sorties & téléchargements", "Releases & downloads")} action={<ReleaseNotesButton project={p} latest={latest} />}>
      <Gate src={rel}>
        {(all) => {
          const list = all.filter((r) => !r.prerelease);
          if (!list.length) return <Empty>{tr("Aucune version publiée sur GitHub pour l'instant.", "No release published on GitHub yet.")}</Empty>;
          const total = downloads(list);
          const recent = list.filter((r) => Date.now() - new Date(r.published_at).getTime() < 30 * 864e5).length;
          return (
            <div className="grid gap-x-8 gap-y-6 lg:grid-cols-[1.4fr_1fr]">
              <div className="min-w-0">
                <div className="mb-5 grid grid-cols-3 gap-6">
                  <Stat label={tr("Téléchargements", "Downloads")} value={total} color={p.color} hint={`${list.length} ${plural(list.length, ["version", "versions"], ["release", "releases"])}`} />
                  <Stat label={list[0].tag_name} value={downloads([list[0]])} hint={tr(`publiée ${ago(list[0].published_at)}`, `released ${ago(list[0].published_at)}`)} />
                  <Stat label={tr("Versions en 30 j", "Releases in 30 d")} value={recent} hint={tr("cadence de sortie", "release cadence")} />
                </div>
                <Bars data={list.slice(0, 12).reverse().map((r) => ({ label: r.tag_name, value: downloads([r]) }))} color={p.color} height={160} />
              </div>
              <ReleaseRows project={p} list={list} count={(r) => downloads([r])} />
            </div>
          );
        }}
      </Gate>
    </Panel>
  );
}

/** RevenueCat: MRR, subscriptions and revenue per week. */
async function Revenue({ project: p, projectId }: { project: Project; projectId: string }) {
  const [m, rev] = await Promise.all([source(() => rc.overview(projectId)), source(() => rc.chart(projectId, "revenue"))]);
  const o = m.ok ? m.data : null;
  const currency = config().currency;
  return (
    <Panel title={tr("Revenu par semaine", "Revenue per week")}>
      {o && (
        <div className="mb-5 grid grid-cols-3 gap-6">
          <Stat label="MRR" value={o.mrr?.value ?? null} format={{ style: "currency", currency, maximumFractionDigits: 2 }} color={p.color} hint={tr("mensuel récurrent", "monthly recurring")} />
          <Stat label={tr("Revenu 28 j", "Revenue 28 d")} value={o.revenue?.value ?? null} format={{ style: "currency", currency, maximumFractionDigits: 2 }} />
          <Stat
            label={tr("Abonnés", "Subscribers")}
            value={o.active_subscriptions?.value ?? null}
            hint={`${o.active_trials?.value ?? 0} ${plural(o.active_trials?.value ?? 0, ["essai en cours", "essais en cours"], ["active trial", "active trials"])}`}
          />
        </div>
      )}
      <Gate src={rev}>
        {(series) => {
          const total = series.reduce((a, b) => a + b.value, 0);
          return (
            <>
              <div className="mb-3 flex items-baseline gap-2">
                <span className="text-2xl font-semibold tracking-tight text-ink tabular">{base(total)}</span>
                <span className="text-xs text-ink-3">{tr("sur 16 semaines", "over 16 weeks")}</span>
              </div>
              <Bars data={series.map((s) => ({ label: date(s.t), value: s.value, incomplete: s.incomplete }))} color={p.color} unit="base" height={140} />
            </>
          );
        }}
      </Gate>
    </Panel>
  );
}
