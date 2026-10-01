// @effect-diagnostics globalDate:off -- Relative times and durations are wall-clock, like the usage page's.
/**
 * zenith: the Sessions page. Every Claude Code and Codex session of this machine (live
 * state, cost, tokens, lines, PRs, subagents, branch, project, the command to resume it),
 * read by zenith code's Rust server from the agents' own logs. Costs and limits stay on
 * the Usage page; this is the per-session view next to it.
 */
import { CheckIcon, GitBranchIcon, ServerIcon, TerminalIcon } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { formatCount, formatDayShort, formatTokens, formatUsd } from "@t3tools/shared/usageFormat";

import { isElectron } from "../env";
import { cn } from "../lib/utils";
import { useCopyToClipboard } from "../hooks/useCopyToClipboard";
import { Button } from "../components/ui/button";
import { RefreshIcon } from "../components/ui/refresh-icon";
import { ScrollArea } from "../components/ui/scroll-area";
import { SidebarInset } from "../components/ui/sidebar";
import { Skeleton } from "../components/ui/skeleton";
import { Tooltip, TooltipPopup, TooltipTrigger } from "../components/ui/tooltip";
import { WorkspaceBreadcrumb, WorkspaceBreadcrumbItem } from "../components/WorkspaceBreadcrumb";
import { WorkspacePageContainer } from "../components/WorkspacePageContainer";
import { WorkspacePageHeader } from "../components/WorkspacePageHeader";
import { PROVIDER_PRESENTATION } from "../components/usage/usageProviders";
import { PullRequestGlyph } from "../components/pullRequest/pullRequestIcons";
import {
  loadZenithSessions,
  type ZenithSession,
  type ZenithSessionAgent,
  type ZenithSessionsLoad,
  type ZenithSessionTotals,
} from "./sessions";

/** How often the page reads the sessions again while it is visible. */
const REFRESH_MS = 30_000;

const AGENTS: Record<ZenithSessionAgent, (typeof PROVIDER_PRESENTATION)["claude" | "codex"]> = {
  claude: PROVIDER_PRESENTATION.claude,
  codex: PROVIDER_PRESENTATION.codex,
};

function useZenithSessions() {
  const [load, setLoad] = useState<ZenithSessionsLoad | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const controller = useRef<AbortController | null>(null);

  const refresh = useCallback(async () => {
    controller.current?.abort();
    const abort = new AbortController();
    controller.current = abort;
    setRefreshing(true);
    try {
      const next = await loadZenithSessions({ signal: abort.signal });
      if (!abort.signal.aborted) setLoad(next);
    } catch {
      // Aborted by a newer read or by leaving the page.
    } finally {
      if (controller.current === abort) setRefreshing(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => {
      if (document.visibilityState === "visible") void refresh();
    }, REFRESH_MS);
    const onVisible = () => {
      if (document.visibilityState === "visible") void refresh();
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisible);
      controller.current?.abort();
    };
  }, [refresh]);

  return { load, refreshing, refresh };
}

