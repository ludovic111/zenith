import { Suspense } from "react";
import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { Download, SquareTerminal, Star, Tag } from "lucide-react";
import { config } from "@/lib/config";
import { PROJECTS, findProject, type Project } from "@/lib/projects";
import { source } from "@/lib/source";
import { plural, tr } from "@/lib/i18n";
import { ago, base, date, nf } from "@/lib/format";
import { downloads } from "@/lib/integrations";
import { uptime } from "@/lib/sources/uptime";
import { releases } from "@/lib/sources/github";
import { appReviews, appStore } from "@/lib/sources/domains";
import * as rc from "@/lib/sources/revenuecat";
import { ProjectHeader } from "@/components/blocks/project-header";
import { UptimePanel, DeployPanel, TrafficPanel } from "@/components/blocks/health";
import { CodePanel } from "@/components/blocks/code";
import { AgentsPanel } from "@/components/blocks/agents";
import { NotesPanel } from "@/components/blocks/notes";
import { SiteKpis } from "@/components/blocks/site-kpis";
import { IdentityCard } from "@/components/identity/identity-card";
import { Chip, Empty, Panel, Skeleton } from "@/components/z/panel";
import { Gate } from "@/components/z/gate";
import { Stat } from "@/components/z/stat";
import { Bars } from "@/components/charts/bars";
import { Marquee } from "@/components/ui/marquee";
import { BorderBeam } from "@/components/ui/border-beam";

export const dynamic = "force-dynamic";

type Props = { params: Promise<{ id: string }> };

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const p = findProject((await params).id);
  return p ? { title: p.name, description: p.tagline || undefined } : {};
}

const COLS = ["", "", "xl:grid-cols-2", "xl:grid-cols-3"];

/**
 * The page of any project listed in zenith.config.json. Every panel shows only when its
 * source is configured: probes, `railway`, `repo`/`dir`, `releases`, `appStore`, `revenuecat`…
 */
export default async function ProjectPage({ params }: Props) {
  const p = findProject((await params).id);
  if (!p) notFound();

  const probes = (await uptime()).filter((u) => u.project === p.id);
  const hasAbout = p.highlights.length > 0 || !!p.about || !!p.shot;
  const health = [probes.length > 0 && "uptime", !!p.railway && "deploy", !!(p.repo || p.dir) && "code"].filter((x): x is string => !!x);

  return (
    <>
      <ProjectHeader
        project={p}
        actions={
          config().code.enabled && (
            <Link
              href={`/code?project=${encodeURIComponent(p.id)}`}
              className="group inline-flex items-center gap-1.5 rounded-full border px-3.5 py-1.5 text-sm text-ink transition hover:brightness-125"
              style={{ borderColor: `${p.glow}66`, background: `${p.glow}1f` }}
            >
              <SquareTerminal className="size-3.5" style={{ color: p.glow }} />
              {tr("Coder dans zenith code", "Code in zenith code")}
            </Link>
          )
        }
      >
        <Suspense fallback={<Skeleton className="h-24" />}>
          <SiteKpis project={p} />
        </Suspense>
      </ProjectHeader>

      {p.showcase && <Showcase project={p} />}

      {(p.railway || hasAbout) && (
        <div className={`grid gap-5 ${p.showcase ? "mt-5" : ""} ${p.railway && hasAbout ? "xl:grid-cols-3" : ""}`}>
          {p.railway && (
            <Suspense fallback={<Skeleton className={`h-72 ${hasAbout ? "xl:col-span-2" : ""}`} />}>
              <div className={hasAbout ? "xl:col-span-2" : undefined}>
                <TrafficPanel project={p} title={p.trafficTitle} />
              </div>
            </Suspense>
          )}
          {hasAbout && <About project={p} />}
        </div>
      )}

      {(p.revenuecat || p.appStore) && (
        <div className={`mt-5 grid gap-5 ${p.revenuecat && p.appStore ? "xl:grid-cols-[1fr_1.4fr]" : ""}`}>
          {p.revenuecat && (
            <Suspense fallback={<Skeleton className="h-72" />}>
              <Revenue project={p} projectId={p.revenuecat.projectId} />
            </Suspense>
          )}
          {p.appStore && (
            <Suspense fallback={<Skeleton className="h-72" />}>
              <AppStore project={p} id={p.appStore.id} countries={p.appStore.countries} />
            </Suspense>
          )}
        </div>
      )}

      {p.releases && p.repo && (
        <div className="mt-5">
          <Suspense fallback={<Skeleton className="h-72" />}>
            <Releases project={p} repo={p.repo} />
          </Suspense>
        </div>
      )}

      {health.length > 0 && (
        <div className={`mt-5 grid gap-5 ${COLS[health.length]}`}>
          {probes.length > 0 && (
            <Suspense fallback={<Skeleton className="h-72" />}>
              <UptimePanel project={p} />
            </Suspense>
          )}
          {p.railway && (
            <Suspense fallback={<Skeleton className="h-72" />}>
              <DeployPanel project={p} />
            </Suspense>
          )}
          {(p.repo || p.dir) && (
            <Suspense fallback={<Skeleton className="h-72" />}>
              <CodePanel project={p} />
            </Suspense>
          )}
        </div>
      )}

      <div className="mt-5">
        <Suspense fallback={<Skeleton className="h-72" />}>
          <AgentsPanel project={p} />
        </Suspense>
      </div>

      <div className="mt-5 empty:hidden">
        <Suspense fallback={null}>
          <NotesPanel project={p} />
        </Suspense>
      </div>

      <div className="mt-5">
        <Suspense fallback={<Skeleton className="h-80" />}>
          <IdentityCard project={p} full={false} />
        </Suspense>
      </div>
    </>
  );
}

