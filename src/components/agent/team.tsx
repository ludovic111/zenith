"use client";

import Link from "next/link";
import { CalendarClock, Hand, MessageSquare, Radar, Smartphone, Sparkles, SquareTerminal, Workflow } from "lucide-react";
import { ago } from "@/lib/format";
import { plural, tr } from "@/lib/i18n";
import { threadHref } from "@/components/code/store";
import type { Avatar } from "@/lib/agent/avatar";
import { AgentAvatar } from "./agent-avatar";
import { ProviderIcon } from "./provider-icon";
import { openAsk } from "./client";

export type MemberView = {
  id: string;
  name: string;
  /** Its job ("Mail"), next to its first name. */
  title: string | null;
  avatar: Avatar;
  provider: "claude" | "codex";
  model: string | null;
  role: string;
  home: string;
  /** Lines in its MEMORY.md. */
  memory: number;
  main: boolean;
};

export type SkillView = { id: string; description: string; file: string };

export type ActivityView = { at: string; source: string; target: string; targetId: string; avatar: Avatar | null; title: string; threadId: string; environmentId: string };

/** Your team: who they are, which subscription they run on, and a way to talk to each. */
export function TeamGrid({ members }: { members: MemberView[] }) {
  return (
    <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
      {members.map((m) => (
        <button
          key={m.id}
          type="button"
          onClick={() => openAsk(m.main ? "" : `@${m.name.toLowerCase()} `)}
          title={`${tr(`Parler à ${m.name}`, `Talk to ${m.name}`)} · ${m.home}`}
          className="group flex min-w-0 flex-col gap-2 rounded-xl border border-line bg-surface p-3.5 text-left transition-colors hover:border-ink-3/30 hover:bg-hover/40"
        >
          <div className="flex items-center gap-2.5">
            <span className="grid size-11 shrink-0 place-items-center rounded-xl transition-transform group-hover:-translate-y-0.5" style={{ background: `color-mix(in oklab, ${m.avatar.color} 12%, transparent)` }}>
              <AgentAvatar avatar={m.avatar} id={m.id} size={34} blink />
            </span>
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-1.5 text-[13px] font-medium text-ink">
                <span className="truncate">{m.name}</span>
                {m.title && <span className="truncate font-normal text-ink-3">· {m.title}</span>}
                {m.main && <span className="shrink-0 rounded border border-line px-1 text-2xs font-normal text-ink-3">{tr("principal", "main")}</span>}
              </div>
              <div className="flex items-center gap-1 text-2xs text-ink-3">
                <ProviderIcon id={m.provider} size={10} />
                <span className="truncate" title={m.provider === "claude" ? tr("Ton abonnement Claude, par Claude Code", "Your Claude subscription, through Claude Code") : tr("Ton abonnement ChatGPT, par Codex", "Your ChatGPT subscription, through Codex")}>
                  {m.provider === "claude" ? "Claude" : "Codex"}
                  {m.model ? ` · ${m.model}` : ""}
                </span>
              </div>
            </div>
            <MessageSquare className="size-3.5 shrink-0 text-ink-3 opacity-0 transition-opacity group-hover:opacity-100" />
          </div>
          <p className="line-clamp-2 text-xs text-ink-2">{m.role}</p>
          <div className="mt-auto flex items-center gap-2 text-2xs text-ink-3">
            <code className="font-mono">{m.main ? "⌘J" : `@${m.name.toLowerCase()}`}</code>
            <span className="ml-auto shrink-0 tabular">{m.memory > 0 ? tr(`${m.memory} ${plural(m.memory, ["souvenir", "souvenirs"], ["", ""])}`, `${m.memory} ${m.memory === 1 ? "memory" : "memories"}`) : tr("mémoire vide", "no memory yet")}</span>
          </div>
        </button>
      ))}
    </div>
  );
}

/** Written know-how the whole team follows. */
export function SkillList({ skills }: { skills: SkillView[] }) {
  return (
    <ul className="divide-y divide-line">
      {skills.map((s) => (
        <li key={s.id} className="flex items-start gap-3 px-4 py-2.5">
          <Sparkles className="mt-0.5 size-3.5 shrink-0 text-ink-3" />
          <div className="min-w-0 flex-1">
            <div className="font-mono text-[12px] text-ink">{s.id}</div>
            <div className="line-clamp-2 text-xs text-ink-3" title={s.file}>
              {s.description}
            </div>
          </div>
        </li>
      ))}
    </ul>
  );
}

const SOURCES: Record<string, { icon: typeof Hand; label: () => string }> = {
  bar: { icon: MessageSquare, label: () => tr("toi", "you") },
  command: { icon: MessageSquare, label: () => tr("toi", "you") },
  now: { icon: Hand, label: () => tr("Maintenant", "Now") },
  routine: { icon: CalendarClock, label: () => tr("routine", "routine") },
  watch: { icon: Radar, label: () => tr("déclencheur", "trigger") },
  mcp: { icon: Workflow, label: () => tr("un agent", "an agent") },
  gateway: { icon: Smartphone, label: () => "Telegram" },
};

/** What zenith asked its agents lately, and why. */
export function ActivityList({ items }: { items: ActivityView[] }) {
  return (
    <ul className="divide-y divide-line">
      {items.map((a) => {
        const s = SOURCES[a.source] ?? { icon: SquareTerminal, label: () => a.source };
        return (
          <li key={a.threadId}>
            <Link href={threadHref({ environmentId: a.environmentId, id: a.threadId })} className="flex items-center gap-3 px-4 py-2 transition-colors hover:bg-hover/50">
              {a.avatar ? <AgentAvatar avatar={a.avatar} id={`act-${a.targetId}`} size={16} /> : <s.icon className="size-3.5 shrink-0 text-ink-3" />}
              <span className="min-w-0 flex-1 truncate text-[13px] text-ink">{a.title}</span>
              <span className="hidden shrink-0 items-center gap-1 text-xs text-ink-3 sm:inline-flex">
                <s.icon className="size-3" /> {s.label()} → {a.target}
              </span>
              <span className="w-16 shrink-0 text-right text-xs text-ink-3 tabular" suppressHydrationWarning>
                {ago(a.at)}
              </span>
            </Link>
          </li>
        );
      })}
    </ul>
  );
}