export function SessionsPage() {
  const { load, refreshing, refresh } = useZenithSessions();
  const data = load?.status === "ok" ? load.data : null;

  const summary = data ? summaryLine(data.totals) : "Claude Code and Codex, read from this Mac.";
  const live = useMemo(() => data?.sessions.filter((s) => s.live) ?? [], [data]);
  const history = useMemo(() => data?.sessions.filter((s) => !s.live) ?? [], [data]);

  return (
    <SidebarInset className="h-dvh min-h-0 overflow-hidden overscroll-y-none isolate">
      <div className="flex min-h-0 min-w-0 flex-1 flex-col bg-background text-foreground">
        <WorkspacePageHeader electron={isElectron} className="h-auto">
          <div className="flex w-full min-w-0 items-center gap-3 py-2">
            <WorkspaceBreadcrumb ariaLabel="Sessions breadcrumb" className="min-w-0">
              <WorkspaceBreadcrumbItem current>
                <h1>Sessions</h1>
              </WorkspaceBreadcrumbItem>
            </WorkspaceBreadcrumb>
            <span className="hidden min-w-0 truncate text-xs text-muted-foreground sm:block">
              {summary}
            </span>
            <Button
              className="ms-auto"
              onClick={() => void refresh()}
              aria-label="Refresh sessions"
              aria-busy={refreshing}
              disabled={refreshing}
              size="icon-sm"
              variant="ghost"
            >
              <RefreshIcon size="sm" refreshing={refreshing} />
            </Button>
          </div>
        </WorkspacePageHeader>

        <ScrollArea className="min-h-0 flex-1">
          <WorkspacePageContainer width="wide">
            {load === null ? (
              <SessionsSkeleton />
            ) : load.status === "unavailable" ? (
              <UnavailableNotice />
            ) : load.status === "error" ? (
              <p className="text-sm text-muted-foreground">{load.message}</p>
            ) : (
              <>
                <TotalsStrip totals={load.data.totals} />

                {live.length > 0 ? (
                  <section className="flex flex-col gap-2">
                    <h2 className="flex items-center gap-2 text-sm font-medium text-foreground">
                      <LiveDot />
                      Running
                      <span className="font-normal text-muted-foreground tabular-nums">
                        {live.length}
                      </span>
                    </h2>
                    <SessionList sessions={live} now={load.data.generatedAt} />
                  </section>
                ) : null}

                <section className="grid gap-6 lg:grid-cols-[minmax(0,2fr)_minmax(0,1fr)]">
                  <div className="flex min-w-0 flex-col gap-3">
                    <div className="flex items-baseline justify-between gap-3">
                      <h2 className="text-sm font-medium text-foreground">Sessions per day</h2>
                      <span className="text-xs text-muted-foreground">
                        {load.data.totals.perDay.length} days
                      </span>
                    </div>
                    <PerDayChart days={load.data.totals.perDay} />
                  </div>
                  <div className="flex min-w-0 flex-col gap-3">
                    <div className="flex items-baseline justify-between gap-3">
                      <h2 className="text-sm font-medium text-foreground">Per project</h2>
                      <span className="text-xs text-muted-foreground">sessions · cost</span>
                    </div>
                    <PerProject rows={load.data.totals.perProject} />
                  </div>
                </section>

                <section className="flex flex-col gap-2">
                  <div className="flex items-baseline justify-between gap-3">
                    <h2 className="text-sm font-medium text-foreground">History</h2>
                    <span className="text-xs text-muted-foreground tabular-nums">
                      {load.data.totals.sessions > load.data.sessions.length
                        ? `latest ${formatCount(history.length)} of ${formatCount(load.data.totals.sessions - live.length)}`
                        : formatCount(history.length)}
                    </span>
                  </div>
                  <SessionList sessions={history} now={load.data.generatedAt} />
                </section>
              </>
            )}
          </WorkspacePageContainer>
        </ScrollArea>
      </div>
    </SidebarInset>
  );
}

function summaryLine(totals: ZenithSessionTotals): string {
  return [
    totals.live > 0 ? `${totals.live} running` : "none running",
    `${totals.today} ${totals.today === 1 ? "session" : "sessions"} today`,
    totals.week.costUSD > 0 ? `${formatUsd(totals.week.costUSD)} this week` : null,
  ]
    .filter(Boolean)
    .join(" · ");
}

function UnavailableNotice() {
  return (
    <div className="flex flex-col items-center gap-3 py-16 text-center">
      <span className="grid size-9 place-items-center rounded-lg border border-border bg-muted/40 text-muted-foreground">
        <ServerIcon className="size-4" aria-hidden />
      </span>
      <div className="flex max-w-sm flex-col gap-1">
        <p className="text-sm font-medium text-foreground">
          Sessions are available with zenith&apos;s Rust server
        </p>
        <p className="text-xs text-muted-foreground">
          It reads every Claude Code and Codex session of this Mac from their own logs. This server
          does not list them yet; costs and limits are on the Usage page.
        </p>
      </div>
    </div>
  );
}

