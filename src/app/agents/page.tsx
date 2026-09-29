import type { Metadata } from "next";
import { Bot } from "lucide-react";
import { PROJECTS } from "@/lib/projects";
import { source } from "@/lib/source";
import { date, nf, usd } from "@/lib/format";
import { tr } from "@/lib/i18n";
import { AGENTS, isLive, sessions } from "@/lib/sources/agents";
import { Empty, Panel } from "@/components/z/panel";
import { Gate } from "@/components/z/gate";
import { Stat } from "@/components/z/stat";
import { LiveDot } from "@/components/z/status";
import { Bars } from "@/components/charts/bars";
import { HBars } from "@/components/charts/hbars";
import { SessionList } from "@/components/agents/session-list";
import { toRows } from "@/components/agents/rows";
import { Meteors } from "@/components/ui/meteors";
import { PlanCard } from "@/components/plans/plan-card";
import { claudePlan, codexPlan } from "@/lib/sources/plans";
import { SUBSCRIPTIONS } from "@/lib/subscriptions";
import { config } from "@/lib/config";
import { routines } from "@/lib/agent/routines";
import { agentUi } from "@/lib/agent/ui";
import { RoutinesList } from "@/components/agent/routines-panel";

export const dynamic = "force-dynamic";
export function generateMetadata(): Metadata {
  return { title: tr("Agents IA", "AI agents") };
}

/** Monthly plan of a vendor listed in `subscriptions`, if any (the most expensive active one). */
const planOf = (vendor: string) =>
  SUBSCRIPTIONS.filter((x) => x.vendor.toLowerCase() === vendor.toLowerCase() && x.period === "month" && x.status !== "cancelled").sort((a, b) => (b.amount ?? 0) - (a.amount ?? 0))[0];

