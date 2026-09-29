import { Suspense, cache } from "react";
import Link from "next/link";
import {
  ArrowUpRight,
  Bot,
  CircleAlert,
  FileJson,
  GitCommitHorizontal,
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
import { PROJECTS, findProject, type Project } from "@/lib/projects";
import { source } from "@/lib/source";
import { l10n, plural, tr } from "@/lib/i18n";
import { ago, compact, date, nf, weekly } from "@/lib/format";
import { activity, events, type Event } from "@/lib/overview";
import { EXTENSIONS, collect, type CardStat, type Kpi } from "@/lib/extensions";
import { cardStats } from "@/lib/integrations";
import { OWNER } from "@/lib/identity";
import { uptime } from "@/lib/sources/uptime";
import { allLocalRepos } from "@/lib/sources/git";
import { billing, traffic } from "@/lib/sources/railway";
import { fx } from "@/lib/sources/plans";
import { SUBSCRIPTIONS, monthly, urgent } from "@/lib/subscriptions";
import { describe, weather } from "@/lib/sources/weather";
import { life } from "@/lib/sources/life";
import { AGENTS, isLive, sessions } from "@/lib/sources/agents";
import { SessionList } from "@/components/agents/session-list";
import { toRows } from "@/components/agents/rows";
import { Gate } from "@/components/z/gate";
import { Orbit } from "@/components/overview/orbit";
import { Greeting } from "@/components/overview/greeting";
import { Empty, Panel, Skeleton } from "@/components/z/panel";
import { Stat } from "@/components/z/stat";
import { LiveDot, Status } from "@/components/z/status";
import { Heatmap } from "@/components/charts/heatmap";
import { Bars } from "@/components/charts/bars";
import { Marquee } from "@/components/ui/marquee";
import { BorderBeam } from "@/components/ui/border-beam";
import { AnimatedShinyText } from "@/components/ui/animated-shiny-text";
import { now } from "@/lib/agent/now";
import { SUGGESTIONS, agentUi, examplesFrom } from "@/lib/agent/ui";
import { AskBar } from "@/components/agent/ask-bar";
import { NowList } from "@/components/agent/now-list";
import { AskButton } from "@/components/agent/ask-button";
import { setupPrompt } from "@/lib/agent/tasks";

export const dynamic = "force-dynamic";

const DAY = 864e5;

/** Name and colors of an event's project, with a neutral fallback. */
const who = (id: string) => findProject(id) ?? { name: id, color: "#9C97AD", glow: "#C9C4D9" };

/** What is waiting, once per request (the ask bar and the Now list both use it). */
const waiting = cache(now);

export default function Home() {
  if (!PROJECTS.length) return <Welcome />;
  const agent = agentUi().enabled;
  return (
    <>
      <Suspense fallback={<div className="h-12" />}>
        <Ticker />
      </Suspense>

      {!agent && <Urgent />}

      <section className="mt-4 grid items-center gap-8 lg:grid-cols-[1.1fr_1fr]">
        <div>
          <Greeting name={OWNER.firstName} place={OWNER.place} />
          {agent && (
            <Suspense fallback={<div className="mt-8 h-[124px] max-w-2xl rounded-[26px] border border-line bg-white/[0.03]" />}>
              <Ask />
            </Suspense>
          )}
          <Suspense fallback={<p className="mt-6 h-16" />}>
            <Summary />
          </Suspense>
          <Suspense fallback={<div className="mt-6 h-20" />}>
            <Today />
          </Suspense>
        </div>
        <Suspense fallback={<div className="mx-auto aspect-square w-full max-w-[520px]" />}>
          <SolarSystem />
        </Suspense>
      </section>

      {agent && (
        <Suspense fallback={<Skeleton className="mt-8 h-72" />}>
          <div className="mt-8">
            <Now />
          </div>
        </Suspense>
      )}

      <Suspense fallback={<Skeleton className="mt-8 h-28" />}>
        <Kpis />
      </Suspense>

      <div className="mt-8 grid gap-5 lg:grid-cols-6">
        {PROJECTS.map((p, i) => {
          const span = cardSpan(i, PROJECTS.length);
          return (
            <Suspense key={p.id} fallback={<Skeleton className={`h-72 ${span}`} />}>
              <ProjectCard p={p} span={span} index={i} />
            </Suspense>
          );
        })}
      </div>

      <div className="mt-8 grid gap-5 xl:grid-cols-[1fr_1.4fr]">
        <Suspense fallback={<Skeleton className="h-80" />}>
          <Money />
        </Suspense>
        <Suspense fallback={<Skeleton className="h-80" />}>
          <LiveAgents />
        </Suspense>
      </div>

      <div className="mt-8 grid gap-5 xl:grid-cols-[1.6fr_1fr]">
        <Suspense fallback={<Skeleton className="h-96" />}>
          <Activity />
        </Suspense>
        <Suspense fallback={<Skeleton className="h-96" />}>
          <Feed />
        </Suspense>
      </div>
    </>
  );
}

/**
 * Card widths on the 6-column grid: the first two cards are wide, the others go three
 * per row; a last row of one or two cards stretches to fill it.
 */
function cardSpan(i: number, n: number) {
  if (n === 1) return "lg:col-span-6";
  if (i < 2) return "lg:col-span-3";
  const rest = n - 2;
  const lastRow = rest % 3;
  const inLastRow = lastRow > 0 && i - 2 >= rest - lastRow;
  if (inLastRow && lastRow === 1) return "lg:col-span-6";
  if (inLastRow && lastRow === 2) return "lg:col-span-3";
  return "lg:col-span-2";
}

/* ——— Event ticker ——— */

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

async function Ticker() {
  const list = await events(16);
  if (!list.length) return null;
  return (
    <div className="-mx-4 border-y border-line bg-black/20 backdrop-blur sm:-mx-6 lg:-mx-10 lg:-mt-8">
      <Marquee pauseOnHover className="py-2 [--duration:70s] [--gap:2.5rem]">
        {list.map((e, i) => {
          const p = who(e.project);
          const Icon = ICON[e.kind] ?? Sparkles;
          return (
            <span key={i} className="inline-flex items-center gap-2 whitespace-nowrap text-xs text-ink-2">
              <Icon className="size-3.5" style={{ color: p.glow }} />
              <span className="font-medium text-ink">{p.name}</span>
              <span className="max-w-[40ch] truncate">{e.text}</span>
              <span className="text-ink-3">{ago(e.at)}</span>
            </span>
          );
        })}
      </Marquee>
    </div>
  );
}

/* ——— Ask zenith, and what is waiting ——— */

const projectNames = () => Object.fromEntries(agentUi().targets.map((t) => [t.id, t.name]));

async function Ask() {
  const ui = agentUi();
  const items = await waiting().catch(() => []);
  return (
    <AskBar
      className="mt-8 max-w-2xl"
      targets={ui.targets}
      provider={ui.provider}
      examples={examplesFrom(items, projectNames())}
      suggestions={SUGGESTIONS()}
    />
  );
}

async function Now() {
  const items = await waiting().catch(() => []);
  return <NowList items={items} colors={Object.fromEntries(PROJECTS.map((p) => [p.id, p.glow]))} projectNames={projectNames()} />;
}

/* ——— Header ——— */

async function Summary() {
  const [probes, repos] = await Promise.all([uptime(), allLocalRepos()]);
  const up = probes.filter((u) => u.up).length;
  const down = probes.length - up;
  const today = repos.flatMap((r) => r.commits).filter((c) => c.at > Date.now() - DAY).length;
  const dirty = repos.filter((r) => r.dirty > 0);
  return (
    <div className="mt-6 max-w-xl space-y-4">
      <p className="font-serif text-2xl italic leading-snug text-ink-2 sm:text-3xl">
        {down === 0
          ? tr("Tout le ciel est dégagé", "Clear skies everywhere")
          : tr(`${down} service${down > 1 ? "s" : ""} dans les nuages`, `${down} ${plural(down, ["service", "services"], ["service", "services"])} in the clouds`)}
        {" — "}
        {today
          ? tr(`${today} commit${today > 1 ? "s" : ""} en 24 h.`, `${today} ${plural(today, ["commit", "commits"], ["commit", "commits"])} in 24 h.`)
          : tr("aucun commit en 24 h.", "no commits in 24 h.")}
      </p>
      {probes.length > 0 && (
        <AnimatedShinyText className="mx-0 inline-flex items-center gap-2 text-sm">
          <LiveDot health={down === 0 ? "up" : "warn"} />
          {tr(`${up}/${probes.length} services répondent · mesuré chaque minute depuis ce Mac`, `${up}/${probes.length} services answering · checked every minute from this Mac`)}
        </AnimatedShinyText>
      )}
      {dirty.length > 0 && (
        <p className="text-sm text-ink-3">
          {tr("Travail non commité dans ", "Uncommitted work in ")}
          {dirty.map((r) => who(r.project).name).join(", ")}.
        </p>
      )}
    </div>
  );
}

async function SolarSystem() {
  const probes = await uptime();
  return (
    <Orbit
      planets={PROJECTS.map((p) => {
        const mine = probes.filter((u) => u.project === p.id);
        return {
          id: p.id,
          name: p.name,
          href: p.href,
          glow: p.glow,
          color: p.color,
          emoji: p.emoji,
          up: !mine.length || mine.some((u) => u.up == null) ? null : mine.every((u) => u.up),
        };
      })}
    />
  );
}

const XL = ["", "xl:grid-cols-1", "xl:grid-cols-2", "xl:grid-cols-3", "xl:grid-cols-4", "xl:grid-cols-5", "xl:grid-cols-6"];
const MD = ["", "md:grid-cols-1", "md:grid-cols-2", "md:grid-cols-3"];

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
  return (
    <div className={`mt-8 grid grid-cols-2 gap-6 rounded-3xl border border-line bg-white/[0.03] p-6 backdrop-blur-md ${MD[Math.min(3, n)]} ${XL[Math.min(6, n)]}`}>
      {tiles.map((k, i) => (
        <Stat key={`${k.label}${i}`} label={k.label} value={k.value} format={k.format as Format | undefined} suffix={k.suffix} hint={k.hint} color={k.project ? findProject(k.project)?.glow : undefined} />
      ))}
    </div>
  );
}

