import { Suspense, cache } from "react";
import Link from "next/link";
import {
  ArrowDownLeft,
  ArrowUpRight,
  Bot,
  CalendarDays,
  ChevronRight,
  CircleAlert,
  Cloud,
  CloudDrizzle,
  CloudFog,
  CloudLightning,
  CloudMoon,
  CloudRain,
  CloudSnow,
  CloudSun,
  GitCommitHorizontal,
  Moon,
  Radar,
  Rocket,
  Settings2,
  ShoppingBag,
  Sparkles,
  SquareTerminal,
  Star,
  Sun,
  TrendingUp,
  UserPlus,
  Wallet,
} from "lucide-react";
import type { Format } from "@number-flow/react";
import { config } from "@/lib/config";
import { PROJECTS, findProject, projectDir, type Project } from "@/lib/projects";
import { source } from "@/lib/source";
import { l10n, plural, tr } from "@/lib/i18n";
import { ago, compact, date, nf, weekly } from "@/lib/format";
import { activity, events, type Event } from "@/lib/overview";
import { EXTENSIONS, collect, type CardStat, type Kpi } from "@/lib/extensions";
import { cardStats } from "@/lib/integrations";
import { OWNER } from "@/lib/identity";
import { uptime } from "@/lib/sources/uptime";
import { allLocalRepos, projectVersion } from "@/lib/sources/git";
import { billing, traffic } from "@/lib/sources/railway";
import { fx } from "@/lib/sources/plans";
import { SUBSCRIPTIONS, monthly, urgent } from "@/lib/subscriptions";
import { describe, weather } from "@/lib/sources/weather";
import { life } from "@/lib/sources/life";
import { ProjectThreads } from "@/components/agents/project-threads";
import { Empty, PageHeader, Panel, Skeleton } from "@/components/z/panel";
import { Stat } from "@/components/z/stat";
import { LiveDot, Status } from "@/components/z/status";
import { Heatmap } from "@/components/charts/heatmap";
import { Bars } from "@/components/charts/bars";
import { now } from "@/lib/agent/now";
import { agentUi } from "@/lib/agent/ui";
import { NowList } from "@/components/agent/now-list";
import { AskButton } from "@/components/agent/ask-button";
import { setupPrompt } from "@/lib/agent/tasks";
import { needsWelcome } from "@/lib/setup";
import { redirect } from "next/navigation";

export const dynamic = "force-dynamic";

const DAY = 864e5;

/** Name and color of an event's project, with a neutral fallback. */
const who = (id: string) => findProject(id) ?? { name: id, color: "var(--ink-3)" };

/** What is waiting, once per request. */
const waiting = cache(now);

/** A quiet "see all" link for a card's header. */
function More({ href, children }: { href: string; children: React.ReactNode }) {
  return (
    <Link href={href} className="inline-flex items-center gap-0.5 text-xs text-ink-3 transition-colors hover:text-ink">
      {children}
      <ChevronRight className="size-3.5" />
    </Link>
  );
}

/** A count next to a card title. */
function Count({ n }: { n: number }) {
  return <span className="ml-1.5 font-normal text-ink-3 tabular">{n}</span>;
}

export default function Home() {
  if (needsWelcome()) redirect("/bienvenue");
  if (!PROJECTS.length) return <Welcome />;
  const agent = agentUi().enabled;
  return (
    <>
      <Suspense fallback={<HeaderShell />}>
        <Header />
      </Suspense>

      {agent ? (
        <Suspense fallback={<Skeleton className="mt-2 h-64" />}>
          <div className="mt-2">
            <Now />
          </div>
        </Suspense>
      ) : (
        <Urgent />
      )}

      <Suspense fallback={<Skeleton className="mt-8 h-[88px]" />}>
        <Kpis />
      </Suspense>

      <Projects agent={agent} />

      <div className="mt-8 grid gap-4 xl:grid-cols-[1.45fr_1fr]">
        <Suspense fallback={<Skeleton className="h-[480px]" />}>
          <Activity />
        </Suspense>
        <div className="flex min-w-0 flex-col gap-4">
          <Suspense fallback={<Skeleton className="h-72" />}>
            <Money />
          </Suspense>
          <Suspense fallback={<Skeleton className="h-72" />}>
            <Feed />
          </Suspense>
        </div>
      </div>
    </>
  );
}