/** Highlights, a sentence and the screenshot. */
function About({ project: p }: { project: Project }) {
  return (
    <Panel kicker={tr("Carte d'identité", "At a glance")} title={tr("Ce qui tourne", "What it is")} accent={p.glow}>
      {p.shot && (
        // eslint-disable-next-line @next/next/no-img-element
        <img src={`/api/shot/${p.id}`} alt={tr(`Capture de ${p.name}`, `Screenshot of ${p.name}`)} className="mb-4 w-full rounded-2xl border border-line" />
      )}
      {p.highlights.length > 0 && (
        <div className="flex flex-wrap gap-2">
          {p.highlights.map((h, i) => (
            <Chip key={h} color={i < 2 ? p.color : undefined}>
              {h}
            </Chip>
          ))}
        </div>
      )}
      {p.about && <p className={`text-sm text-ink-3 ${p.highlights.length ? "mt-4" : ""}`}>{p.about}</p>}
    </Panel>
  );
}

/** Every project's screenshot scrolling by, for a showcase site. */
function Showcase({ project: p }: { project: Project }) {
  const shots = PROJECTS.filter((x) => x.shot);
  if (!shots.length) return null;
  return (
    <Panel kicker={tr("Vitrine", "Showcase")} title={tr(`Ce que ${p.name} montre`, `What ${p.name} shows`)} accent={p.glow} bodyClassName="px-0">
      <Marquee pauseOnHover className="[--duration:50s]">
        {shots.map((x) => (
          <figure key={x.id} className="relative h-56 w-auto shrink-0 overflow-hidden rounded-2xl border border-line">
            {/* eslint-disable-next-line @next/next/no-img-element */}
            <img src={`/api/shot/${x.id}`} alt={tr(`Capture de ${x.name}`, `Screenshot of ${x.name}`)} className="h-full w-auto object-cover" />
            <figcaption className="absolute bottom-2 left-2 inline-flex items-center gap-1.5 rounded-full bg-black/60 px-2.5 py-1 text-xs backdrop-blur">
              <span className="size-2 rounded-full" style={{ background: x.glow }} /> {x.name}
            </figcaption>
          </figure>
        ))}
      </Marquee>
    </Panel>
  );
}

/** GitHub releases: downloads per version and the latest releases. */
async function Releases({ project: p, repo }: { project: Project; repo: string }) {
  const rel = await source(() => releases(repo));
  return (
    <Panel kicker={tr("Distribution", "Distribution")} title={tr("Sorties & téléchargements", "Releases & downloads")} accent={p.glow}>
      <Gate src={rel}>
        {(all) => {
          const list = all.filter((r) => !r.prerelease);
          if (!list.length) return <Empty>{tr("Aucune version publiée sur GitHub pour l'instant.", "No release published on GitHub yet.")}</Empty>;
          const total = downloads(list);
          const recent = list.filter((r) => Date.now() - new Date(r.published_at).getTime() < 30 * 864e5).length;
          return (
            <div className="grid gap-8 xl:grid-cols-[1.6fr_1fr]">
              <div>
                <div className="mb-5 grid grid-cols-3 gap-6">
                  <Stat label={tr("Téléchargements", "Downloads")} value={total} color={p.glow} hint={tr(`sur ${list.length} versions`, `across ${list.length} ${plural(list.length, ["version", "versions"], ["release", "releases"])}`)} />
                  <Stat label={list[0].tag_name} value={downloads([list[0]])} hint={tr(`publiée ${ago(list[0].published_at)}`, `released ${ago(list[0].published_at)}`)} />
                  <Stat label={tr("Versions en 30 j", "Releases in 30 d")} value={recent} hint={tr("cadence de sortie", "release cadence")} />
                </div>
                <Bars data={list.slice(0, 12).reverse().map((r) => ({ label: r.tag_name, value: downloads([r]) }))} color={p.color} height={180} />
              </div>
              <ol className="relative space-y-4 border-l border-white/10 pl-5">
                {list.slice(0, 7).map((r, i) => (
                  <li key={r.tag_name} className="relative">
                    <span className="absolute -left-[25px] top-1 size-2.5 rounded-full ring-4 ring-[#0b0a14]" style={{ background: i === 0 ? p.glow : "rgb(255 255 255 / .25)" }} />
                    <div className="flex items-center gap-2">
                      <a href={r.html_url} target="_blank" rel="noopener noreferrer" className="font-medium text-ink hover:underline">
                        {r.name || r.tag_name}
                      </a>
                      {i === 0 && (
                        <Chip color={p.color}>
                          <Tag className="size-3" /> {tr("dernière", "latest")}
                        </Chip>
                      )}
                    </div>
                    <div className="mt-0.5 flex gap-3 text-xs text-ink-3">
                      <span>{date(r.published_at, { day: "numeric", month: "long" })}</span>
                      <span className="inline-flex items-center gap-1">
                        <Download className="size-3" />
                        {nf(downloads([r]))}
                      </span>
                    </div>
                  </li>
                ))}
              </ol>
            </div>
          );
        }}
      </Gate>
    </Panel>
  );
}