function LiveDot() {
  return (
    <span className="relative flex size-2">
      <span className="absolute inline-flex size-full animate-ping rounded-full bg-success opacity-60 motion-reduce:animate-none" />
      <span className="relative inline-flex size-2 rounded-full bg-success" />
    </span>
  );
}

function Metric({
  label,
  value,
  hint,
}: {
  readonly label: ReactNode;
  readonly value: string;
  readonly hint?: string;
}) {
  return (
    <div className="flex min-w-0 flex-col gap-0.5">
      <span className="flex items-center gap-1.5 text-xs text-muted-foreground">{label}</span>
      <span className="text-base font-medium text-foreground tabular-nums">{value}</span>
      {hint ? <span className="truncate text-2xs text-muted-foreground">{hint}</span> : null}
    </div>
  );
}

function TotalsStrip({ totals }: { readonly totals: ZenithSessionTotals }) {
  const week = totals.week;
  return (
    <section className="grid grid-cols-2 gap-x-6 gap-y-4 py-1 sm:grid-cols-3 lg:grid-cols-6">
      <Metric
        label={
          <>
            {totals.live > 0 ? <LiveDot /> : null}
            Running
          </>
        }
        value={formatCount(totals.live)}
        hint="active < 3 min"
      />
      <Metric label="Today" value={formatCount(totals.today)} hint="sessions started" />
      <Metric
        label="7 days"
        value={formatCount(week.sessions)}
        hint={`${formatCount(totals.sessions)} in total`}
      />
      <Metric label="Claude cost · 7 d" value={formatUsd(week.costUSD)} hint="API equivalent" />
      <Metric
        label="Lines · 7 d"
        value={`+${formatTokens(week.linesAdded)}`}
        hint={`−${formatCount(week.linesRemoved)} removed`}
      />
      <Metric label="PRs · 7 d" value={formatCount(week.prs)} hint="opened by agents" />
    </section>
  );
}

function PerDayChart({ days }: { readonly days: ZenithSessionTotals["perDay"] }) {
  const max = Math.max(1, ...days.map((d) => d.claude + d.codex));
  return (
    <div className="flex flex-col gap-1.5">
      <div className="flex h-36 items-end gap-1" role="img" aria-label="Sessions started per day">
        {days.map((d) => {
          const total = d.claude + d.codex;
          return (
            <Tooltip key={d.day}>
              <TooltipTrigger
                render={
                  <div className="flex h-full min-w-0 flex-1 flex-col justify-end rounded-xs hover:bg-muted/50">
                    <div
                      className="flex w-full flex-col-reverse overflow-hidden rounded-xs"
                      style={{ height: `${(total / max) * 100}%` }}
                    >
                      {(["claude", "codex"] as const).map((agent) =>
                        d[agent] > 0 ? (
                          <div
                            key={agent}
                            style={{
                              height: `${(d[agent] / total) * 100}%`,
                              background: AGENTS[agent].color,
                            }}
                          />
                        ) : null,
                      )}
                    </div>
                  </div>
                }
              />
              <TooltipPopup side="top">
                <div className="flex flex-col gap-0.5 text-xs">
                  <span className="font-medium">{formatDayShort(d.day)}</span>
                  {(["claude", "codex"] as const).map((agent) => (
                    <span key={agent} className="flex items-center gap-1.5 tabular-nums">
                      <span
                        className="size-2 rounded-full"
                        style={{ background: AGENTS[agent].color }}
                      />
                      {AGENTS[agent].label}: {d[agent]}
                    </span>
                  ))}
                </div>
              </TooltipPopup>
            </Tooltip>
          );
        })}
      </div>
      <div className="flex justify-between text-2xs text-muted-foreground tabular-nums">
        <span>{days[0] ? formatDayShort(days[0].day) : ""}</span>
        <span className="flex items-center gap-3">
          {(["claude", "codex"] as const).map((agent) => (
            <span key={agent} className="flex items-center gap-1.5">
              <span className="size-2 rounded-full" style={{ background: AGENTS[agent].color }} />
              {AGENTS[agent].label}
            </span>
          ))}
        </span>
        <span>Today</span>
      </div>
    </div>
  );
}