/* ——— Header: greeting, today, weather, next appointment, how things run ——— */

/** Good morning / afternoon / evening, in your time zone. */
function greeting() {
  const h = Number(new Intl.DateTimeFormat("en-GB", { timeZone: l10n().timeZone, hour: "numeric", hourCycle: "h23" }).format(new Date()));
  const hello = h >= 5 && h < 12 ? tr("Bonjour", "Good morning") : h >= 12 && h < 18 ? tr("Bon après-midi", "Good afternoon") : tr("Bonsoir", "Good evening");
  return OWNER.firstName ? `${hello}, ${OWNER.firstName}` : hello;
}

function todayLabel() {
  const s = date(Date.now(), { weekday: "long", day: "numeric", month: "long" });
  return s.charAt(0).toUpperCase() + s.slice(1);
}

function HeaderShell({ context, action }: { context?: React.ReactNode; action?: React.ReactNode }) {
  return (
    <PageHeader
      title={greeting()}
      description={
        <span className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <span>{todayLabel()}</span>
          {context}
        </span>
      }
      action={action}
    />
  );
}

/** Lucide symbol of a WMO weather code. */
function WeatherIcon({ code, day, className }: { code: number; day: boolean; className?: string }) {
  const Icon =
    code === 0
      ? day
        ? Sun
        : Moon
      : code <= 2
        ? day
          ? CloudSun
          : CloudMoon
        : code === 3
          ? Cloud
          : code <= 48
            ? CloudFog
            : code <= 57
              ? CloudDrizzle
              : code <= 67 || (code >= 80 && code <= 82)
                ? CloudRain
                : code <= 86
                  ? CloudSnow
                  : CloudLightning;
  return <Icon className={className} />;
}

const Sep = () => <span className="text-ink-3/50">·</span>;

async function Header() {
  const [w, l, probes, repos] = await Promise.all([source(weather), source(life), uptime().catch(() => []), allLocalRepos()]);
  const next = l.ok ? l.data?.agenda.find((e) => new Date(e.end ?? e.start).getTime() > Date.now()) : null;
  const { locale, timeZone } = l10n();
  const sameDay = (t: string) => {
    const f = new Intl.DateTimeFormat("en-CA", { timeZone });
    return f.format(new Date(t)) === f.format(new Date());
  };
  const when = (e: NonNullable<typeof next>) => {
    if (e.allDay) return sameDay(e.start) || new Date(e.start).getTime() < Date.now() ? tr("aujourd'hui", "today") : date(e.start, { weekday: "short", day: "numeric", month: "short" });
    const opts: Intl.DateTimeFormatOptions = sameDay(e.start) ? { hour: "2-digit", minute: "2-digit" } : { weekday: "short", hour: "2-digit", minute: "2-digit" };
    return new Intl.DateTimeFormat(locale, { timeZone, ...opts }).format(new Date(e.start));
  };

  const up = probes.filter((u) => u.up).length;
  const down = probes.length - up;
  const today = repos.flatMap((r) => r.commits).filter((c) => c.at > Date.now() - DAY).length;

  const context = (
    <>
      {w.ok && (
        <>
          <Sep />
          <Link href="/vie" className="inline-flex items-center gap-1.5 transition-colors hover:text-ink">
            <WeatherIcon code={w.data.now.code} day={w.data.now.isDay} className="size-3.5" />
            <span className="font-medium text-ink-2 tabular">{Math.round(w.data.now.temp)}°</span>
            <span>{describe(w.data.now.code, w.data.now.isDay).label.toLowerCase()}</span>
            {w.data.days[0] && (
              <span className="tabular">
                ({Math.round(w.data.days[0].min)}° / {Math.round(w.data.days[0].max)}°)
              </span>
            )}
          </Link>
        </>
      )}
      {next && (
        <>
          <Sep />
          <Link href="/vie" className="inline-flex min-w-0 max-w-[28rem] items-center gap-1.5 transition-colors hover:text-ink">
            <CalendarDays className="size-3.5 shrink-0" />
            <span className="truncate text-ink-2">{next.title}</span>
            <span className="shrink-0">{when(next)}</span>
          </Link>
        </>
      )}
    </>
  );

  const action = (
    <div className="flex items-center gap-3 text-xs text-ink-3">
      {probes.length > 0 && (
        <Link
          href={down ? (findProject(probes.find((p) => !p.up)?.project ?? "")?.href ?? "/") : "/veille"}
          className="inline-flex h-7 items-center gap-2 rounded-md border border-line bg-surface px-2.5 text-ink-2 transition-colors hover:bg-hover hover:text-ink"
          title={tr("Sondés chaque minute depuis ce Mac", "Checked every minute from this Mac")}
        >
          <LiveDot health={down === 0 ? "up" : "down"} />
          {down === 0
            ? tr(`${up}/${probes.length} en ligne`, `${up}/${probes.length} up`)
            : tr(`${down} hors ligne`, `${down} down`)}
        </Link>
      )}
      <span className="hidden items-center gap-1.5 tabular sm:inline-flex">
        <GitCommitHorizontal className="size-3.5" />
        {today ? tr(`${today} commit${today > 1 ? "s" : ""} en 24 h`, `${today} ${plural(today, ["commit", "commits"], ["commit", "commits"])} in 24 h`) : tr("aucun commit en 24 h", "no commits in 24 h")}
      </span>
    </div>
  );

  return <HeaderShell context={context} action={action} />;
}

