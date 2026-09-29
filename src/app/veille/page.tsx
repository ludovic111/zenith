import { Suspense } from "react";
import type { Metadata } from "next";
import { Bell, Cpu, GitPullRequest, HardDrive, MessageSquare, Newspaper, Package, Server, Star } from "lucide-react";
import { PROJECTS } from "@/lib/projects";
import { source } from "@/lib/source";
import { ago, money, pct } from "@/lib/format";
import { plural, tr } from "@/lib/i18n";
import { config } from "@/lib/config";
import { hackerNews, hnMentions, localNews, WATCH, type Headline } from "@/lib/sources/news";
import { githubMentions, groupNotifications, notifications, profile, recentStars } from "@/lib/sources/github";
import { brewOutdated, devServers, machine } from "@/lib/sources/machine";
import { crypto } from "@/lib/sources/markets";
import { fx } from "@/lib/sources/plans";
import { Panel, Skeleton, Empty, Chip } from "@/components/z/panel";
import { Gate } from "@/components/z/gate";
import { Stat } from "@/components/z/stat";
import { Heatmap } from "@/components/charts/heatmap";
import { cn } from "@/lib/utils";

export const dynamic = "force-dynamic";
export function generateMetadata(): Metadata {
  return { title: tr("Veille", "Watch") };
}

const ACCENT = "#7dd3fc";

export default function Veille() {
  return (
    <>
      <header className="mb-8">
        <div className="font-mono text-xs uppercase tracking-[0.25em] text-ink-3">{tr("Ce qui bouge autour de toi", "What moves around you")}</div>
        <h1 className="mt-3 font-display text-5xl font-black tracking-tight sm:text-6xl">{tr("Veille", "Watch")}</h1>
        <p className="mt-2 max-w-2xl font-serif text-xl italic text-ink-2">
          {tr(
            "Qui parle de tes projets, ce qui t'attend sur GitHub, l'actualité, les marchés et l'état de ce Mac.",
            "Who talks about your projects, what waits for you on GitHub, the news, the markets and the state of this Mac.",
          )}
        </p>
      </header>

      <div className="grid gap-5 xl:grid-cols-[1.4fr_1fr]">
        <Suspense fallback={<Skeleton className="h-80" />}>
          <Mentions />
        </Suspense>
        <Suspense fallback={<Skeleton className="h-80" />}>
          <Inbox />
        </Suspense>
      </div>

      <div className="mt-5">
        <Suspense fallback={<Skeleton className="h-64" />}>
          <Contributions />
        </Suspense>
      </div>

      <div className="mt-5 grid gap-5 xl:grid-cols-2">
        <Suspense fallback={<Skeleton className="h-96" />}>
          <News />
        </Suspense>
        <Suspense fallback={<Skeleton className="h-96" />}>
          <HackerNews />
        </Suspense>
      </div>

      <div className="mt-5 grid gap-5 xl:grid-cols-[1.4fr_1fr]">
        <Suspense fallback={<Skeleton className="h-72" />}>
          <ThisMac />
        </Suspense>
        <Suspense fallback={<Skeleton className="h-72" />}>
          <Markets />
        </Suspense>
      </div>
    </>
  );
}

