import { Suspense } from "react";
import type { Metadata } from "next";
import { CircleCheck, CircleDot, CircleX, GitPullRequest, Info, MessageSquare, Package, Server, ShieldAlert, Star, Tag } from "lucide-react";
import { PROJECTS } from "@/lib/projects";
import { source } from "@/lib/source";
import { ago, money, nf, pct } from "@/lib/format";
import { plural, tr } from "@/lib/i18n";
import { config } from "@/lib/config";
import { hackerNews, hnMentions, localNews, WATCH, type Headline } from "@/lib/sources/news";
import { githubMentions, groupNotifications, notifications, profile, recentStars, type Notification } from "@/lib/sources/github";
import { brewOutdated, devServers, machine } from "@/lib/sources/machine";
import { crypto } from "@/lib/sources/markets";
import { fx } from "@/lib/sources/plans";
import { agentUi } from "@/lib/agent/ui";
import { AskButton } from "@/components/agent/ask-button";
import { Empty, PageHeader, Panel, SectionTitle, Skeleton } from "@/components/z/panel";
import { Gate } from "@/components/z/gate";
import { Stat } from "@/components/z/stat";
import { Heatmap } from "@/components/charts/heatmap";
import { Counted, Row, Rows } from "@/components/life/rows";
import { ciPrompt, mentionPrompt } from "@/components/life/prompts";
import { cn } from "@/lib/utils";

export const dynamic = "force-dynamic";
export function generateMetadata(): Metadata {
  return { title: tr("Veille", "Watch") };
}

export default function Veille() {
  const agent = agentUi().enabled;
  return (
    <>
      <PageHeader
        title={tr("Veille", "Watch")}
        description={tr(
          "Qui parle de tes projets, ce qui t'attend sur GitHub, l'actualité, les marchés et l'état de ce Mac.",
          "Who talks about your projects, what waits on GitHub, the news, the markets and the state of this Mac.",
        )}
      />

      <div className="grid items-start gap-4 lg:grid-cols-2">
        <Suspense fallback={<Skeleton className="h-96" />}>
          <GitHubInbox agent={agent} />
        </Suspense>
        <div className="grid gap-4">
          <Suspense fallback={<Skeleton className="h-40" />}>
            <Mentions agent={agent} />
          </Suspense>
          <Suspense fallback={<Skeleton className="h-64" />}>
            <Contributions />
          </Suspense>
          <Suspense fallback={<Skeleton className="h-40" />}>
            <Stars />
          </Suspense>
        </div>
      </div>

      <SectionTitle>{tr("Le monde", "The world")}</SectionTitle>
      <Suspense fallback={<Skeleton className="h-20" />}>
        <Markets />
      </Suspense>
      <div className="mt-4 grid items-start gap-4 lg:grid-cols-2">
        <Suspense fallback={<Skeleton className="h-96" />}>
          <News />
        </Suspense>
        <Suspense fallback={<Skeleton className="h-96" />}>
          <HackerNews />
        </Suspense>
      </div>

      <SectionTitle>{tr("Ce Mac", "This Mac")}</SectionTitle>
      <Suspense fallback={<Skeleton className="h-72" />}>
        <ThisMac />
      </Suspense>
    </>
  );
}

const ListBody = "p-0 pb-1.5 pt-1";