/* ——— What is waiting ——— */

const projectNames = () => Object.fromEntries(agentUi().targets.map((t) => [t.id, t.name]));

async function Now() {
  const items = await waiting().catch(() => []);
  return <NowList items={items} colors={Object.fromEntries(PROJECTS.map((p) => [p.id, p.color]))} projectNames={projectNames()} />;
}

/** Without agents: failing payments, what breaks soon if nobody deals with it. */
function Urgent() {
  const list = urgent();
  if (!list.length) return null;
  const n = list.length;
  return (
    <Link href="/abonnements" className="flex items-center gap-3 rounded-xl border border-bad/25 bg-bad/5 px-4 py-3 text-[13px] transition-colors hover:bg-bad/10">
      <CircleAlert className="size-4 shrink-0 text-bad" />
      <div className="min-w-0 flex-1 truncate">
        <span className="font-medium text-ink">{tr(`${n} paiement${n > 1 ? "s" : ""} en échec · `, `${n} failed ${plural(n, ["payment", "payments"], ["payment", "payments"])} · `)}</span>
        <span className="text-ink-2">{list.map((s) => s.name).join(", ")}</span>
        {list[0].evidence && <span className="text-ink-3"> · {list[0].evidence}</span>}
      </div>
      <ChevronRight className="size-4 shrink-0 text-ink-3" />
    </Link>
  );
}

/* ——— Key numbers ——— */

async function Kpis() {
  const [extra, repos, probes] = await Promise.all([collect((e) => e.kpis), allLocalRepos(), uptime()]);
  const commits30 = repos.flatMap((r) => r.commits).filter((c) => c.at > Date.now() - 30 * DAY).length;
  const tiles: Kpi[] = [
    ...extra,
    {
      label: tr("Commits 30 j", "Commits 30 d"),
      value: commits30,
      hint: tr(`${repos.length} dépôt${repos.length > 1 ? "s" : ""}, toutes branches`, `${repos.length} ${plural(repos.length, ["repo", "repos"], ["repo", "repos"])}, all branches`),
    },
    ...(probes.length
      ? [{ label: tr("Services en ligne", "Services up"), value: probes.filter((u) => u.up).length, suffix: ` / ${probes.length}`, hint: tr("sondés chaque minute", "checked every minute") }]
      : []),
  ];
  const n = tiles.length;
  const cols = n >= 6 ? "lg:grid-cols-6" : n === 5 ? "lg:grid-cols-5" : n === 4 ? "lg:grid-cols-4" : n === 3 ? "lg:grid-cols-3" : "lg:grid-cols-2";
  // Inner dividers that survive any wrap: each cell draws its top and left edge, the card clips the outer ones.
  return (
    <div className={`mt-8 grid grid-cols-2 overflow-hidden rounded-xl border border-line bg-surface sm:grid-cols-3 ${cols}`}>
      {tiles.map((k, i) => (
        <Stat
          key={`${k.label}${i}`}
          label={k.label}
          value={k.value}
          format={k.format as Format | undefined}
          suffix={k.suffix}
          hint={k.hint}
          color={k.project ? findProject(k.project)?.color : undefined}
          className="-ml-px -mt-px border-l border-t border-line px-4 py-3"
        />
      ))}
    </div>
  );
}

