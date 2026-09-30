import type { Metadata } from "next";
import Link from "next/link";
import { Suspense } from "react";
import { tr } from "@/lib/i18n";
import { PROJECTS } from "@/lib/projects";
import { now } from "@/lib/agent/now";
import { SUGGESTIONS, agentUi, examplesFrom } from "@/lib/agent/ui";
import { Empty, PageHeader, Panel, Skeleton } from "@/components/z/panel";
import { AskBar } from "@/components/agent/ask-bar";
import { RoutinesList } from "@/components/agent/routines-panel";
import { routineViews } from "@/components/agent/routine-view";
import { ActivityList, SkillList, TeamGrid } from "@/components/agent/team";
import { activityViews, gatewayStatus, skillViews, teamView } from "@/components/agent/team-view";

export const dynamic = "force-dynamic";
export function generateMetadata(): Metadata {
  return { title: tr("Équipe", "Team") };
}

/**
 * The team's space: say what you want to one of your agents, see who is working, what
 * they did lately, and what they do on their own (routines) or know how to do (skills).
 */
export default function TeamPage() {
  const ui = agentUi();
  if (!ui.enabled)
    return (
      <>
        <PageHeader title={tr("Équipe", "Team")} />
        <Empty>{tr("Les agents passent par zenith code : active-le (code.enabled) et l'agent (agent.enabled).", "Agents run through zenith code: turn it on (code.enabled) and the agent (agent.enabled).")}</Empty>
      </>
    );
  const gw = gatewayStatus();
  return (
    <>
      <PageHeader
        title={tr("Équipe", "Team")}
        description={tr(
          "Parle à tes agents : ils travaillent sur tes abonnements Claude et ChatGPT, et se parlent entre eux.",
          "Talk to your agents: they work on your Claude and ChatGPT subscriptions, and talk to each other.",
        )}
        action={
          <Link href="/reglages" className="text-xs text-ink-3 transition-colors hover:text-ink" title={tr("Parle à ton agent depuis Telegram", "Talk to your agent from Telegram")}>
            Telegram · {gw.running ? tr("branché", "on") : tr("à brancher", "off")}
          </Link>
        }
      />

      <Suspense fallback={<Skeleton className="h-[108px]" />}>
        <Ask />
      </Suspense>

      <Suspense fallback={<Skeleton className="mt-6 h-48" />}>
        <Members />
      </Suspense>

      <div className="mt-6 grid gap-4 lg:grid-cols-5">
        <Suspense fallback={<Skeleton className="h-72 lg:col-span-3" />}>
          <Recent />
        </Suspense>
        <div className="flex flex-col gap-4 lg:col-span-2">
          <Suspense fallback={<Skeleton className="h-40" />}>
            <Routines />
          </Suspense>
          <Suspense fallback={<Skeleton className="h-40" />}>
            <Skills />
          </Suspense>
        </div>
      </div>
    </>
  );
}

async function Ask() {
  const ui = agentUi();
  const items = await now().catch(() => []);
  const names = Object.fromEntries(PROJECTS.map((p) => [p.id, p.name]));
  return <AskBar targets={ui.targets} provider={ui.provider} examples={examplesFrom(items, names)} suggestions={SUGGESTIONS()} />;
}

async function Members() {
  return (
    <div className="mt-6">
      <TeamGrid members={await teamView()} />
    </div>
  );
}

async function Recent() {
  const items = await activityViews(14);
  return (
    <Panel title={tr("Conversations récentes", "Recent conversations")} className="lg:col-span-3" bodyClassName={items.length ? "p-0 pb-1" : undefined}>
      {items.length ? <ActivityList items={items} /> : <Empty>{tr("Rien encore : dis quelque chose à ton équipe.", "Nothing yet: say something to your team.")}</Empty>}
    </Panel>
  );
}

async function Routines() {
  const list = await routineViews();
  return (
    <Panel
      title={
        <span>
          {tr("Routines", "Routines")}
          {list.length > 0 && <span className="ml-2 font-normal text-ink-3 tabular">{list.filter((r) => r.enabled).length}/{list.length}</span>}
        </span>
      }
      bodyClassName={list.length ? "p-0 pb-1" : undefined}
    >
      {list.length ? (
        <RoutinesList routines={list} />
      ) : (
        <Empty>{tr("Des agents qui travaillent seuls, à heure fixe ou quand quelque chose arrive (agent.routines).", "Agents that work on their own, at a set time or when something happens (agent.routines).")}</Empty>
      )}
    </Panel>
  );
}

async function Skills() {
  const skills = await skillViews();
  return (
    <Panel
      title={
        <span>
          Skills
          <span className="ml-2 font-normal text-ink-3 tabular">{skills.length}</span>
        </span>
      }
      bodyClassName={skills.length ? "p-0 pb-1" : undefined}
    >
      {skills.length ? <SkillList skills={skills} /> : <Empty>{tr("Les skills apparaissent à la première demande.", "Skills appear with the first request.")}</Empty>}
    </Panel>
  );
}
