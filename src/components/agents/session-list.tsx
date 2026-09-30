"use client";

import { useState } from "react";
import { Check, GitBranch, GitPullRequest, Terminal } from "lucide-react";
import { cn } from "@/lib/utils";
import { ProviderIcon } from "@/components/agent/provider-icon";
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
  claude: { name: "Claude Code" },
  codex: { name: "Codex" },
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
  if (!rows.length) return <p className="py-2 text-[13px] text-ink-3">{tr("Aucune session pour l'instant.", "No sessions yet.")}</p>;
  return (
    <ul className="divide-y divide-line">
      {rows.map((r) => {
        const a = AGENT[r.agent];
        return (
          <li key={r.agent + r.id} className="group flex items-center gap-3 py-2.5">
            <span className="relative grid size-7 shrink-0 place-items-center rounded-md border border-line bg-muted text-ink-2" title={a.name}>
              <ProviderIcon id={r.agent} size={14} />
              {r.live && (
                <span
                  className="live-dot absolute -right-0.5 -top-0.5 size-2 rounded-full ring-2 ring-surface"
                  style={{ color: "var(--good)", background: "var(--good)" }}
                />
              )}
            </span>
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-2">
                <span className="truncate text-[13px] font-medium text-ink">{r.title}</span>
                {r.live && <span className="shrink-0 rounded bg-good/10 px-1.5 py-px text-2xs font-medium text-good">{tr("en cours", "live")}</span>}
              </div>
              <div className="mt-0.5 flex flex-wrap items-center gap-x-2.5 gap-y-0.5 text-xs text-ink-3">
                <span>{a.name}</span>
                {!compact && r.projectName && (
                  <span className="inline-flex items-center gap-1.5">
                    <span className="size-1.5 rounded-full" style={{ background: r.projectColor ?? "var(--ink-3)" }} />
                    {r.projectName}
                  </span>
                )}
                {r.branch && (
                  <span className="inline-flex max-w-48 items-center gap-1 truncate">
                    <GitBranch className="size-3 shrink-0" />
                    <span className="truncate">{r.branch.replace(/^claude\//, "")}</span>
                  </span>
                )}
                <span className="tabular">{rel(r.end)}</span>
                <span className="tabular">{dur(r.end - r.start)}</span>
                {r.turns > 0 && (
                  <span className="tabular">
                    {r.turns} {plural(r.turns, ["message", "messages"], ["message", "messages"])}
                  </span>
                )}
                {r.costUSD != null && <span className="text-ink-2 tabular">{tr(`${r.costUSD.toFixed(2)} $`, `$${r.costUSD.toFixed(2)}`)}</span>}
                {r.costUSD == null && r.tokens != null && <span className="text-ink-2 tabular">{tok(r.tokens)} tokens</span>}
                {r.linesAdded != null && (
                  <span className="tabular">
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
              type="button"
              onClick={() => copy(r)}
              className={cn(
                "inline-flex h-7 shrink-0 items-center gap-1 rounded-md border border-line bg-surface px-2 text-xs text-ink-2 transition-colors hover:bg-hover hover:text-ink",
                "opacity-100 lg:opacity-0 lg:group-hover:opacity-100 lg:focus-visible:opacity-100",
                copied === r.id && "lg:opacity-100",
              )}
              title={r.resume}
            >
              {copied === r.id ? (
                <>
                  <Check className="size-3.5 text-good" /> {tr("copié", "copied")}
                </>
              ) : (
                <>
                  <Terminal className="size-3.5" /> {tr("reprendre", "resume")}
                </>
              )}
            </button>
          </li>
        );
      })}
    </ul>
  );
}