/* ——— Projects: one dense row each ——— */

/** Railway traffic, for projects no extension describes. */
async function trafficStats(p: Project): Promise<CardStat[]> {
  if (!p.railway) return [];
  const t = await source(() => traffic(p));
  return [
    { label: tr("requêtes", "requests"), value: t.ok && t.data ? compact(t.data.requests) : "—" },
    { label: tr("IP uniques", "unique IPs"), value: t.ok && t.data ? compact(t.data.visitors) : "—" },
  ];
}

/** Up to two numbers from the extensions (else Railway traffic), then commits over 30 days. */
async function stats(p: Project): Promise<CardStat[]> {
  const first = await cardStats(p, () => trafficStats(p), EXTENSIONS);
  const fill = first.length < 2 ? (await trafficStats(p)).filter((s) => !first.some((x) => x.label === s.label)) : [];
  const out = [...first, ...fill].slice(0, 2);
  if (p.dir) {
    const repo = (await allLocalRepos()).find((r) => r.project === p.id);
    const c30 = repo ? repo.commits.filter((c) => c.at > Date.now() - 30 * DAY).length : 0;
    out.push({ label: tr("commits 30 j", "commits 30 d"), value: nf(c30) });
  }
  return out;
}

const ROW = "grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-4 gap-y-1.5 px-4 py-3 lg:grid-cols-[minmax(0,1.3fr)_8.5rem_minmax(0,1.7fr)_6.5rem_7.5rem]";

function Projects({ agent }: { agent: boolean }) {
  return (
    <Panel
      className="mt-8"
      title={
        <>
          {tr("Projets", "Projects")}
          <Count n={PROJECTS.length} />
        </>
      }
      action={config().code.enabled ? <More href="/code">zenith code</More> : undefined}
      bodyClassName="p-0"
    >
      <div className={`${ROW} hidden border-t border-line py-2 text-xs font-medium text-ink-3 lg:grid`}>
        <span>{tr("Projet", "Project")}</span>
        <span>{tr("État", "Status")}</span>
        <span>{tr("Chiffres", "Numbers")}</span>
        <span>{tr("12 semaines", "12 weeks")}</span>
        <span className="text-right">{tr("Dernier commit", "Last commit")}</span>
      </div>
      <ul className="divide-y divide-line border-t border-line">
        {PROJECTS.map((p) => (
          <Suspense key={p.id} fallback={<ProjectRowSkeleton p={p} />}>
            <ProjectRow p={p} agent={agent} />
          </Suspense>
        ))}
      </ul>
    </Panel>
  );
}

function ProjectRowSkeleton({ p }: { p: Project }) {
  return (
    <li className={ROW}>
      <span className="flex items-center gap-2 text-[13px] font-medium text-ink">
        <span className="size-2 rounded-full" style={{ background: p.color }} />
        {p.name}
      </span>
      <span className="h-3 w-20 rounded bg-muted" />
      <span className="col-span-2 h-3 w-48 rounded bg-muted lg:col-span-1" />
      <span className="hidden h-6 rounded bg-muted lg:block" />
      <span className="hidden h-3 w-16 justify-self-end rounded bg-muted lg:block" />
    </li>
  );
}