/** Mentions of your projects outside your own repositories: Hacker News and GitHub. */
async function Mentions({ agent }: { agent: boolean }) {
  const title = (n?: number) => <Counted count={n}>{tr("On parle de toi", "People talk about you")}</Counted>;
  if (!WATCH.length)
    return (
      <Panel title={title()} action="Hacker News · GitHub">
        <Empty>
          <span>
            {tr("Ajoute les mots qui te désignent, toi ou tes projets, sous ", "Add the words that mean you or your projects under ")}
            <code className="font-mono text-xs text-ink">watch</code>
            {tr(" dans ", " in ")}
            <code className="font-mono text-xs">zenith.config.json</code>.
          </span>
        </Empty>
      </Panel>
    );
  const [hn, gh] = await Promise.all([source(hnMentions), source(() => githubMentions(WATCH.map((w) => w.term)))]);
  const items = [
    ...(hn.ok ? hn.data.map((m) => ({ key: m.url, term: m.term, where: m.where, who: m.label, title: m.title, excerpt: m.excerpt, url: m.url, at: m.at, pr: false })) : []),
    ...(gh.ok
      ? gh.data.map((m) => ({ key: m.url, term: m.term, where: `GitHub · ${m.repo}`, who: WATCH.find((w) => w.term === m.term)?.label ?? m.term, title: m.title, excerpt: "", url: m.url, at: m.at, pr: m.kind === "pr" }))
      : []),
  ]
    .filter((m, i, all) => all.findIndex((x) => x.key === m.key) === i)
    .sort((a, b) => b.at.localeCompare(a.at));
  // The project a mention is about, when its word or label names one.
  const projectOf = (m: { term: string; who: string }) =>
    PROJECTS.find((p) => [p.id, p.name].some((x) => x.toLowerCase() === m.term || x.toLowerCase() === m.who.toLowerCase()))?.id;
  return (
    <Panel title={title(items.length)} action="Hacker News · GitHub" bodyClassName={ListBody}>
      {!hn.ok && !gh.ok ? (
        <div className="px-4 pb-3">
          <Gate src={hn}>{() => null}</Gate>
        </div>
      ) : items.length ? (
        <Rows>
          {items.slice(0, 10).map((m) => (
            <Row
              key={m.key}
              icon={m.where === "Hacker News" ? MessageSquare : m.pr ? GitPullRequest : CircleDot}
              title={m.title}
              href={m.url}
              meta={
                <>
                  {m.who} · {m.where}
                  {m.excerpt && <span className="text-ink-3"> · {m.excerpt}</span>}
                </>
              }
              aside={ago(m.at)}
              action={agent && <AskButton prompt={mentionPrompt(m)} target={projectOf(m)} label={tr("Prépare une réponse", "Draft a reply")} />}
            />
          ))}
        </Rows>
      ) : (
        <div className="px-4 pb-3">
          <Empty>
            {tr(
              `Personne ne parle encore de ${WATCH.map((w) => w.term).join(", ")} sur Hacker News ni dans les issues GitHub des autres (90 jours).`,
              `Nobody mentions ${WATCH.map((w) => w.term).join(", ")} on Hacker News or in other people's GitHub issues yet (90 days).`,
            )}
          </Empty>
        </div>
      )}
    </Panel>
  );
}

const REASON = (): Record<string, string> => ({
  review_requested: tr("revue demandée", "review requested"),
  mention: "mention",
  team_mention: tr("mention d'équipe", "team mention"),
  author: tr("tu es l'auteur", "you are the author"),
  comment: tr("commentaire", "comment"),
  assign: tr("assigné", "assigned"),
  ci_activity: "CI",
  state_change: tr("changement d'état", "state change"),
  subscribed: tr("abonné", "subscribed"),
  security_alert: tr("alerte de sécurité", "security alert"),
  manual: tr("suivi", "watching"),
});

const failed = (n: Notification) => n.reason === "ci_activity" && /fail/i.test(n.title);
const projectOfRepo = (repo: string) => PROJECTS.find((p) => p.repo?.toLowerCase() === repo.toLowerCase()) ?? null;

