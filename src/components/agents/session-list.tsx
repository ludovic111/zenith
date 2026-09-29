"use client";

import { useState } from "react";
import { Check, GitBranch, GitPullRequest, Terminal } from "lucide-react";
import { cn } from "@/lib/utils";
import { l10n, plural, tr } from "@/lib/i18n";

export type SessionRow = {
  agent: "claude" | "codex";
  id: string;
  title: string;
  projectName: string | null;
  projectColor: string | null;
  branch: string | null;
  start: number;
  end: number;
  turns: number;
  model: string | null;
  costUSD: number | null;
  tokens: number | null;
  linesAdded: number | null;
  linesRemoved: number | null;
  prs: { number: number; url: string }[];
  subagents: number;
  live: boolean;
  resume: string;
};

const AGENT = {
  claude: { name: "Claude Code", color: "#D4724F", mark: "✳" },
  codex: { name: "Codex", color: "#5B8DEF", mark: "◎" },
};

const rel = (t: number) => {
  const m = Math.round((Date.now() - t) / 60e3);
  if (m < 1) return tr("à l'instant", "just now");
  if (m < 60) return tr(`il y a ${m} min`, `${m} min ago`);
  const h = Math.round(m / 60);
  if (h < 24) return tr(`il y a ${h} h`, `${h} h ago`);
  return tr(`il y a ${Math.round(h / 24)} j`, `${Math.round(h / 24)} d ago`);
};

const dur = (ms: number) => {
  const m = Math.round(ms / 60e3);
  if (m < 60) return `${m} min`;
  return `${Math.floor(m / 60)} h ${String(m % 60).padStart(2, "0")}`;
};

const tok = (n: number) => new Intl.NumberFormat(l10n().locale, { notation: "compact", maximumFractionDigits: 1 }).format(n);

export function SessionList({ rows, compact }: { rows: SessionRow[]; compact?: boolean }) {
  const [copied, setCopied] = useState<string | null>(null);
  const copy = async (r: SessionRow) => {
    await navigator.clipboard.writeText(r.resume);
    setCopied(r.id);
    setTimeout(() => setCopied(null), 1500);
  };
  if (!rows.length) return <p className="text-sm text-ink-3">{tr("Aucune session pour l'instant.", "No sessions yet.")}</p>;
  return (
    <ul className="divide-y divide-line">
      {rows.map((r) => {
        const a = AGENT[r.agent];
        return (
          <li key={r.agent + r.id} className="group flex gap-3 py-3">
            <span
              className="relative mt-0.5 grid size-8 shrink-0 place-items-center rounded-xl text-sm font-bold"
              style={{ background: `${a.color}22`, color: a.color }}
              title={a.name}
            >
              {a.mark}
              {r.live && <span className="live-dot absolute -right-0.5 -top-0.5 size-2.5 rounded-full ring-2 ring-[#0b0a14]" style={{ color: "var(--good)", background: "var(--good)" }} />}
            </span>
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-2">
                <span className="truncate text-sm font-medium text-ink">{r.title}</span>
                {r.live && <span className="shrink-0 rounded-full bg-good/15 px-2 py-0.5 text-[10px] font-medium text-good">{tr("en cours", "live")}</span>}
              </div>
              <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-ink-3">
                <span style={{ color: a.color }}>{a.name}</span>
                {!compact && r.projectName && (
                  <span className="inline-flex items-center gap-1.5">
                    <span className="size-1.5 rounded-full" style={{ background: r.projectColor ?? "var(--ink-3)" }} />
                    {r.projectName}
                  </span>
                )}
                {r.branch && (
                  <span className="inline-flex max-w-48 items-center gap-1 truncate font-mono">
                    <GitBranch className="size-3 shrink-0" />
                    {r.branch.replace(/^claude\//, "")}
                  </span>
                )}
                <span>{rel(r.end)}</span>
                <span>{dur(r.end - r.start)}</span>
                {r.turns > 0 && <span>{r.turns} {plural(r.turns, ["message", "messages"], ["message", "messages"])}</span>}
                {r.costUSD != null && <span className="font-mono text-ink-2">{tr(`${r.costUSD.toFixed(2)} $`, `$${r.costUSD.toFixed(2)}`)}</span>}
                {r.costUSD == null && r.tokens != null && <span className="font-mono text-ink-2">{tok(r.tokens)} tokens</span>}
                {r.linesAdded != null && (
                  <span className="font-mono">
                    <span className="text-good">+{r.linesAdded}</span> <span className="text-bad">−{r.linesRemoved ?? 0}</span>
                  </span>
                )}
                {r.subagents > 0 && <span>{tr(`${r.subagents} sous-agents`, `${r.subagents} ${r.subagents === 1 ? "subagent" : "subagents"}`)}</span>}
                {r.prs.map((p) => (
                  <a key={p.number} href={p.url} target="_blank" rel="noopener noreferrer" className="inline-flex items-center gap-1 text-ink-2 hover:text-ink">
                    <GitPullRequest className="size-3" /> #{p.number}
                  </a>
                ))}
              </div>
            </div>
            <button
              onClick={() => copy(r)}
              className={cn(
                "h-8 shrink-0 self-center rounded-lg border border-line px-2.5 text-xs text-ink-3 transition hover:text-ink",
                "opacity-100 lg:opacity-0 lg:group-hover:opacity-100",
                copied === r.id && "lg:opacity-100",
              )}
              title={r.resume}
            >
              {copied === r.id ? (
                <span className="inline-flex items-center gap-1 text-good"><Check className="size-3.5" /> {tr("copié", "copied")}</span>
              ) : (
                <span className="inline-flex items-center gap-1"><Terminal className="size-3.5" /> {tr("reprendre", "resume")}</span>
              )}
            </button>
          </li>
        );
      })}
    </ul>
  );
}