async function ProjectRow({ p, agent }: { p: Project; agent: boolean }) {
  const [list, probes, repos, version] = await Promise.all([stats(p), uptime(), allLocalRepos(), p.dir ? projectVersion(p.id).catch(() => null) : null]);
  const mine = probes.filter((u) => u.project === p.id);
  const up = mine.length ? (mine.some((u) => u.up == null) ? null : mine.every((u) => u.up)) : null;
  const ms = mine.find((u) => u.up)?.last?.ms;
  const ratio = mine.length && mine.every((u) => u.ratio != null) ? Math.min(...mine.map((u) => u.ratio!)) : null;
  const repo = repos.find((r) => r.project === p.id);
  const series = repo ? weekly(repo.commits.map((c) => c.at), 12) : [];
  const max = Math.max(1, ...series.map((x) => x.value));
  const last = repo?.commits.reduce<(typeof repo.commits)[number] | null>((a, c) => (!a || c.at > a.at ? c : a), null) ?? null;

  return (
    <li className={`group relative transition-colors hover:bg-hover ${ROW}`}>
      <Link href={p.href} className="absolute inset-0 outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring/60" aria-label={p.name} />

      <div className="min-w-0">
        <div className="flex items-center gap-2">
          <span className="size-2 shrink-0 rounded-full" style={{ background: p.color }} />
          <span className="truncate text-[13px] font-medium text-ink">{p.name}</span>
          {version && <span className="shrink-0 text-2xs text-ink-3 tabular">v{version.replace(/^v/, "")}</span>}
          <ProjectThreads dir={projectDir(p)} />
        </div>
        {p.tagline && <div className="mt-0.5 truncate pl-4 text-xs text-ink-3">{p.tagline}</div>}
      </div>

      <div className="justify-self-end lg:justify-self-start" title={ratio != null ? tr(`${nf(ratio * 100, 1)} % de réponses`, `${nf(ratio * 100, 1)}% answered`) : undefined}>
        {mine.length ? (
          <Status
            health={up == null ? "unknown" : up ? "up" : "down"}
            label={up == null ? tr("Mesure…", "Measuring…") : up ? (ms != null ? `${ms} ms` : tr("En ligne", "Online")) : tr("Hors ligne", "Offline")}
          />
        ) : (
          <span className="text-xs text-ink-3">—</span>
        )}
      </div>

      <div className="col-span-2 flex min-w-0 flex-wrap gap-x-4 gap-y-0.5 pl-4 text-xs lg:col-span-1 lg:pl-0">
        {list.map((s) => (
          <span key={s.label} className="whitespace-nowrap">
            <span className="font-semibold text-ink tabular">{s.value}</span> <span className="text-ink-3">{s.label}</span>
          </span>
        ))}
      </div>

      <div className="hidden h-6 items-end gap-[2px] lg:flex" aria-label={tr("Commits par semaine, 12 semaines", "Commits per week, 12 weeks")}>
        {series.map((w, i) => (
          <div
            key={i}
            className="flex-1 rounded-[1.5px]"
            style={{ height: w.value ? `${Math.max(18, (w.value / max) * 100)}%` : "2px", background: w.value ? p.color : "color-mix(in srgb, var(--foreground) 10%, transparent)" }}
            title={tr(`${w.label} : ${w.value} commits`, `${w.label}: ${w.value} ${plural(w.value, ["commit", "commits"], ["commit", "commits"])}`)}
          />
        ))}
      </div>

      <div className="hidden items-center justify-end lg:flex">
        <span className={`text-right text-xs text-ink-3 tabular ${agent ? "group-hover:hidden group-focus-within:hidden" : ""}`} title={last?.subject}>
          {last ? ago(last.at) : "—"}
          {repo && repo.dirty > 0 && (
            <span className="block text-2xs text-warn">{tr(`${repo.dirty} non commité${repo.dirty > 1 ? "s" : ""}`, `${repo.dirty} uncommitted`)}</span>
          )}
        </span>
        {agent && (
          <AskButton
            className="relative z-10 hidden group-hover:inline-flex group-focus-within:inline-flex"
            target={p.id}
            label={tr("Faire le point", "Check in")}
            prompt={tr(
              `Fais le point sur ${p.name} : derniers commits et branches, travail non commité, état du site et des déploiements, CI, ce qui est en cours. Réponds en 6 lignes au plus, puis propose les 3 prochaines actions. Ne modifie rien.`,
              `Give me a status check on ${p.name}: latest commits and branches, uncommitted work, site and deploy health, CI, what's in progress. Answer in 6 lines at most, then suggest the 3 next actions. Don't change anything.`,
            )}
          />
        )}
      </div>
    </li>
  );
}

/* ——— Money ——— */

type Row = { label: string; value: number | null; currency: string; sign: 1 | -1; hint: string; color: string | null };