/** Unread GitHub notifications, grouped by repository, CI failures first with a one-click fix. */
async function GitHubInbox({ agent }: { agent: boolean }) {
  const n = await source(notifications);
  const reason = REASON();
  const count = n.ok ? n.data.length : undefined;
  return (
    <Panel title={<Counted count={count}>{tr("Notifications GitHub", "GitHub notifications")}</Counted>} action={tr("non lues", "unread")} bodyClassName={ListBody}>
      <Gate src={n}>
        {(list) => {
          if (!list.length)
            return (
              <div className="px-4 pb-3">
                <Empty>{tr("Aucune notification non lue.", "No unread notification.")}</Empty>
              </div>
            );
          const byRepo = new Map<string, ReturnType<typeof groupNotifications>>();
          for (const x of groupNotifications(list)) byRepo.set(x.repo, [...(byRepo.get(x.repo) ?? []), x]);
          // Repositories with a failing workflow first, then the most recent.
          const repos = [...byRepo.entries()].sort(([, a], [, b]) => Number(b.some(failed)) - Number(a.some(failed)) || b[0].at.localeCompare(a[0].at));
          return repos.map(([repo, items], i) => {
            const proj = projectOfRepo(repo);
            const failures = items.filter(failed);
            // A failing default branch is what matters; old tags come after.
            const main = failures.filter((f) => /\b(main|master)\b/i.test(f.title));
            return (
              <div key={repo} className={cn(i > 0 && "border-t border-line")}>
                <div className="flex h-10 items-center gap-2.5 px-4">
                  <span className="size-2 shrink-0 rounded-full" style={{ background: proj?.color ?? "var(--ink-3)" }} />
                  <a href={`https://github.com/${repo}`} target="_blank" rel="noopener noreferrer" className="min-w-0 truncate text-[13px] font-medium text-ink hover:underline">
                    {proj?.name ?? repo.split("/")[1]}
                  </a>
                  <span className="truncate text-xs text-ink-3 max-sm:hidden">{repo}</span>
                  <span className="text-xs text-ink-3 tabular">{items.reduce((a, x) => a + x.count, 0)}</span>
                  <span className="flex-1" />
                  {failures.length > 0 && (
                    <span className={cn("shrink-0 text-xs", main.length ? "text-bad" : "text-ink-3")}>
                      {main.length ? tr("CI principale en échec", "Main CI failing") : `${failures.length} ${plural(failures.length, ["échec", "échecs"], ["failure", "failures"])}`}
                    </span>
                  )}
                  {agent && failures.length > 0 && (
                    <AskButton prompt={ciPrompt(repo, (main.length ? [...main, ...failures.filter((f) => !main.includes(f))] : failures).slice(0, 8))} target={proj?.id} label={tr("Répare", "Fix")} />
                  )}
                </div>
                <Rows className="border-t border-line">
                  {items.slice(0, 6).map((x) => {
                    const bad = failed(x);
                    const Icon = bad ? CircleX : x.reason === "ci_activity" ? CircleCheck : x.reason === "security_alert" ? ShieldAlert : x.type === "PullRequest" ? GitPullRequest : x.type === "Release" ? Tag : x.type === "Issue" ? CircleDot : Info;
                    return (
                      <Row
                        key={x.id}
                        icon={Icon}
                        tone={bad && /\b(main|master)\b/i.test(x.title) ? "text-bad" : x.reason === "security_alert" ? "text-warn" : undefined}
                        title={x.title}
                        href={x.url}
                        meta={`${reason[x.reason] ?? x.reason}${x.count > 1 ? tr(` · ${x.count} fois`, ` · ${x.count} times`) : ""}`}
                        aside={ago(x.at)}
                      />
                    );
                  })}
                  {items.length > 6 && <li className="flex h-8 items-center px-4 pl-11 text-xs text-ink-3">{tr(`et ${items.length - 6} de plus`, `and ${items.length - 6} more`)}</li>}
                </Rows>
              </div>
            );
          });
        }}
      </Gate>
    </Panel>
  );
}

async function Contributions() {
  const p = await source(profile);
  return (
    <Panel title={tr("Contributions", "Contributions")} action={p.ok ? `@${p.data.login}` : "GitHub"}>
      <Gate src={p}>
        {(g) => (
          <>
            <div className="grid grid-cols-3 gap-4 sm:grid-cols-5">
              <Stat label={tr("12 mois", "12 months")} value={g.contributions} />
              <Stat label={tr("7 jours", "7 days")} value={g.days.slice(-7).reduce((a, d) => a + d.count, 0)} />
              <Stat label={tr("Série", "Streak")} value={g.streak} suffix={tr(" j", " d")} hint={g.streak ? undefined : tr("à relancer aujourd'hui", "restart it today")} />
              <Stat label={tr("Étoiles", "Stars")} value={g.stars} hint={tr(`${g.repos} dépôts`, `${g.repos} repositories`)} />
              <Stat label={tr("Abonnés", "Followers")} value={g.followers} />
            </div>
            <div className="mt-5 border-t border-line pt-4">
              <Heatmap days={g.days.slice(-182).map((d) => ({ t: Date.parse(`${d.date}T00:00:00Z`), total: d.count, parts: [{ name: "Contributions", color: "var(--primary)", value: d.count }] }))} />
            </div>
          </>
        )}
      </Gate>
    </Panel>
  );
}

