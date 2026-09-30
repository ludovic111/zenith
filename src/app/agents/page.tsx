import type { Metadata } from "next";
import Link from "next/link";
import { PROJECTS } from "@/lib/projects";
import { source } from "@/lib/source";
import { date, nf, today, usd } from "@/lib/format";
import { l10n, plural, tr } from "@/lib/i18n";
import { AGENTS, isLive, sessions, type Session } from "@/lib/sources/agents";
import { Empty, PageHeader, Panel } from "@/components/z/panel";
import { Gate } from "@/components/z/gate";
import { Stat } from "@/components/z/stat";
import { LiveDot } from "@/components/z/status";
import { Bars } from "@/components/charts/bars";
import { HBars } from "@/components/charts/hbars";
import { SessionList } from "@/components/agents/session-list";
import { toRows } from "@/components/agents/rows";
import { PlanCard } from "@/components/plans/plan-card";
import { claudePlan, codexPlan } from "@/lib/sources/plans";
import { SUBSCRIPTIONS } from "@/lib/subscriptions";
import { agentUi } from "@/lib/agent/ui";
import { RoutinesList } from "@/components/agent/routines-panel";
import { routineViews } from "@/components/agent/routine-view";
import { AskButton } from "@/components/agent/ask-button";

export const dynamic = "force-dynamic";
export function generateMetadata(): Metadata {
  return { title: tr("Agents IA", "AI agents") };
}

/** Monthly plan of a vendor listed in `subscriptions`, if any (the most expensive active one). */
const planOf = (vendor: string) =>
  SUBSCRIPTIONS.filter((x) => x.vendor.toLowerCase() === vendor.toLowerCase() && x.period === "month" && x.status !== "cancelled").sort((a, b) => (b.amount ?? 0) - (a.amount ?? 0))[0];

const WEEK = 7 * 864e5;
/** Local calendar day of an instant, in your time zone. */
const dayOf = (t: number) => new Intl.DateTimeFormat("en-CA", { timeZone: l10n().timeZone }).format(new Date(t));
const sum = (list: Session[], f: (s: Session) => number | null) => list.reduce((a, s) => a + (f(s) ?? 0), 0);