function PerProject({ rows }: { readonly rows: ZenithSessionTotals["perProject"] }) {
  if (rows.length === 0) {
    return <p className="text-sm text-muted-foreground">No sessions yet.</p>;
  }
  const max = Math.max(1, ...rows.map((r) => r.sessions));
  return (
    <ul className="flex flex-col gap-2">
      {rows.slice(0, 8).map((r) => (
        <li key={r.project?.id ?? "none"} className="flex flex-col gap-1">
          <div className="flex items-baseline justify-between gap-3 text-sm">
            <span
              className={cn("truncate", r.project ? "text-foreground" : "text-muted-foreground")}
            >
              {r.project?.title ?? "No project"}
            </span>
            <span className="shrink-0 text-xs text-muted-foreground tabular-nums">
              {formatCount(r.sessions)}
              {r.costUSD > 0 ? ` · ${formatUsd(r.costUSD)}` : ""}
            </span>
          </div>
          <div className="h-1 overflow-hidden rounded-full bg-muted">
            <div
              className="h-full rounded-full bg-muted-foreground/60"
              style={{ width: `${(r.sessions / max) * 100}%` }}
            />
          </div>
        </li>
      ))}
    </ul>
  );
}

function relative(t: number, now: number): string {
  const m = Math.round((now - t) / 60_000);
  if (m < 1) return "just now";
  if (m < 60) return `${m} min ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h} h ago`;
  return `${Math.round(h / 24)} d ago`;
}

function duration(ms: number): string {
  const m = Math.max(0, Math.round(ms / 60_000));
  if (m < 60) return `${m} min`;
  return `${Math.floor(m / 60)} h ${String(m % 60).padStart(2, "0")}`;
}

function SessionList({
  sessions,
  now,
}: {
  readonly sessions: ReadonlyArray<ZenithSession>;
  /** When the list was read: relative times count from it. */
  readonly now: number;
}) {
  if (sessions.length === 0) {
    return <p className="py-2 text-sm text-muted-foreground">No sessions yet.</p>;
  }
  return (
    <ul className="divide-y divide-border/60 border-y border-border/60">
      {sessions.map((s) => (
        <SessionRow key={`${s.agent}:${s.id}`} session={s} now={now} />
      ))}
    </ul>
  );
}