async function Stars() {
  const repos = PROJECTS.flatMap((p) => (p.repo ? [p.repo] : []));
  const s = await source(() => recentStars(repos));
  const count = s.ok ? s.data.length : undefined;
  return (
    <Panel title={<Counted count={count}>{tr("Nouvelles étoiles", "New stars")}</Counted>} action={tr("30 jours", "30 days")} bodyClassName={ListBody}>
      <Gate src={s}>
        {(list) =>
          list.length ? (
            <Rows>
              {list.slice(0, 8).map((x) => {
                const proj = projectOfRepo(x.repo);
                return (
                  <Row
                    key={x.repo + x.user}
                    icon={Star}
                    title={x.user}
                    href={x.url}
                    meta={
                      <span className="inline-flex items-center gap-1.5">
                        {proj && <span className="size-1.5 rounded-full" style={{ background: proj.color }} />}
                        {proj?.name ?? x.repo.split("/")[1]}
                      </span>
                    }
                    aside={ago(x.at)}
                  />
                );
              })}
            </Rows>
          ) : (
            <div className="px-4 pb-3">
              <Empty>{tr("Pas de nouvelle étoile ce mois-ci.", "No new star this month.")}</Empty>
            </div>
          )
        }
      </Gate>
    </Panel>
  );
}

function Headlines({ list, hn }: { list: Headline[]; hn?: boolean }) {
  return (
    <Rows>
      {list.map((h) => (
        <li key={h.url} className="flex min-h-10 items-center gap-3 px-4 py-2 transition-colors hover:bg-hover">
          <div className="min-w-0 flex-1">
            <a href={h.url} target="_blank" rel="noopener noreferrer" className="line-clamp-1 text-[13px] text-ink underline-offset-2 hover:underline" title={h.title}>
              {h.title}
            </a>
            <div className="flex gap-2 text-xs text-ink-3">
              {!hn && <span className="truncate">{h.source}</span>}
              {hn && h.score != null && <span className="tabular">{h.score} {plural(h.score, ["point", "points"], ["point", "points"])}</span>}
              {hn && h.discussion && (
                <a href={h.discussion} target="_blank" rel="noopener noreferrer" className="tabular hover:text-ink">
                  {h.comments ?? 0} {plural(h.comments ?? 0, ["commentaire", "commentaires"], ["comment", "comments"])}
                </a>
              )}
            </div>
          </div>
          {h.at && <span className="shrink-0 text-xs text-ink-3 tabular">{ago(h.at)}</span>}
        </li>
      ))}
    </Rows>
  );
}

async function News() {
  const where = config().location?.name ?? null;
  const title = (n?: number) => <Counted count={n}>{tr("Actualité", "News")}</Counted>;
  if (!config().news.length)
    return (
      <Panel title={title()}>
        <Empty>
          <span>
            {tr("Ajoute tes flux RSS sous ", "Add your RSS feeds under ")}
            <code className="font-mono text-xs text-ink">news</code>
            {tr(" dans ", " in ")}
            <code className="font-mono text-xs">zenith.config.json</code>.
          </span>
        </Empty>
      </Panel>
    );
  const n = await source(localNews);
  return (
    <Panel title={title(n.ok ? Math.min(n.data.length, 12) : undefined)} action={where} bodyClassName={ListBody}>
      <Gate src={n}>
        {(list) =>
          list.length ? (
            <Headlines list={list.slice(0, 12)} />
          ) : (
            <div className="px-4 pb-3">
              <Empty>{tr("Pas d'article.", "No article.")}</Empty>
            </div>
          )
        }
      </Gate>
    </Panel>
  );
}