async function Money() {
  const withRailway = PROJECTS.filter((p) => p.railway);
  const [extra, rw, rates] = await Promise.all([collect((e) => e.money), withRailway.length ? source(billing) : null, SUBSCRIPTIONS.length ? fx() : null]);
  const base = l10n().currency;
  const rows: Row[] = extra.map((m) => ({ ...m, color: (m.project && findProject(m.project)?.color) || null }));
  if (rw)
    rows.push({
      label: tr("Railway · période en cours", "Railway · current period"),
      value: rw.ok ? rw.data.currentUsage : null,
      currency: "USD",
      sign: -1,
      hint:
        rw.ok && rw.data.period
          ? tr(`facturé le ${date(rw.data.period.end, { day: "numeric", month: "long" })}`, `billed on ${date(rw.data.period.end, { day: "numeric", month: "long" })}`)
          : tr("Railway à brancher", "Connect Railway"),
      color: null,
    });
  let subCount = 0;
  if (rates) {
    // Railway has its own live row: don't count it twice among the subscriptions.
    const subs = SUBSCRIPTIONS.filter((s) => (s.status === "active" || s.status === "failing") && !(rw && s.vendor === "Railway"));
    subCount = subs.length;
    const toBase = rates.toBase;
    const total = subs.reduce((a, s) => a + monthly(s) * (s.currency.toUpperCase() === base.toUpperCase() ? 1 : (toBase[s.currency.toUpperCase()] ?? 1)), 0);
    rows.push({
      label: tr("Abonnements · par mois", "Subscriptions · per month"),
      value: total,
      currency: base,
      sign: -1,
      hint: tr(`${subs.length} abonnements actifs`, `${subs.length} active ${plural(subs.length, ["subscription", "subscriptions"], ["subscription", "subscriptions"])}`),
      color: null,
    });
  }
  if (!rows.length) return null;
  const fmt = (v: number, c: string) => new Intl.NumberFormat(l10n().locale, { style: "currency", currency: c, maximumFractionDigits: 2 }).format(v);
  const share = rw?.ok ? withRailway.flatMap((p) => (p.railway && rw.data.byProject[p.railway.projectId] ? [{ p, value: rw.data.byProject[p.railway.projectId] }] : [])) : [];
  const failing = urgent().length;
  return (
    <Panel
      title={tr("Argent", "Money")}
      action={
        <>
          {failing > 0 && (
            <Link href="/abonnements" className="inline-flex items-center gap-1 text-bad hover:underline">
              <CircleAlert className="size-3.5" />
              {tr(`${failing} en échec`, `${failing} failing`)}
            </Link>
          )}
          <More href="/abonnements">{subCount ? tr("Abonnements", "Subscriptions") : tr("Détail", "Details")}</More>
        </>
      }
      bodyClassName="px-4 pb-4 pt-1"
    >
      <ul className="divide-y divide-line">
        {rows.map((r) => (
          <li key={r.label} className="flex items-center gap-3 py-2.5">
            <span className="grid size-7 shrink-0 place-items-center rounded-md border border-line bg-muted">
              {r.sign > 0 ? <ArrowDownLeft className="size-3.5 text-good" /> : <ArrowUpRight className="size-3.5 text-ink-3" />}
            </span>
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-1.5 truncate text-[13px] text-ink">
                {r.color && <span className="size-1.5 shrink-0 rounded-full" style={{ background: r.color }} />}
                <span className="truncate">{r.label}</span>
              </div>
              <div className="truncate text-xs text-ink-3">{r.hint}</div>
            </div>
            <span className={`shrink-0 text-[13px] font-semibold tabular ${r.sign > 0 && r.value ? "text-good" : "text-ink"}`}>
              {r.value == null ? <span className="text-ink-3">—</span> : `${r.sign > 0 ? "+" : "−"}${fmt(r.value, r.currency)}`}
            </span>
          </li>
        ))}
      </ul>
      {share.length > 0 && (
        <div className="mt-2 border-t border-line pt-3">
          <div className="mb-2 text-xs text-ink-3">{tr("Railway par projet · projection sur la période", "Railway per project · projected over the period")}</div>
          <div className="flex h-1.5 gap-px overflow-hidden rounded-full bg-muted">
            {share.map(({ p, value }) => (
              <div key={p.id} className="h-full" style={{ flexGrow: value, background: p.color }} title={tr(`${p.name} : ${fmt(value, "USD")}`, `${p.name}: ${fmt(value, "USD")}`)} />
            ))}
          </div>
          <div className="mt-2 flex flex-wrap gap-x-4 gap-y-1 text-xs text-ink-3">
            {share.map(({ p, value }) => (
              <span key={p.id} className="inline-flex items-center gap-1.5">
                <span className="size-1.5 rounded-full" style={{ background: p.color }} />
                {p.name} <span className="text-ink-2 tabular">{fmt(value, "USD")}</span>
              </span>
            ))}
          </div>
        </div>
      )}
    </Panel>
  );
}

/* ——— Agents at work ——— */

/* ——— Activity & feed ——— */