/* ——— Project cards ——— */

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

async function ProjectCard({ p, span, index }: { p: Project; span: string; index: number }) {
  const [list, probes, repos] = await Promise.all([stats(p), uptime(), allLocalRepos()]);
  const mine = probes.filter((u) => u.project === p.id);
  const up = mine.length ? mine.every((u) => u.up) : null;
  const repo = repos.find((r) => r.project === p.id);
  const series = repo ? weekly(repo.commits.map((c) => c.at), 12) : [];
  const max = Math.max(1, ...series.map((x) => x.value));
  return (
    <Link
      href={p.href}
      className={`group relative block overflow-hidden rounded-3xl border border-line bg-white/[0.03] p-6 transition duration-500 hover:-translate-y-1 hover:border-white/20 ${span}`}
      style={{ boxShadow: `0 30px 80px -40px ${p.glow}55` }}
    >
      <div aria-hidden className="absolute inset-0 opacity-70 transition-opacity duration-500 group-hover:opacity-100" style={{ background: `radial-gradient(90% 80% at 100% 0%, ${p.glow}30 0%, transparent 60%)` }} />
      {p.shot && (
        // eslint-disable-next-line @next/next/no-img-element
        <img
          src={`/api/shot/${p.id}`}
          alt=""
          className="pointer-events-none absolute -right-10 top-8 hidden w-[52%] rotate-[-6deg] rounded-xl border border-white/10 opacity-25 shadow-2xl transition duration-700 [mask-image:linear-gradient(to_bottom,black_30%,transparent_85%)] group-hover:rotate-[-3deg] group-hover:opacity-45 sm:block"
        />
      )}
      <BorderBeam size={90} duration={14} delay={index * 2.5} colorFrom={p.glow} colorTo={p.color} />
      <div className="relative">
        <div className="flex items-start justify-between gap-4">
          <div>
            <div className="text-3xl">{p.emoji}</div>
            <h3 className="mt-3 font-display text-2xl font-bold tracking-tight">{p.name}</h3>
            {p.tagline && <p className="font-serif text-lg italic text-ink-2">{p.tagline}</p>}
          </div>
          <ArrowUpRight className="size-5 text-ink-3 transition group-hover:-translate-y-0.5 group-hover:translate-x-0.5 group-hover:text-ink" />
        </div>
        {mine.length > 0 && (
          <div className="mt-4">
            <Status
              health={up == null ? "unknown" : up ? "up" : "down"}
              label={up == null ? tr("Mesure en cours", "Measuring…") : up ? tr(`En ligne · ${mine[0]?.last?.ms ?? "—"} ms`, `Online · ${mine[0]?.last?.ms ?? "—"} ms`) : tr("Hors ligne", "Offline")}
            />
          </div>
        )}
        {list.length > 0 && (
          <div className="mt-6 grid grid-cols-3 gap-3">
            {list.map((s) => (
              <div key={s.label}>
                <div className="font-display text-xl tabular">{s.value}</div>
                <div className="text-[11px] uppercase tracking-[0.14em] text-ink-3">{s.label}</div>
              </div>
            ))}
          </div>
        )}
        {series.length > 0 && (
          <div className="mt-5 flex h-10 items-end gap-[3px]" aria-label={tr("Commits par semaine, 12 semaines", "Commits per week, 12 weeks")}>
            {series.map((w, i) => (
              <div
                key={i}
                className="flex-1 rounded-t-[3px]"
                style={{ height: `${Math.max(4, (w.value / max) * 100)}%`, background: w.value ? p.color : "rgb(255 255 255 / .08)" }}
                title={tr(`${w.label} : ${w.value} commits`, `${w.label}: ${w.value} ${plural(w.value, ["commit", "commits"], ["commit", "commits"])}`)}
              />
            ))}
          </div>
        )}
      </div>
    </Link>
  );
}

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
      kicker={tr("Activité", "Activity")}
      title={tr("Six mois de code", "Six months of code")}
      action={<span className="text-xs text-ink-3">{tr(`${nf(total)} commits · série de ${streak} j`, `${nf(total)} ${plural(total, ["commit", "commits"], ["commit", "commits"])} · ${streak}-day streak`)}</span>}
    >
      <Heatmap days={days} />
      <div className="mt-8 border-t border-line pt-6">
        <div className="mb-3 text-[11px] uppercase tracking-[0.18em] text-ink-3">{tr("Commits par semaine et par projet", "Commits per week and project")}</div>
        <Bars data={weeks} height={140} legend={PROJECTS.map((p) => ({ name: p.name, color: p.color }))} />
      </div>
    </Panel>
  );
}