export default async function AgentsPage() {
  const [all, claude, codex] = await Promise.all([source(sessions), source(claudePlan), source(codexPlan)]);
  return (
    <>
      <header className="relative mb-8 overflow-hidden rounded-[2rem] border border-line px-6 py-8 sm:px-10 sm:py-10">
        <div aria-hidden className="absolute inset-0" style={{ background: `radial-gradient(110% 140% at 0% 0%, ${AGENTS.claude.color}33 0%, transparent 55%), radial-gradient(90% 130% at 100% 100%, ${AGENTS.codex.color}33 0%, transparent 60%)` }} />
        <div aria-hidden className="absolute inset-0 overflow-hidden opacity-60"><Meteors number={12} /></div>
        <div className="relative">
          <h1 className="font-display text-5xl font-black tracking-tight sm:text-7xl">
            <Bot className="mr-3 inline size-10 align-middle sm:size-14" />
            {tr("Agents IA", "AI agents")}
          </h1>
          <p className="mt-2 font-serif text-xl italic text-ink-2 sm:text-2xl">{tr("Claude Code et Codex, lus directement sur ce Mac.", "Claude Code and Codex, read straight from this Mac.")}</p>
          <Gate src={all}>
            {(list) => {
              const d30 = list.filter((s) => s.end > Date.now() - 30 * 864e5);
              return (
                <div className="mt-8 grid grid-cols-2 gap-6 rounded-3xl border border-white/10 bg-black/30 p-6 backdrop-blur md:grid-cols-3 xl:grid-cols-6">
                  <Stat label={<span className="inline-flex items-center gap-2"><LiveDot /> {tr("En cours", "Running")}</span>} value={list.filter(isLive).length} hint={tr("actives ces 3 dernières minutes", "active in the last 3 minutes")} />
                  <Stat label={tr("Sessions 7 j", "Sessions 7 d")} value={list.filter((s) => s.start > Date.now() - 7 * 864e5).length} hint={tr(`${list.length} au total`, `${list.length} in total`)} />
                  <Stat label={tr("Coût Claude 30 j", "Claude cost 30 d")} value={d30.reduce((a, s) => a + (s.costUSD ?? 0), 0)} format={{ style: "currency", currency: "USD", maximumFractionDigits: 0 }} color={AGENTS.claude.color} hint={tr("équivalent API, d'après Claude Code", "API equivalent, per Claude Code")} />
                  <Stat label={tr("Lignes ajoutées", "Lines added")} value={d30.reduce((a, s) => a + (s.linesAdded ?? 0), 0)} hint={tr(`−${nf(d30.reduce((a, s) => a + (s.linesRemoved ?? 0), 0))} retirées · 30 j`, `−${nf(d30.reduce((a, s) => a + (s.linesRemoved ?? 0), 0))} removed · 30 d`)} />
                  <Stat label={tr("PR ouvertes", "PRs opened")} value={d30.reduce((a, s) => a + s.prs.length, 0)} hint={tr("par des agents · 30 j", "by agents · 30 d")} />
                  <Stat label={tr("Tokens Codex", "Codex tokens")} value={d30.filter((s) => s.agent === "codex").reduce((a, s) => a + (s.tokens ?? 0), 0)} format={{ notation: "compact" }} color={AGENTS.codex.color} hint={tr("30 j", "30 d")} />
                </div>
              );
            }}
          </Gate>
        </div>
      </header>

      {agentUi().enabled && <Routines />}

      <div className="mb-5 grid gap-5 xl:grid-cols-2">
        <PlanCard title="Claude" color={AGENTS.claude.color} price={planOf("Anthropic")} src={claude} hint={CLAUDE_HINT()} />
        <PlanCard title="ChatGPT & Codex" color={AGENTS.codex.color} price={planOf("OpenAI")} src={codex} hint={CODEX_HINT()} />
      </div>

      <Gate src={all}>
        {(list) => {
          const days = Array.from({ length: 21 }, (_, i) => {
            const d = new Date();
            d.setHours(0, 0, 0, 0);
            return d.getTime() - (20 - i) * 864e5;
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
            ...(PROJECTS.some((p) => p.id === "zenith") ? [] : [{ id: "zenith", name: "zenith", color: "#FFD166" }]),
            { id: "null", name: tr("Hors projet", "No project"), color: "#7c7791" },
          ]
            .map((p) => ({ ...p, n: list.filter((s) => String(s.project) === p.id).length, cost: list.filter((s) => String(s.project) === p.id).reduce((a, s) => a + (s.costUSD ?? 0), 0) }))
            .filter((p) => p.n > 0)
            .sort((a, b) => b.n - a.n);
          return (
            <>
              <div className="grid gap-5 xl:grid-cols-3">
                <Panel kicker={tr("Rythme", "Rhythm")} title={tr("Sessions lancées par jour", "Sessions started per day")} className="xl:col-span-2">
                  <Bars data={perDay} height={170} legend={Object.values(AGENTS).map((a) => ({ name: a.name, color: a.color }))} />
                </Panel>
                <Panel kicker={tr("Où", "Where")} title={tr("Sessions par projet", "Sessions per project")}>
                  {byProject.length ? (
                    <HBars rows={byProject.map((p) => ({ label: p.name, value: p.n, key: p.id, color: p.color, hint: p.cost ? tr(`${p.cost.toFixed(0)} $`, usd(p.cost, 0)) : undefined }))} color="#FFD166" />
                  ) : (
                    <Empty>{tr("Aucune session pour l'instant.", "No session yet.")}</Empty>
                  )}
                </Panel>
              </div>
              <Panel kicker={tr("Historique", "History")} title={tr("Toutes les sessions", "All sessions")} className="mt-5">
                <SessionList rows={toRows(list.slice(0, 80))} />
              </Panel>
            </>
          );
        }}
      </Gate>
    </>
  );
}

const CLAUDE_HINT = () =>
  tr(
    "Aucun fichier local n'expose les limites Claude : demande à Claude « mets à jour mes limites Claude dans zenith ».",
    'No local file exposes Claude\'s limits: ask Claude "update my Claude limits in zenith".',
  );

const CODEX_HINT = () => tr("Lues dans ~/.codex dès ta première session Codex.", "Read from ~/.codex after your first Codex session.");

/** zenith's routines (`agent.routines`), or how to add one. */
async function Routines() {
  const list = await routines();
  const names = Object.fromEntries(agentUi().targets.map((t) => [t.id, t.name]));
  return (
    <Panel className="mb-5" kicker="zenith" title={tr("Routines", "Routines")} accent="#FFD166">
      {list.length ? (
        <RoutinesList
          routines={list.map((r) => ({ id: r.id, title: r.title, at: r.at, days: r.days, enabled: r.enabled, target: names[r.project ?? "life"] ?? r.project ?? "", last: r.last }))}
        />
      ) : (
        <Empty>
          <span>
            {tr("Des agents qui travaillent seuls, chaque jour à heure fixe. Ajoute-en dans ", "Agents that work on their own, every day at a set time. Add some in ")}
            <code className="font-mono text-xs text-ink-2">{config().meta.file.split("/").slice(-2).join("/")}</code>
            {tr(" : ", ": ")}
            <code className="font-mono text-xs text-ink-2">{`"agent": { "routines": [{ "id": "matin", "at": "07:30", "task": "refresh-life" }] }`}</code>
          </span>
        </Empty>
      )}
    </Panel>
  );
}