async function Activity() {
  const [days, repos] = await Promise.all([activity(), allLocalRepos()]);
  if (!repos.length) return null;
  const total = days.reduce((a, d) => a + d.total, 0);
  const streak = (() => {
    let n = 0;
    for (let i = days.length - 1; i >= 0 && days[i].total > 0; i--) n++;
    return n;
  })();
  const weeks = weekly([], 16).map((w) => ({
    label: w.label,
    parts: PROJECTS.map((p) => ({
      key: p.id,
      name: p.name,
      color: p.color,
      value: (repos.find((r) => r.project === p.id)?.commits ?? []).filter((c) => c.at >= w.t && c.at < w.t + 7 * DAY).length,
    })),
  }));
  return (
    <Panel
      title={tr("Activité", "Activity")}
      action={
        <span className="tabular">
          {tr(`${nf(total)} commits en 6 mois`, `${nf(total)} ${plural(total, ["commit", "commits"], ["commit", "commits"])} in 6 months`)}
          {streak > 1 && tr(` · ${streak} jours d'affilée`, ` · ${streak}-day streak`)}
        </span>
      }
    >
      <Heatmap days={days} />
      <div className="mt-5 border-t border-line pt-4">
        <div className="mb-3 text-xs text-ink-3">{tr("Commits par semaine et par projet", "Commits per week and project")}</div>
        <Bars data={weeks} height={150} legend={PROJECTS.map((p) => ({ name: p.name, color: p.color }))} />
      </div>
    </Panel>
  );
}

const ICON: Record<Event["kind"], typeof GitCommitHorizontal> = {
  commit: GitCommitHorizontal,
  signup: UserPlus,
  release: Sparkles,
  deploy: Rocket,
  trade: TrendingUp,
  agent: Bot,
  review: Star,
  sale: ShoppingBag,
};

async function Feed() {
  const list = await events(30);
  return (
    <Panel title={tr("Fil d'activité", "Feed")} action={<LiveDot />} bodyClassName={list.length ? "p-0" : undefined} className="flex flex-col">
      {list.length ? (
        <ol className="max-h-[500px] divide-y divide-line overflow-y-auto border-t border-line">
          {list.map((e, i) => {
            const p = who(e.project);
            const Icon = ICON[e.kind] ?? Sparkles;
            const body = (
              <>
                <Icon className="mt-0.5 size-3.5 shrink-0 text-ink-3" />
                <div className="min-w-0 flex-1">
                  <div className="truncate text-[13px] text-ink">{e.text}</div>
                  <div className="mt-0.5 flex items-center gap-1.5 text-xs text-ink-3">
                    <span className="size-1.5 shrink-0 rounded-full" style={{ background: p.color }} />
                    <span className="truncate">{p.name}</span>
                    <span className="text-ink-3/60">·</span>
                    <span className="shrink-0 tabular">{ago(e.at)}</span>
                  </div>
                </div>
                {e.href && <ArrowUpRight className="mt-0.5 size-3.5 shrink-0 text-ink-3 opacity-0 transition-opacity group-hover:opacity-100" />}
              </>
            );
            return (
              <li key={i}>
                {e.href ? (
                  <a href={e.href} target="_blank" rel="noopener noreferrer" className="group flex gap-3 px-4 py-2 transition-colors hover:bg-hover">
                    {body}
                  </a>
                ) : (
                  <div className="flex gap-3 px-4 py-2">{body}</div>
                )}
              </li>
            );
          })}
        </ol>
      ) : (
        <Empty>{tr("Rien de neuf pour l'instant : les commits, déploiements et sessions d'agents apparaîtront ici.", "Nothing new yet: commits, deploys and agent sessions will show up here.")}</Empty>
      )}
    </Panel>
  );
}

/* ——— First run: no project configured ——— */