function SessionRow({
  session: s,
  now,
}: {
  readonly session: ZenithSession;
  readonly now: number;
}) {
  const agent = AGENTS[s.agent];
  const Mark = agent.mark;
  const { copyToClipboard, isCopied } = useCopyToClipboard();
  return (
    <li className="group flex items-center gap-3 py-2.5">
      <span className="relative grid size-7 shrink-0 place-items-center rounded-md border border-border bg-muted/40">
        <Mark className="size-3.5" aria-label={agent.label} />
        {s.live ? (
          <span className="absolute -top-0.5 -right-0.5 size-2 rounded-full bg-success ring-2 ring-background" />
        ) : null}
      </span>
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate text-sm font-medium text-foreground">{s.title}</span>
          {s.live ? (
            <span className="shrink-0 rounded-sm bg-success/8 px-1.5 py-px text-2xs font-medium text-success-foreground dark:bg-success/16">
              live
            </span>
          ) : null}
        </div>
        <div className="mt-0.5 flex flex-wrap items-center gap-x-2.5 gap-y-0.5 text-xs text-muted-foreground">
          <span>{agent.label}</span>
          {s.project ? (
            <span className="inline-flex items-center gap-1.5">
              <span className="size-1.5 rounded-full bg-muted-foreground/60" />
              {s.project.title}
            </span>
          ) : null}
          {s.branch ? (
            <span className="inline-flex max-w-48 items-center gap-1">
              <GitBranchIcon className="size-3 shrink-0" aria-hidden />
              <span className="truncate">{s.branch.replace(/^claude\//, "")}</span>
            </span>
          ) : null}
          <span className="tabular-nums">{relative(s.end, now)}</span>
          <span className="tabular-nums">
            {duration(s.end - s.start)}
            {s.activeMs > 0 && s.end - s.start > 2 * s.activeMs
              ? ` · ${duration(s.activeMs)} active`
              : ""}
          </span>
          {s.turns > 0 ? (
            <span className="tabular-nums">
              {s.turns} {s.turns === 1 ? "message" : "messages"}
            </span>
          ) : null}
          {s.model ? <span className="hidden md:inline">{s.model}</span> : null}
          {s.costUSD != null ? (
            <span className="text-foreground/80 tabular-nums">{formatUsd(s.costUSD)}</span>
          ) : s.tokens != null ? (
            <span className="text-foreground/80 tabular-nums">{formatTokens(s.tokens)} tokens</span>
          ) : null}
          {s.linesAdded != null ? (
            <span className="tabular-nums">
              <span className="text-success-foreground">+{formatCount(s.linesAdded)}</span>{" "}
              <span className="text-destructive-foreground">
                −{formatCount(s.linesRemoved ?? 0)}
              </span>
            </span>
          ) : null}
          {s.subagents > 0 ? (
            <span>
              {s.subagents} {s.subagents === 1 ? "subagent" : "subagents"}
            </span>
          ) : null}
          {s.prs.map((pr) => (
            <a
              key={pr.number}
              href={pr.url}
              target="_blank"
              rel="noopener noreferrer"
              className="inline-flex items-center gap-1 text-foreground/80 hover:text-foreground"
            >
              <PullRequestGlyph.pullRequest className="size-3" aria-hidden />#{pr.number}
            </a>
          ))}
        </div>
      </div>
      {/* Shown on hover on wide screens, always on narrow ones (no hover there). */}
      <span
        className={cn(
          "shrink-0 lg:opacity-0 lg:group-hover:opacity-100 lg:focus-within:opacity-100",
          isCopied && "lg:opacity-100",
        )}
      >
        <Tooltip>
          <TooltipTrigger
            render={
              <Button
                size="compact"
                variant="ghost-muted"
                onClick={() => copyToClipboard(s.resume, undefined)}
                aria-label={`Copy the command that resumes ${s.title}`}
              >
                {isCopied ? (
                  <CheckIcon className="text-success-foreground" aria-hidden />
                ) : (
                  <TerminalIcon aria-hidden />
                )}
                {isCopied ? "Copied" : "Resume"}
              </Button>
            }
          />
          <TooltipPopup side="left" variant="code" className="max-w-md break-all">
            {s.resume}
          </TooltipPopup>
        </Tooltip>
      </span>
    </li>
  );
}

function SessionsSkeleton() {
  return (
    <>
      <div className="grid grid-cols-2 gap-x-6 gap-y-4 py-1 sm:grid-cols-3 lg:grid-cols-6">
        {Array.from({ length: 6 }, (_, i) => (
          <div key={i} className="flex flex-col gap-1">
            <Skeleton className="h-3.5 w-16" />
            <Skeleton className="h-5 w-12" />
          </div>
        ))}
      </div>
      <Skeleton className="h-40" />
      <div className="flex flex-col gap-3">
        {Array.from({ length: 6 }, (_, i) => (
          <div key={i} className="flex items-center gap-3">
            <Skeleton className="size-7 shrink-0" />
            <div className="flex flex-1 flex-col gap-1">
              <Skeleton className="h-4 w-2/3" />
              <Skeleton className="h-3 w-1/2" />
            </div>
          </div>
        ))}
      </div>
    </>
  );
}