/** Mentions of your projects outside your own repositories: Hacker News and GitHub. */
async function Mentions() {
  if (!WATCH.length)
    return (
      <Panel kicker="Mentions" title={tr("On parle de toi", "People talk about you")} accent={ACCENT}>
        <Empty>
          {tr("Ajoute les mots qui te désignent, toi ou tes projets, sous ", "Add the words that mean you or your projects under ")}
          <code className="font-mono text-xs text-sun">watch</code>
          {tr(" dans ", " in ")}
          <code className="font-mono text-xs">zenith.config.json</code>.
        </Empty>
      </Panel>
    );
  const [hn, gh] = await Promise.all([source(hnMentions), source(() => githubMentions(WATCH.map((w) => w.term)))]);
  const items = [
    ...(hn.ok ? hn.data.map((m) => ({ key: m.url, where: m.where, who: m.label, title: m.title, excerpt: m.excerpt, url: m.url, at: m.at })) : []),
    ...(gh.ok
      ? gh.data.map((m) => ({ key: m.url, where: `GitHub · ${m.repo}`, who: WATCH.find((w) => w.term === m.term)?.label ?? m.term, title: m.title, excerpt: m.kind === "pr" ? "Pull request" : "Issue", url: m.url, at: m.at }))
      : []),
  ]
    .filter((m, i, all) => all.findIndex((x) => x.key === m.key) === i)
    .sort((a, b) => b.at.localeCompare(a.at));
  return (
    <Panel kicker="Mentions" title={tr("On parle de toi", "People talk about you")} accent={ACCENT}>
      {!hn.ok && !gh.ok ? (
        <Gate src={hn}>{() => null}</Gate>
      ) : items.length ? (
        <ul className="space-y-3">
          {items.slice(0, 10).map((m) => (
            <li key={m.key}>
              <a href={m.url} target="_blank" rel="noopener noreferrer" className="group flex gap-3 text-sm">
                <MessageSquare className="mt-0.5 size-4 shrink-0 text-sky-300" />
                <div className="min-w-0 flex-1">
                  <div className="truncate text-ink group-hover:underline">{m.title}</div>
                  {m.excerpt && <div className="line-clamp-2 text-xs text-ink-2">{m.excerpt}</div>}
                  <div className="text-xs text-ink-3">{m.who} · {m.where} · {ago(m.at)}</div>
                </div>
              </a>
            </li>
          ))}
        </ul>
      ) : (
        <Empty>
          {tr(
            `Personne ne parle encore de ${WATCH.map((w) => w.term).join(", ")} sur Hacker News ni dans les issues GitHub des autres (90 jours).`,
            `Nobody mentions ${WATCH.map((w) => w.term).join(", ")} on Hacker News or in other people's GitHub issues yet (90 days).`,
          )}
        </Empty>
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

async function Inbox() {
  const repos = PROJECTS.flatMap((p) => (p.repo ? [p.repo] : []));
  const [n, stars] = await Promise.all([source(notifications), source(() => recentStars(repos))]);
  const reason = REASON();
  return (
    <Panel kicker="GitHub" title={tr("Notifications & étoiles", "Notifications & stars")} accent={ACCENT}>
      <Gate src={n}>
        {(list) => {
          const groups = groupNotifications(list);
          return list.length ? (
            <ul className="space-y-2.5">
              {groups.slice(0, 8).map((x) => (
                <li key={x.id}>
                  <a href={x.url} target="_blank" rel="noopener noreferrer" className="group flex gap-3 text-sm">
                    {x.type === "PullRequest" ? <GitPullRequest className="mt-0.5 size-4 shrink-0 text-good" /> : <Bell className={cn("mt-0.5 size-4 shrink-0", x.reason === "ci_activity" ? "text-bad" : "text-sun")} />}
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-ink group-hover:underline">{x.title}</div>
                      <div className="text-xs text-ink-3">{x.repo} · {reason[x.reason] ?? x.reason} · {ago(x.at)}{x.count > 1 ? tr(` · ${x.count} fois`, ` · ${x.count} times`) : ""}</div>
                    </div>
                  </a>
                </li>
              ))}
            </ul>
          ) : (
            <Empty>{tr("Aucune notification non lue.", "No unread notification.")}</Empty>
          );
        }}
      </Gate>
      {stars.ok && stars.data.length > 0 && (
        <div className="mt-5 border-t border-line pt-4">
          <div className="mb-2 text-[11px] uppercase tracking-[0.18em] text-ink-3">{tr("Nouvelles étoiles · 30 jours", "New stars · 30 days")}</div>
          <div className="flex flex-wrap gap-2">
            {stars.data.slice(0, 16).map((s) => (
              <a key={s.repo + s.user} href={s.url} target="_blank" rel="noopener noreferrer">
                <Chip>
                  <Star className="size-3 fill-sun text-sun" /> {s.user} → {s.repo.split("/")[1]} · {ago(s.at)}
                </Chip>
              </a>
            ))}
          </div>
        </div>
      )}
    </Panel>
  );
}

async function Contributions() {
  const p = await source(profile);
  return (
    <Panel kicker="GitHub" title={tr("Contributions de l'année", "Contributions this year")} accent={ACCENT}>
      <Gate src={p}>
        {(g) => (
          <>
            <div className="mb-5 grid grid-cols-2 gap-5 sm:grid-cols-5">
              <Stat label="Contributions" value={g.contributions} hint={tr("12 derniers mois", "last 12 months")} color={ACCENT} />
              <Stat label={tr("Série", "Streak")} value={g.streak} suffix={tr(" j", " d")} hint={g.streak ? tr("jours d'affilée", "days in a row") : tr("à relancer aujourd'hui", "restart it today")} />
              <Stat label={tr("Étoiles", "Stars")} value={g.stars} hint={tr(`sur ${g.repos} dépôts`, `across ${g.repos} repositories`)} />
              <Stat label={tr("Abonnés", "Followers")} value={g.followers} hint={`@${g.login}`} />
              <Stat label={tr("7 jours", "7 days")} value={g.days.slice(-7).reduce((a, d) => a + d.count, 0)} hint="contributions" />
            </div>
            <Heatmap days={g.days.slice(-182).map((d) => ({ t: Date.parse(`${d.date}T00:00:00Z`), total: d.count, parts: [{ name: "Contributions", color: "#ffd166", value: d.count }] }))} />
          </>
        )}
      </Gate>
    </Panel>
  );
}

function Headlines({ list, showScore }: { list: Headline[]; showScore?: boolean }) {
  return (
    <ul className="space-y-3">
      {list.map((h) => (
        <li key={h.url} className="flex gap-3 text-sm">
          <Newspaper className="mt-0.5 size-4 shrink-0 text-ink-3" />
          <div className="min-w-0 flex-1">
            <a href={h.url} target="_blank" rel="noopener noreferrer" className="text-ink hover:underline">{h.title}</a>
            <div className="flex flex-wrap gap-x-3 text-xs text-ink-3">
              <span>{h.source}</span>
              {h.at && <span>{ago(h.at)}</span>}
              {showScore && h.score != null && <span>{h.score} {plural(h.score, ["point", "points"], ["point", "points"])}</span>}
              {h.discussion && (
                <a href={h.discussion} target="_blank" rel="noopener noreferrer" className="hover:text-ink">
                  {h.comments} {plural(h.comments ?? 0, ["commentaire", "commentaires"], ["comment", "comments"])}
                </a>
              )}
            </div>
          </div>
        </li>
      ))}
    </ul>
  );
}

async function News() {
  const kicker = config().location?.name ?? tr("Actualité", "News");
  if (!config().news.length)
    return (
      <Panel kicker={kicker} title={tr("L'actualité", "The news")} accent={ACCENT}>
        <Empty>
          {tr("Ajoute tes flux RSS sous ", "Add your RSS feeds under ")}
          <code className="font-mono text-xs text-sun">news</code>
          {tr(" dans ", " in ")}
          <code className="font-mono text-xs">zenith.config.json</code>.
        </Empty>
      </Panel>
    );
  const n = await source(localNews);
  return (
    <Panel kicker={kicker} title={tr("L'actualité", "The news")} accent={ACCENT}>
      <Gate src={n}>{(list) => (list.length ? <Headlines list={list.slice(0, 10)} /> : <Empty>{tr("Pas d'article.", "No article.")}</Empty>)}</Gate>
    </Panel>
  );
}

async function HackerNews() {
  const n = await source(hackerNews);
  return (
    <Panel kicker="Tech" title="Hacker News" accent={ACCENT}>
      <Gate src={n}>{(list) => <Headlines list={list} showScore />}</Gate>
    </Panel>
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
    <Panel kicker={tr("Ce Mac", "This Mac")} title={m.ok ? `${m.data.chip} · macOS ${m.data.macos}` : tr("Ce Mac", "This Mac")} accent={ACCENT}>
      <Gate src={m}>
        {(x) => {
          const usedDisk = 1 - x.disk.freeGB / x.disk.totalGB;
          return (
            <div className="grid grid-cols-2 gap-5 sm:grid-cols-4">
              <Stat
                label={tr("Disque libre", "Free disk")}
                value={Math.round(x.disk.freeGB)}
                suffix={tr(" Go", " GB")}
                hint={tr(`${pct(usedDisk)} utilisés sur ${Math.round(x.disk.totalGB)} Go`, `${pct(usedDisk)} used of ${Math.round(x.disk.totalGB)} GB`)}
                color={usedDisk > 0.9 ? "#fb5a6b" : ACCENT}
              />
              <Stat label={tr("Mémoire libre", "Free memory")} value={x.memoryFree} suffix={tr(" %", "%")} hint={tr(`${x.memoryGB} Go au total`, `${x.memoryGB} GB in total`)} />
              <Stat label={tr("Charge", "Load")} value={Math.round(x.load * 10) / 10} hint={tr(`${x.cores} cœurs`, `${x.cores} cores`)} />
              {x.battery ? (
                <Stat label={tr("Batterie", "Battery")} value={x.battery.percent} suffix={tr(" %", "%")} hint={x.battery.charging ? tr("en charge", "charging") : tr(`sur ${x.battery.source}`, `on ${x.battery.source}`)} />
              ) : (
                <Stat label={tr("Allumé depuis", "Up for")} value={Math.floor(x.uptime / 86400)} suffix={tr(" j", " d")} hint={duration(x.uptime)} />
              )}
            </div>
          );
        }}
      </Gate>
      <div className="mt-5 grid gap-5 border-t border-line pt-4 md:grid-cols-2">
        <div>
          <div className="mb-2 flex items-center gap-2 text-[11px] uppercase tracking-[0.18em] text-ink-3"><Server className="size-3.5" /> {tr("Serveurs de dev en marche", "Running dev servers")}</div>
          {servers.ok && servers.data.length ? (
            <ul className="space-y-1.5 text-sm">
              {servers.data.map((s) => (
                <li key={s.port} className="flex items-center gap-2">
                  <a href={`http://127.0.0.1:${s.port}`} target="_blank" rel="noopener noreferrer" className="font-mono text-xs text-sky-300 hover:underline">:{s.port}</a>
                  <span className="truncate text-ink-2" title={s.dir ?? undefined}>{s.project ?? s.dir?.split("/").pop() ?? s.command}</span>
                  <span className="ml-auto font-mono text-[11px] text-ink-3">{s.command}</span>
                </li>
              ))}
            </ul>
          ) : (
            <div className="text-sm text-ink-3">{tr("Aucun.", "None.")}</div>
          )}
        </div>
        <div>
          <div className="mb-2 flex items-center gap-2 text-[11px] uppercase tracking-[0.18em] text-ink-3"><Package className="size-3.5" /> {tr("Homebrew à mettre à jour", "Homebrew updates")}</div>
          {brew.ok && brew.data ? (
            brew.data.length ? (
              <>
                <div className="flex flex-wrap gap-1.5">
                  {brew.data.slice(0, 14).map((b) => (
                    <Chip key={b.name} className="font-mono">{b.name} {b.current}→{b.latest}</Chip>
                  ))}
                </div>
                <code className="mt-2 block font-mono text-[11px] text-sun">brew upgrade</code>
              </>
            ) : (
              <div className="text-sm text-ink-3">{tr("Tout est à jour.", "Everything is up to date.")}</div>
            )
          ) : (
            <div className="text-sm text-ink-3">{tr("Homebrew introuvable.", "Homebrew not found.")}</div>
          )}
        </div>
      </div>
      <p className="mt-4 flex items-center gap-1.5 text-xs text-ink-3"><Cpu className="size-3" /><HardDrive className="size-3" /> {tr("Lu localement : sysctl, memory_pressure, pmset, lsof, brew.", "Read locally: sysctl, memory_pressure, pmset, lsof, brew.")}</p>
    </Panel>
  );
}

async function Markets() {
  const [q, rates] = await Promise.all([source(crypto), fx()]);
  const cur = rates.base;
  // Two reference currencies other than yours.
  const refs = ["EUR", "USD", "GBP"].filter((c) => c !== cur && rates.toBase[c]).slice(0, 2);
  const usdTo = rates.toBase.USD;
  return (
    <Panel kicker={tr("Marchés", "Markets")} title={tr("Cours du moment", "Current prices")} accent={ACCENT}>
      <ul className="space-y-3 text-sm">
        {refs.map((c) => (
          <li key={c} className="flex items-baseline justify-between gap-3">
            <span className="text-ink-2">1 {c}</span>
            <span className="font-mono text-ink">{money(rates.toBase[c], cur, 4)}</span>
          </li>
        ))}
        {q.ok &&
          q.data.map((x) => (
            <li key={x.symbol} className="flex items-baseline justify-between gap-3">
              <span className="text-ink-2">{x.name}</span>
              <span className="font-mono text-ink">
                {usdTo ? money(x.usd * usdTo, cur, x.usd > 1000 ? 0 : 2) : money(x.usd, "USD", x.usd > 1000 ? 0 : 2)}{" "}
                <span className={cn("text-xs", x.change >= 0 ? "text-good" : "text-bad")}>{x.change >= 0 ? "+" : ""}{pct(x.change, 1)}</span>
              </span>
            </li>
          ))}
      </ul>
      {!q.ok && <div className="mt-3"><Gate src={q} compact>{() => null}</Gate></div>}
      <p className="mt-4 text-xs text-ink-3">
        {rates.date
          ? tr(`Change BCE du ${rates.date} (Frankfurter)`, `ECB rates of ${rates.date} (Frankfurter)`)
          : tr("Change approximatif (Frankfurter injoignable)", "Approximate rates (Frankfurter unreachable)")}
        {tr(", cryptos Kraken, variation depuis minuit UTC.", ", crypto from Kraken, change since midnight UTC.")}
      </p>
    </Panel>
  );
}