export default async function AgentsPage() {
  const [all, claude, codex] = await Promise.all([source(sessions), source(claudePlan), source(codexPlan)]);
  const ui = agentUi();
  const list = all.ok ? all.data : [];
  const live = list.filter(isLive);
  const now = Date.now();
  const d = today();
  const todayN = list.filter((s) => dayOf(s.start) === d).length;
  const week = list.filter((s) => s.end > now - WEEK);
  const cost = sum(week, (s) => s.costUSD);

  const summary = all.ok
    ? [
        live.length ? tr(`${live.length} en cours`, `${live.length} running`) : tr("aucune en cours", "none running"),
        tr(`${todayN} ${plural(todayN, ["session", "sessions"], ["", ""])} aujourd'hui`, `${todayN} ${todayN === 1 ? "session" : "sessions"} today`),
        cost > 0 ? tr(`${usd(cost, 0)} cette semaine`, `${usd(cost, 0)} this week`) : null,
      ]
        .filter(Boolean)
        .join(" · ")
    : tr("Claude Code et Codex, lus sur ce Mac.", "Claude Code and Codex, read from this Mac.");

  return (
    <>
      <PageHeader
        title={tr("Agents IA", "AI agents")}
        description={summary}
        action={
          ui.enabled && all.ok && list.length > 0 ? (
            <AskButton
              target="life"
              label={tr("Bilan de la semaine", "Weekly review")}
              prompt={tr(
                "Fais le bilan de la semaine de mes agents IA (Claude Code et Codex) : ce qui a été fait dans chaque projet, les PR ouvertes à relire, les sessions laissées en plan et ce que je devrais reprendre en premier. Les sessions sont dans ~/.claude/projects et ~/.codex/sessions, le brief zenith résume le rythme. Ne modifie rien, c'est seulement un résumé.",
                "Review my AI agents' week (Claude Code and Codex): what got done in each project, the open PRs to review, the sessions left unfinished and what I should pick up first. Sessions are in ~/.claude/projects and ~/.codex/sessions; the zenith brief sums up the rhythm. Don't change anything, it's only a summary.",
              )}
            />
          ) : undefined
        }
      />

      <Gate src={all}>
        {() => (
          <div className="grid grid-cols-2 gap-x-6 gap-y-4 rounded-xl border border-line bg-surface px-4 py-3.5 sm:grid-cols-3 lg:grid-cols-6">
            <Stat
              label={
                <span className="inline-flex items-center gap-1.5">
                  {live.length > 0 && <LiveDot />}
                  {tr("En cours", "Running")}
                </span>
              }
              value={live.length}
              hint={tr("actives < 3 min", "active < 3 min")}
            />
            <Stat label={tr("Aujourd'hui", "Today")} value={todayN} hint={tr("sessions lancées", "sessions started")} />
            <Stat label={tr("7 jours", "7 days")} value={week.length} hint={tr(`${nf(list.length)} au total`, `${nf(list.length)} in total`)} />
            <Stat label={tr("Coût Claude · 7 j", "Claude cost · 7 d")} value={cost} format={{ style: "currency", currency: "USD", maximumFractionDigits: 0 }} color={AGENTS.claude.color} hint={tr("équivalent API", "API equivalent")} />
            <Stat
              label={tr("Lignes · 7 j", "Lines · 7 d")}
              value={sum(week, (s) => s.linesAdded)}
              format={{ notation: "compact" }}
              prefix="+"
              hint={tr(`−${nf(sum(week, (s) => s.linesRemoved))} retirées`, `−${nf(sum(week, (s) => s.linesRemoved))} removed`)}
            />
            <Stat label={tr("PR · 7 j", "PRs · 7 d")} value={sum(week, (s) => s.prs.length)} hint={tr("ouvertes par des agents", "opened by agents")} />
          </div>
        )}
      </Gate>

      {live.length > 0 && (
        <Panel
          className="mt-4"
          title={
            <span className="inline-flex items-center gap-2">
              <LiveDot />
              {tr("En cours", "Running")}
              <span className="font-normal text-ink-3 tabular">{live.length}</span>
            </span>
          }
          bodyClassName="px-4 pb-2 pt-0"
        >
          <SessionList rows={toRows(live)} />
        </Panel>
      )}

      <div className="mt-8 grid gap-4 lg:grid-cols-2">
        <PlanCard
          title="Claude"
          color={AGENTS.claude.color}
          price={planOf("Anthropic")}
          src={claude}
          hint={tr("Aucun fichier local n'expose les limites Claude : un agent les relève pour zenith.", "No local file exposes Claude's limits: an agent reads them for zenith.")}
          action={
            ui.enabled ? (
              <AskButton
                target="zenith"
                label={tr("Relever mes limites", "Read my limits")}
                prompt={tr(
                  "Mets à jour mes limites Claude dans zenith : relève l'usage de mon plan Claude (fenêtre de 5 heures et semaine, avec leurs dates de recharge) et écris .data/claude-plan.json au format décrit dans docs/releves.md. Ne touche à rien d'autre.",
                  "Update my Claude limits in zenith: read my Claude plan usage (5-hour and weekly windows, with their reset times) and write .data/claude-plan.json in the format described in docs/releves.md. Don't touch anything else.",
                )}
              />
            ) : undefined
          }
        />
        <PlanCard title="ChatGPT & Codex" color={AGENTS.codex.color} price={planOf("OpenAI")} src={codex} hint={tr("Lues dans ~/.codex dès ta première session Codex.", "Read from ~/.codex after your first Codex session.")} />
      </div>

      {ui.enabled && <Routines />}

      <Gate src={all}>
        {(list) => {
          const days = Array.from({ length: 21 }, (_, i) => {
            const t = new Date();
            t.setHours(0, 0, 0, 0);
            return t.getTime() - (20 - i) * 864e5;
          });
          const perDay = days.map((t) => ({
            label: date(t),
            parts: (["claude", "codex"] as const).map((a) => ({
              key: a,
              name: AGENTS[a].name,
              color: AGENTS[a].color,
              value: list.filter((s) => s.agent === a && s.start >= t && s.start < t + 864e5).length,
            })),
          }));
          const byProject = [
            ...PROJECTS.map((p) => ({ id: p.id as string, name: p.name, color: p.color })),
            // Sessions in zenith's own folder, unless it is already one of your projects.
            ...(PROJECTS.some((p) => p.id === "zenith") ? [] : [{ id: "zenith", name: "zenith", color: "var(--primary)" }]),
            { id: "null", name: tr("Hors projet", "No project"), color: "var(--ink-3)" },
          ]
            .map((p) => {
              const mine = list.filter((s) => String(s.project) === p.id);
              return { ...p, n: mine.length, cost: sum(mine, (s) => s.costUSD) };
            })
            .filter((p) => p.n > 0)
            .sort((a, b) => b.n - a.n);
          const history = list.filter((s) => !isLive(s));
          return (
            <>
              <div className="mt-8 grid gap-4 lg:grid-cols-3">
                <Panel title={tr("Sessions par jour", "Sessions per day")} action={tr("21 jours", "21 days")} className="lg:col-span-2">
                  <Bars data={perDay} height={160} legend={Object.values(AGENTS).map((a) => ({ name: a.name, color: a.color }))} />
                </Panel>
                <Panel title={tr("Par projet", "Per project")} action={tr("sessions · coût", "sessions · cost")}>
                  {byProject.length ? (
                    <HBars rows={byProject.map((p) => ({ label: p.name, value: p.n, key: p.id, color: p.color, hint: p.cost ? usd(p.cost, 0) : undefined }))} color="var(--ink-3)" />
                  ) : (
                    <Empty>{tr("Aucune session pour l'instant.", "No session yet.")}</Empty>
                  )}
                </Panel>
              </div>
              <Panel
                className="mt-4"
                title={tr("Sessions", "Sessions")}
                action={<span className="tabular">{history.length > 80 ? tr(`80 dernières sur ${nf(history.length)}`, `latest 80 of ${nf(history.length)}`) : nf(history.length)}</span>}
                bodyClassName="px-4 pb-2 pt-0"
              >
                <SessionList rows={toRows(history.slice(0, 80))} />
              </Panel>
            </>
          );
        }}
      </Gate>
    </>
  );
}

/** zenith's routines (`agent.routines`), or how to add one. */
async function Routines() {
  const list = await routineViews();
  return (
    <Panel
      className="mt-4"
      title={
        <span>
          {tr("Routines", "Routines")}
          {list.length > 0 && <span className="ml-2 font-normal text-ink-3 tabular">{list.filter((r) => r.enabled).length}/{list.length}</span>}
        </span>
      }
      action={
        <Link href="/reglages" className="hover:text-ink">
          {tr("Réglages", "Settings")}
        </Link>
      }
      bodyClassName={list.length ? "p-0 pb-1" : undefined}
    >
      {list.length ? (
        <RoutinesList routines={list} />
      ) : (
        <Empty>
          <span>
            {tr("Des agents qui travaillent seuls, chaque jour à heure fixe. Ajoute-en dans zenith.config.json : ", "Agents that work on their own, every day at a set time. Add some in zenith.config.json: ")}
            <code className="font-mono text-xs text-ink-2">{`"agent": { "routines": [{ "id": "matin", "at": "07:30", "task": "refresh-life" }] }`}</code>
          </span>
        </Empty>
      )}
    </Panel>
  );
}