/** App Store listing and what people write, in every followed country. */
async function AppStore({ project: p, id, countries }: { project: Project; id: string; countries?: string[] }) {
  const [reviews, store] = await Promise.all([source(() => appReviews(id, countries)), source(() => appStore(id, countries?.[0]))]);
  const st = store.ok ? store.data : null;
  return (
    <Panel
      kicker="App Store"
      title={tr("Ce que disent les gens", "What people say")}
      accent={p.glow}
      action={
        st && (
          <a href={st.url} target="_blank" rel="noopener noreferrer" className="text-xs text-ink-3 hover:text-ink">
            v{st.version} · {st.rating != null ? `${st.rating.toFixed(1)} ★` : tr("pas encore de note", "no rating yet")} · {st.ratings}{" "}
            {plural(st.ratings, ["note", "notes"], ["rating", "ratings"])}
          </a>
        )
      }
    >
      <Gate src={reviews}>
        {(list) =>
          list.length ? (
            <ul className="grid gap-4 md:grid-cols-2">
              {list.slice(0, 8).map((r) => (
                <li key={r.country + r.author + r.at} className="rounded-2xl border border-line p-4 text-sm">
                  <div className="flex items-center gap-2">
                    <span className="flex" aria-label={`${r.rating}/5`}>
                      {Array.from({ length: 5 }, (_, i) => (
                        <Star key={i} className={i < r.rating ? "size-3.5 fill-sun text-sun" : "size-3.5 text-ink-3"} />
                      ))}
                    </span>
                    <span className="truncate font-medium text-ink">{r.title}</span>
                  </div>
                  <p className="mt-2 line-clamp-3 text-ink-2">{r.body}</p>
                  <div className="mt-2 text-xs text-ink-3">
                    {r.author} · {r.country} · v{r.version} · {ago(r.at)}
                  </div>
                </li>
              ))}
            </ul>
          ) : (
            <Empty>
              {tr("Aucun avis écrit pour l'instant", "No written reviews yet")}
              {countries?.length ? ` (${countries.map((c) => c.toUpperCase()).join(", ")})` : ""}.
            </Empty>
          )
        }
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
    <Panel kicker={tr("L'argent", "Money")} title={tr("Revenu par semaine", "Revenue per week")} accent={p.glow} className="relative">
      <BorderBeam size={120} duration={10} colorFrom={p.glow} colorTo="#FFD166" />
      {o && (
        <div className="mb-6 grid grid-cols-3 gap-6">
          <Stat label="MRR" value={o.mrr?.value ?? null} format={{ style: "currency", currency, maximumFractionDigits: 2 }} color={p.glow} hint={tr("revenu mensuel récurrent", "monthly recurring revenue")} />
          <Stat label={tr("Revenu 28 j", "Revenue 28 d")} value={o.revenue?.value ?? null} format={{ style: "currency", currency, maximumFractionDigits: 2 }} />
          <Stat
            label={tr("Abonnés", "Subscribers")}
            value={o.active_subscriptions?.value ?? null}
            hint={tr(`${o.active_trials?.value ?? 0} essai(s) en cours`, `${o.active_trials?.value ?? 0} active ${plural(o.active_trials?.value ?? 0, ["trial", "trials"], ["trial", "trials"])}`)}
          />
        </div>
      )}
      <Gate src={rev}>
        {(series) => {
          const total = series.reduce((a, b) => a + b.value, 0);
          return (
            <>
              <div className="mb-4 flex items-baseline gap-3">
                <span className="font-display text-3xl tabular">{base(total)}</span>
                <span className="text-sm text-ink-3">{tr("sur 16 semaines", "over 16 weeks")}</span>
              </div>
              <Bars data={series.map((s) => ({ label: date(s.t), value: s.value, incomplete: s.incomplete }))} color={p.color} unit="base" height={150} />
            </>
          );
        }}
      </Gate>
    </Panel>
  );
}