async function Feed() {
  const list = await events(22);
  return (
    <Panel kicker={tr("En direct", "Live")} title={tr("Le fil", "The feed")} action={<LiveDot />} bodyClassName={list.length ? "p-0" : undefined}>
      {list.length ? (
        <ol className="max-h-[640px] overflow-y-auto px-5 pb-5">
          {list.map((e, i) => {
            const p = who(e.project);
            const Icon = ICON[e.kind] ?? Sparkles;
            const body = (
              <>
                <span className="mt-0.5 grid size-7 shrink-0 place-items-center rounded-full" style={{ background: `${p.color}26` }}>
                  <Icon className="size-3.5" style={{ color: p.glow }} />
                </span>
                <div className="min-w-0 flex-1">
                  <div className="truncate text-sm text-ink">{e.text}</div>
                  <div className="text-xs text-ink-3">
                    {p.name} · {ago(e.at)}
                  </div>
                </div>
              </>
            );
            return (
              <li key={i} className="border-b border-line last:border-0">
                {e.href ? (
                  <a href={e.href} target="_blank" rel="noopener noreferrer" className="-mx-2 flex gap-3 rounded-xl px-2 py-3 hover:bg-white/[0.03]">
                    {body}
                  </a>
                ) : (
                  <div className="flex gap-3 py-3">{body}</div>
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

/* ——— Money ——— */

type Row = { label: string; value: number | null; currency: string; sign: 1 | -1; hint: string; color: string };

async function Money() {
  const withRailway = PROJECTS.filter((p) => p.railway);
  const [extra, rw, rates] = await Promise.all([
    collect((e) => e.money),
    withRailway.length ? source(billing) : null,
    SUBSCRIPTIONS.length ? fx() : null,
  ]);
  const base = l10n().currency;
  const rows: Row[] = extra.map((m) => ({ ...m, color: (m.project && findProject(m.project)?.color) || "#9C97AD" }));
  if (rw)
    rows.push({
      label: tr("Railway · période en cours", "Railway · current period"),
      value: rw.ok ? rw.data.currentUsage : null,
      currency: "USD",
      sign: -1,
      hint: rw.ok && rw.data.period ? tr(`facturé le ${date(rw.data.period.end, { day: "numeric", month: "long" })}`, `billed on ${date(rw.data.period.end, { day: "numeric", month: "long" })}`) : tr("Railway à brancher", "Connect Railway"),
      color: "#9C97AD",
    });
  if (rates) {
    // Railway has its own live row: don't count it twice among the subscriptions.
    const subs = SUBSCRIPTIONS.filter((s) => (s.status === "active" || s.status === "failing") && !(rw && s.vendor === "Railway"));
    const toBase = rates.toBase;
    const total = subs.reduce((a, s) => a + monthly(s) * (s.currency.toUpperCase() === base.toUpperCase() ? 1 : (toBase[s.currency.toUpperCase()] ?? 1)), 0);
    rows.push({
      label: tr("Abonnements · par mois", "Subscriptions · per month"),
      value: total,
      currency: base,
      sign: -1,
      hint: tr(`${subs.length} abonnements · détail dans Abonnements`, `${subs.length} ${plural(subs.length, ["subscription", "subscriptions"], ["subscription", "subscriptions"])} · details in Subscriptions`),
      color: AGENTS.claude.color,
    });
  }
  if (!rows.length) return null;
  const fmt = (v: number, c: string) => new Intl.NumberFormat(l10n().locale, { style: "currency", currency: c, maximumFractionDigits: 2 }).format(v);
  const share = rw?.ok ? withRailway.flatMap((p) => (p.railway && rw.data.byProject[p.railway.projectId] ? [{ p, value: rw.data.byProject[p.railway.projectId] }] : [])) : [];
  return (
    <Panel kicker={tr("L'argent", "Money")} title={tr("Ce qui rentre, ce qui sort", "What comes in, what goes out")}>
      <ul className="space-y-4">
        {rows.map((r) => (
          <li key={r.label} className="flex items-center gap-3">
            <span className="grid size-8 shrink-0 place-items-center rounded-full text-sm font-semibold" style={{ background: `${r.color}26`, color: r.sign > 0 ? "var(--good)" : "var(--ink-2)" }}>
              {r.sign > 0 ? "↓" : "↑"}
            </span>
            <div className="min-w-0 flex-1">
              <div className="truncate text-sm text-ink">{r.label}</div>
              <div className="truncate text-xs text-ink-3">{r.hint}</div>
            </div>
            <span className={`font-display text-lg tabular ${r.sign > 0 && r.value ? "text-good" : "text-ink"}`}>
              {r.value == null ? "—" : `${r.sign > 0 ? "+" : "−"}${fmt(r.value, r.currency)}`}
            </span>
          </li>
        ))}
      </ul>
      {share.length > 0 && (
        <div className="mt-6 border-t border-line pt-4">
          <div className="mb-2 text-[11px] uppercase tracking-[0.18em] text-ink-3">{tr("Railway par projet · projection sur la période", "Railway per project · projected over the period")}</div>
          <div className="flex h-2 overflow-hidden rounded-full bg-white/[0.05]">
            {share.map(({ p, value }) => (
              <div key={p.id} className="h-full border-r-2 border-[#0b0a14] last:border-0" style={{ flexGrow: value, background: p.color }} title={tr(`${p.name} : ${fmt(value, "USD")}`, `${p.name}: ${fmt(value, "USD")}`)} />
            ))}
          </div>
          <div className="mt-2 flex flex-wrap gap-x-4 gap-y-1 text-xs text-ink-3">
            {share.map(({ p, value }) => (
              <span key={p.id} className="inline-flex items-center gap-1.5">
                <span className="size-2 rounded-[3px]" style={{ background: p.color }} />
                {p.name} <span className="font-mono text-ink-2">{fmt(value, "USD")}</span>
              </span>
            ))}
          </div>
        </div>
      )}
    </Panel>
  );
}

async function LiveAgents() {
  const all = await source(sessions);
  return (
    <Panel
      kicker={tr("Agents IA", "AI agents")}
      title={tr("Qui bosse en ce moment", "Who's working right now")}
      action={
        <Link href="/agents" className="text-xs text-ink-3 hover:text-ink">
          {tr("Tout voir →", "See all →")}
        </Link>
      }
    >
      <Gate src={all}>
        {(list) => {
          const live = list.filter(isLive);
          const shown = live.length ? live : list.slice(0, 4);
          const n = live.length;
          return (
            <>
              <p className="mb-2 text-xs text-ink-3">
                {n
                  ? tr(`${n} session${n > 1 ? "s" : ""} active${n > 1 ? "s" : ""}`, `${n} active ${plural(n, ["session", "sessions"], ["session", "sessions"])}`)
                  : tr("Aucune session active — les dernières :", "No active session — the latest:")}
              </p>
              <SessionList rows={toRows(shown.slice(0, 5))} />
            </>
          );
        }}
      </Gate>
    </Panel>
  );
}

/** Failing payments: what breaks soon if nobody deals with it. */
function Urgent() {
  const list = urgent();
  if (!list.length) return null;
  const n = list.length;
  return (
    <Link href="/abonnements" className="mt-4 flex items-start gap-3 rounded-2xl border border-bad/30 bg-bad/[0.06] px-4 py-3 text-sm transition hover:border-bad/50">
      <CircleAlert className="mt-0.5 size-4 shrink-0 text-bad" />
      <div className="min-w-0">
        <span className="font-medium text-ink">{tr(`${n} paiement${n > 1 ? "s" : ""} en échec · `, `${n} failed ${plural(n, ["payment", "payments"], ["payment", "payments"])} · `)}</span>
        <span className="text-ink-2">{list.map((s) => s.name).join(", ")}.</span>{" "}
        {list[0].evidence && <span className="text-ink-3">{list[0].evidence}.</span>}
      </div>
      <ArrowUpRight className="ml-auto size-4 shrink-0 text-ink-3" />
    </Link>
  );
}

/** Today in one line: weather, next appointment, what's waiting. */
async function Today() {
  const [w, l] = await Promise.all([source(weather), source(life)]);
  const snap = l.ok ? l.data : null;
  const next = snap?.agenda.find((e) => new Date(e.end ?? e.start).getTime() > Date.now());
  const pending = agentUi().enabled ? (await waiting().catch(() => [])).length : (snap?.inbox.needsReply.length ?? 0) + urgent().length;
  if (!w.ok && !next && !pending) return null;
  const { locale, timeZone } = l10n();
  const hm = (t: string) => new Intl.DateTimeFormat(locale, { timeZone, weekday: "short", hour: "2-digit", minute: "2-digit" }).format(new Date(t));
  return (
    <Link href="/vie" className="group mt-6 flex max-w-xl flex-wrap items-center gap-x-5 gap-y-2 rounded-2xl border border-line bg-white/[0.03] px-4 py-3 text-sm backdrop-blur transition hover:border-white/20">
      {w.ok && (
        <span className="inline-flex items-center gap-2">
          <span className="text-xl">{describe(w.data.now.code, w.data.now.isDay).icon}</span>
          <span className="font-display tabular text-ink">{Math.round(w.data.now.temp)}°</span>
          {w.data.days[0] && (
            <span className="text-ink-3">
              {Math.round(w.data.days[0].min)}° / {Math.round(w.data.days[0].max)}°
            </span>
          )}
        </span>
      )}
      {next && (
        <span className="min-w-0 truncate text-ink-2">
          📅 {next.title} <span className="text-ink-3">· {next.allDay ? tr("journée", "all day") : hm(next.start)}</span>
        </span>
      )}
      {pending > 0 && (
        <span className="text-ink-2">
          ⚡ {tr(`${pending} chose${pending > 1 ? "s" : ""} t'attend${pending > 1 ? "ent" : ""}`, `${pending} ${plural(pending, ["thing", "things"], ["thing", "things"])} waiting for you`)}
        </span>
      )}
      <ArrowUpRight className="ml-auto size-4 text-ink-3 transition group-hover:text-ink" />
    </Link>
  );
}

/* ——— First run: no project configured ——— */

function Welcome() {
  const { meta } = config();
  const steps = [
    { n: "1", text: tr("Copie le fichier d'exemple à la racine de zenith :", "Copy the example file at the root of zenith:"), code: "cp zenith.config.example.json zenith.config.json" },
    { n: "2", text: tr("Remplace les projets d'exemple par les tiens : nom, dossier local, dépôt GitHub, site à surveiller.", "Replace the sample projects with yours: name, local folder, GitHub repository, site to watch.") },
    { n: "3", text: tr("Relance zenith : tout se remplit, sans base de données ni compte.", "Restart zenith: everything fills in, no database, no account.") },
  ];
  const works = [
    { href: "/agents", icon: Bot, name: tr("Agents IA", "AI agents"), hint: tr("tes sessions Claude Code et Codex", "your Claude Code and Codex sessions"), glow: "#D4724F" },
    ...(config().location ? [{ href: "/vie", icon: Sun, name: tr("Ma vie", "My life"), hint: tr(`météo à ${config().location?.name}`, `weather in ${config().location?.name}`), glow: "#FDBA74" }] : []),
    ...(config().code.enabled ? [{ href: "/code", icon: SquareTerminal, name: "zenith code", hint: tr("coder avec les agents", "code with agents"), glow: "#A5B4FC" }] : []),
    { href: "/veille", icon: Radar, name: tr("Veille", "Radar"), hint: tr("actualité et ce Mac", "news and this Mac"), glow: "#7dd3fc" },
    { href: "/abonnements", icon: Wallet, name: tr("Abonnements", "Subscriptions"), hint: tr("limites IA et frais", "AI limits and fees"), glow: "#34d399" },
  ];
  return (
    <>
      <section className="mt-4 grid items-center gap-8 lg:grid-cols-[1.1fr_1fr]">
        <div>
          <Greeting name={OWNER.firstName} place={OWNER.place} />
          <p className="mt-6 max-w-xl font-serif text-2xl italic leading-snug text-ink-2 sm:text-3xl">
            {meta.found
              ? tr("Ton ciel est encore vide : ajoute un premier projet.", "Your sky is still empty: add a first project.")
              : tr("Bienvenue dans zenith. Ton ciel attend ses premières planètes.", "Welcome to zenith. Your sky is waiting for its first planets.")}
          </p>
          <Suspense fallback={<div className="mt-6 h-20" />}>
            <Today />
          </Suspense>
        </div>
        <Orbit planets={[]} />
      </section>

      <section className="relative mt-8 overflow-hidden rounded-[2rem] border border-line px-6 py-8 sm:px-10 sm:py-10">
        <div aria-hidden className="absolute inset-0" style={{ background: "radial-gradient(120% 140% at 0% 0%, #FFD16626 0%, transparent 55%), radial-gradient(80% 120% at 100% 100%, #B18CFF22 0%, transparent 60%)" }} />
        <BorderBeam size={140} duration={12} colorFrom="#FFD166" colorTo="#B18CFF" />
        <div className="relative grid gap-10 lg:grid-cols-[1.3fr_1fr]">
          <div>
            <div className="mb-2 inline-flex items-center gap-2 text-[11px] font-medium uppercase tracking-[0.2em] text-ink-3">
              <FileJson className="size-3.5 text-sun" /> zenith.config.json
            </div>
            <h2 className="font-display text-3xl font-bold tracking-tight sm:text-4xl">{tr("Un seul fichier, tout ton ciel", "One file, your whole sky")}</h2>
            <p className="mt-3 max-w-xl text-ink-2">
              {tr(
                "zenith tourne en local et ne lit tes données que dans un fichier : tes projets, ta ville, ta devise, tes abonnements. Il n'est jamais publié.",
                "zenith runs locally and reads your data from a single file: your projects, your city, your currency, your subscriptions. It is never published.",
              )}
            </p>
            {meta.error && (
              <div className="mt-5 flex items-start gap-3 rounded-2xl border border-bad/30 bg-bad/5 p-4 text-sm text-ink-2">
                <CircleAlert className="mt-0.5 size-4 shrink-0 text-bad" />
                <div className="min-w-0 break-words">
                  {tr("Le fichier n'a pas pu être lu : ", "The file could not be read: ")}
                  <code className="font-mono text-xs text-bad">{meta.error}</code>
                </div>
              </div>
            )}
            <ol className="mt-6 space-y-4">
              {steps.map((s) => (
                <li key={s.n} className="flex gap-4">
                  <span className="grid size-7 shrink-0 place-items-center rounded-full border border-sun/40 bg-sun/10 font-mono text-xs text-sun">{s.n}</span>
                  <div className="min-w-0 flex-1 pt-0.5 text-sm text-ink-2">
                    {s.text}
                    {s.code && <pre className="mt-2 overflow-x-auto rounded-xl border border-line bg-black/40 px-3 py-2 font-mono text-xs text-ink">{s.code}</pre>}
                  </div>
                </li>
              ))}
            </ol>
            <div className="mt-7 flex flex-wrap items-start gap-3">
              {agentUi().enabled && <AskButton prompt={setupPrompt(config().projectsRoot)} target="zenith" label={tr("Laisser zenith se configurer", "Let zenith set itself up")} />}
              <Link href="/reglages" className="inline-flex items-center gap-2 rounded-full border border-sun/40 bg-sun/10 px-4 py-2 text-sm text-ink transition hover:border-sun/70">
                <Settings2 className="size-4 text-sun" /> {tr("Ouvrir les réglages", "Open settings")}
              </Link>
              <span className="self-center font-mono text-xs text-ink-3">{meta.file}</span>
            </div>
          </div>
          <div>
            <div className="mb-3 text-[11px] font-medium uppercase tracking-[0.2em] text-ink-3">{tr("Ce qui marche déjà", "Already working")}</div>
            <ul className="space-y-2">
              {works.map((w) => (
                <li key={w.href}>
                  <Link href={w.href} className="group flex items-center gap-3 rounded-2xl border border-line bg-white/[0.03] px-4 py-3 transition hover:border-white/20">
                    <span className="grid size-8 shrink-0 place-items-center rounded-xl" style={{ background: `${w.glow}22`, color: w.glow }}>
                      <w.icon className="size-4" />
                    </span>
                    <div className="min-w-0 flex-1">
                      <div className="text-sm text-ink">{w.name}</div>
                      <div className="truncate text-xs text-ink-3">{w.hint}</div>
                    </div>
                    <ArrowUpRight className="size-4 text-ink-3 transition group-hover:text-ink" />
                  </Link>
                </li>
              ))}
            </ul>
          </div>
        </div>
      </section>

      <div className="mt-8">
        <Suspense fallback={<Skeleton className="h-80" />}>
          <LiveAgents />
        </Suspense>
      </div>
    </>
  );
}