async function HackerNews() {
  const n = await source(hackerNews);
  return (
    <Panel title={<Counted count={n.ok ? n.data.length : undefined}>Hacker News</Counted>} action={tr("à la une", "front page")} bodyClassName={ListBody}>
      <Gate src={n}>{(list) => <Headlines list={list.slice(0, 12)} hn />}</Gate>
    </Panel>
  );
}

/** Exchange rates and crypto, in one strip. */
async function Markets() {
  const [q, rates] = await Promise.all([source(crypto), fx()]);
  const cur = rates.base;
  // Two reference currencies other than yours.
  const refs = ["EUR", "USD", "GBP"].filter((c) => c !== cur && rates.toBase[c]).slice(0, 2);
  const usdTo = rates.toBase.USD;
  const cells = [
    ...refs.map((c) => ({ key: c, label: `1 ${c}`, value: money(rates.toBase[c], cur, 4), change: null as number | null })),
    ...(q.ok ? q.data : []).map((x) => ({
      key: x.symbol,
      label: x.name,
      value: usdTo ? money(x.usd * usdTo, cur, x.usd > 1000 ? 0 : 2) : money(x.usd, "USD", x.usd > 1000 ? 0 : 2),
      change: x.change,
    })),
  ];
  return (
    <section className="overflow-hidden rounded-xl border border-line bg-surface">
      <div className="grid grid-cols-2 divide-line sm:grid-cols-3 lg:grid-cols-5 lg:divide-x">
        {cells.map((c) => (
          <div key={c.key} className="min-w-0 px-4 py-3">
            <div className="truncate text-xs text-ink-3">{c.label}</div>
            <div className="mt-0.5 flex items-baseline gap-2">
              <span className="truncate text-[15px] font-semibold text-ink tabular">{c.value}</span>
              {c.change != null && (
                <span className={cn("shrink-0 text-xs tabular", c.change >= 0 ? "text-good" : "text-bad")}>
                  {c.change >= 0 ? "+" : ""}
                  {pct(c.change, 1)}
                </span>
              )}
            </div>
          </div>
        ))}
      </div>
      <div className="flex flex-wrap items-center justify-between gap-2 border-t border-line px-4 py-2 text-2xs text-ink-3">
        <span>
          {rates.date ? tr(`Change BCE du ${rates.date} (Frankfurter)`, `ECB rates of ${rates.date} (Frankfurter)`) : tr("Change approximatif (Frankfurter injoignable)", "Approximate rates (Frankfurter unreachable)")}
          {tr(" · cryptos Kraken, variation depuis minuit UTC", " · crypto from Kraken, change since midnight UTC")}
        </span>
        {!q.ok && <Gate src={q} compact>{() => null}</Gate>}
      </div>
    </section>
  );
}

const duration = (s: number) => {
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  return d ? tr(`${d} j ${h} h`, `${d} d ${h} h`) : `${h} h ${Math.floor((s % 3600) / 60)} min`;
};