function Welcome() {
  const { meta } = config();
  const steps = [
    { text: tr("Copie le fichier d'exemple à la racine de zenith :", "Copy the example file at the root of zenith:"), code: "cp zenith.config.example.json zenith.config.json" },
    { text: tr("Remplace les projets d'exemple par les tiens : nom, dossier local, dépôt GitHub, site à surveiller.", "Replace the sample projects with yours: name, local folder, GitHub repository, site to watch.") },
    { text: tr("Relance zenith : tout se remplit, sans base de données ni compte.", "Restart zenith: everything fills in, no database, no account.") },
  ];
  const works = [
    { href: "/agents", icon: Bot, name: tr("Agents IA", "AI agents"), hint: tr("tes sessions Claude Code et Codex", "your Claude Code and Codex sessions") },
    ...(config().location ? [{ href: "/vie", icon: Sun, name: tr("Ma vie", "My life"), hint: tr(`météo à ${config().location?.name}`, `weather in ${config().location?.name}`) }] : []),
    ...(config().code.enabled ? [{ href: "/code", icon: SquareTerminal, name: "zenith code", hint: tr("coder avec les agents", "code with agents") }] : []),
    { href: "/veille", icon: Radar, name: tr("Veille", "Radar"), hint: tr("actualité et ce Mac", "news and this Mac") },
    { href: "/abonnements", icon: Wallet, name: tr("Abonnements", "Subscriptions"), hint: tr("limites IA et frais", "AI limits and fees") },
  ];
  return (
    <>
      <Suspense fallback={<HeaderShell />}>
        <Header />
      </Suspense>

      <div className="grid gap-4 lg:grid-cols-[1.4fr_1fr]">
        <Panel
          title={meta.found ? tr("Ajoute un premier projet", "Add a first project") : tr("Bienvenue dans zenith", "Welcome to zenith")}
          kicker={<span className="font-mono">zenith.config.json</span>}
        >
          <p className="max-w-xl text-[13px] text-ink-2">
            {tr(
              "zenith tourne en local et ne lit tes données que dans un fichier : tes projets, ta ville, ta devise, tes abonnements. Il n'est jamais publié.",
              "zenith runs locally and reads your data from a single file: your projects, your city, your currency, your subscriptions. It is never published.",
            )}
          </p>
          {meta.error && (
            <div className="mt-4 flex items-start gap-2.5 rounded-lg border border-bad/25 bg-bad/5 px-3 py-2.5 text-[13px] text-ink-2">
              <CircleAlert className="mt-0.5 size-4 shrink-0 text-bad" />
              <div className="min-w-0 break-words">
                {tr("Le fichier n'a pas pu être lu : ", "The file could not be read: ")}
                <code className="font-mono text-xs text-bad">{meta.error}</code>
              </div>
            </div>
          )}
          <ol className="mt-5 space-y-3">
            {steps.map((s, i) => (
              <li key={i} className="flex gap-3">
                <span className="grid size-5 shrink-0 place-items-center rounded-full border border-line bg-muted text-2xs font-medium text-ink-2 tabular">{i + 1}</span>
                <div className="min-w-0 flex-1 text-[13px] text-ink-2">
                  {s.text}
                  {s.code && <pre className="mt-2 overflow-x-auto rounded-md border border-line bg-muted px-3 py-2 font-mono text-xs text-ink">{s.code}</pre>}
                </div>
              </li>
            ))}
          </ol>
          <div className="mt-5 flex flex-wrap items-center gap-2">
            {agentUi().enabled && <AskButton variant="primary" prompt={setupPrompt(config().projectsRoot)} target="zenith" label={tr("Laisser zenith se configurer", "Let zenith set itself up")} />}
            <Link href="/reglages" className="inline-flex h-8 items-center gap-1.5 rounded-md border border-line bg-surface px-3 text-[13px] text-ink-2 transition-colors hover:bg-hover hover:text-ink">
              <Settings2 className="size-3.5" /> {tr("Ouvrir les réglages", "Open settings")}
            </Link>
            <span className="truncate font-mono text-2xs text-ink-3">{meta.file}</span>
          </div>
        </Panel>

        <Panel title={tr("Ce qui marche déjà", "Already working")} bodyClassName="p-0">
          <ul className="divide-y divide-line border-t border-line">
            {works.map((w) => (
              <li key={w.href}>
                <Link href={w.href} className="flex items-center gap-3 px-4 py-2.5 transition-colors hover:bg-hover">
                  <w.icon className="size-4 shrink-0 text-ink-3" />
                  <div className="min-w-0 flex-1">
                    <div className="text-[13px] text-ink">{w.name}</div>
                    <div className="truncate text-xs text-ink-3">{w.hint}</div>
                  </div>
                  <ChevronRight className="size-4 text-ink-3" />
                </Link>
              </li>
            ))}
          </ul>
        </Panel>
      </div>

    </>
  );
}