async function ThisMac() {
  const [m, servers, brew] = await Promise.all([source(machine), source(devServers), source(brewOutdated)]);
  return (
    <div className="grid items-start gap-4 lg:grid-cols-3">
      <Panel title={m.ok ? `${m.data.chip} · macOS ${m.data.macos}` : tr("Ce Mac", "This Mac")}>
        <Gate src={m}>
          {(x) => {
            const usedDisk = 1 - x.disk.freeGB / x.disk.totalGB;
            const bars = [
              { label: tr("Disque", "Disk"), used: usedDisk, text: tr(`${Math.round(x.disk.freeGB)} Go libres sur ${Math.round(x.disk.totalGB)}`, `${Math.round(x.disk.freeGB)} GB free of ${Math.round(x.disk.totalGB)}`) },
              ...(x.memoryFree != null ? [{ label: tr("Mémoire", "Memory"), used: 1 - x.memoryFree / 100, text: tr(`${x.memoryFree} % libre sur ${x.memoryGB} Go`, `${x.memoryFree}% free of ${x.memoryGB} GB`) }] : []),
              { label: tr("Charge", "Load"), used: Math.min(1, x.load / x.cores), text: tr(`${nf(x.load, 1)} sur ${x.cores} cœurs`, `${nf(x.load, 1)} on ${x.cores} cores`) },
              ...(x.battery ? [{ label: tr("Batterie", "Battery"), used: x.battery.percent / 100, text: `${x.battery.percent}${tr(" %", "%")} · ${x.battery.charging ? tr("en charge", "charging") : tr(`sur ${x.battery.source}`, `on ${x.battery.source}`)}`, battery: true }] : []),
            ];
            return (
              <>
              <ul className="space-y-3.5">
                {bars.map((b) => {
                  // Full is bad for disk, memory and load; empty is bad for a battery.
                  const level = "battery" in b ? 1 - b.used : b.used;
                  return (
                    <li key={b.label}>
                      <div className="mb-1 flex items-baseline justify-between gap-3 text-xs">
                        <span className="text-ink-2">{b.label}</span>
                        <span className="truncate text-ink-3 tabular">{b.text}</span>
                      </div>
                      <div className="h-1.5 overflow-hidden rounded-full bg-muted">
                        <div className={cn("h-full rounded-full", level > 0.9 ? "bg-bad" : level > 0.75 ? "bg-warn" : "bg-ink-3")} style={{ width: `${Math.max(2, Math.round(b.used * 100))}%` }} />
                      </div>
                    </li>
                  );
                })}
              </ul>
              <p className="mt-4 text-2xs text-ink-3">{tr(`Allumé depuis ${duration(x.uptime)} · ${x.model}`, `Up for ${duration(x.uptime)} · ${x.model}`)}</p>
              </>
            );
          }}
        </Gate>
      </Panel>

      <Panel title={<Counted count={servers.ok ? servers.data.length : undefined}>{tr("Serveurs de dev", "Dev servers")}</Counted>} action={tr("en marche", "running")} bodyClassName={ListBody}>
        {servers.ok && servers.data.length ? (
          <Rows>
            {servers.data.map((s) => (
              <Row
                key={s.port}
                icon={Server}
                title={
                  <>
                    <span className="tabular text-ink-2">:{s.port}</span> <span>{s.project ?? s.dir?.split("/").pop() ?? s.command}</span>
                  </>
                }
                href={`http://127.0.0.1:${s.port}`}
                aside={<span className="font-mono">{s.command}</span>}
              />
            ))}
          </Rows>
        ) : (
          <p className="px-4 pb-3 text-[13px] text-ink-3">{tr("Aucun serveur de dev en marche.", "No dev server running.")}</p>
        )}
      </Panel>

      <Panel
        title={<Counted count={brew.ok && brew.data ? brew.data.length : undefined}>{tr("Mises à jour Homebrew", "Homebrew updates")}</Counted>}
        action={brew.ok && brew.data?.length ? <code className="rounded-md bg-muted px-1.5 py-0.5 font-mono text-2xs text-ink-2">brew upgrade</code> : null}
        bodyClassName={ListBody}
      >
        {brew.ok && brew.data ? (
          brew.data.length ? (
            <Rows>
              {brew.data.slice(0, 10).map((b) => (
                <Row
                  key={b.name}
                  icon={Package}
                  title={b.name}
                  aside={
                    <span className="font-mono">
                      {b.current} → <span className="text-ink-2">{b.latest}</span>
                    </span>
                  }
                />
              ))}
              {brew.data.length > 10 && <li className="flex h-8 items-center px-4 pl-11 text-xs text-ink-3">{tr(`et ${brew.data.length - 10} de plus`, `and ${brew.data.length - 10} more`)}</li>}
            </Rows>
          ) : (
            <p className="px-4 pb-3 text-[13px] text-ink-3">{tr("Tout est à jour.", "Everything is up to date.")}</p>
          )
        ) : (
          <p className="px-4 pb-3 text-[13px] text-ink-3">{tr("Homebrew introuvable.", "Homebrew not found.")}</p>
        )}
      </Panel>
    </div>
  );
}
