"use client";

import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";
import { useCallback, useEffect, useMemo, useSyncExternalStore } from "react";
import { ArrowDownToLine, ChevronLeft, ChevronRight, FolderPlus, GitPullRequest, PanelLeft, Plus, RotateCw, Search, Settings, SquarePen } from "lucide-react";
import { cn } from "@/lib/utils";
import { tr } from "@/lib/i18n";
import { BRAND } from "@/lib/code/brand";
import { PAGES, SETTINGS, SPACES, isSettingsPath, spaceOf, type Space } from "@/lib/nav";
import {
  appPathOf,
  codeHref,
  codeNavigate,
  NEEDS_YOU,
  sameDir,
  STATUS_STYLE,
  threadHref,
  threadKey,
  topStatus,
  useCode,
  type CodeNavigate,
  type CodeProject,
  type CodeThread,
  type CodeThreadStatus,
} from "@/components/code/store";
import { readPref, subscribePrefs, writePref } from "@/lib/prefs";
import { openAsk } from "@/components/agent/client";
import { ZenithMark } from "./logo";
import { setDrawer, toggleSidebar, useDrawer } from "./sidebar-state";
import { AgentAvatar } from "@/components/agent/agent-avatar";
import type { Avatar } from "@/lib/agent/avatar";

export type NavProject = { id: string; name: string; href: string; color: string; tagline: string; dir: string | null };

const FOLDED_KEY = "zenith:sidebar:folded";
const FULL_KEY = "zenith:sidebar:full";
const BACK_KEY = "zenith:settings:back";
const SPACE_KEY = "zenith:space:";
// Threads shown under a project before "more": the rest, unless they need you.
const THREADS_SHOWN = 4;

/** A set of ids kept in localStorage (folded projects, expanded thread lists). */
function useStoredSet(key: string) {
  const raw = useSyncExternalStore(subscribePrefs, () => readPref(key), () => null);
  const set = useMemo(() => {
    try {
      return new Set(JSON.parse(raw ?? "[]") as string[]);
    } catch {
      return new Set<string>();
    }
  }, [raw]);
  const toggle = useCallback(
    (id: string) => {
      const next = new Set(set);
      if (!next.delete(id)) next.add(id);
      writePref(key, JSON.stringify([...next]));
    },
    [key, set],
  );
  return [set, toggle] as const;
}

/**
 * zenith's one sidebar, in zenith code's style: the pages, your projects with their
 * threads underneath, zenith code's other projects, and Settings. On settings pages it
 * becomes the settings navigation, for zenith and zenith code alike. ⌘B hides it; below
 * lg it is a drawer.
 */
/** The main agent's folder and its bots', whose threads are your conversations. */
export type SidebarAgent = {
  home: string;
  /** The main agent. */
  name: string;
  title?: string;
  avatar: Avatar;
  bots: { id: string; name: string; title?: string; home: string; color: string; avatar: Avatar }[];
};

/** An update on its way, for a line above Settings. */
export type SidebarUpdate = { state: string; behind: number } | null;

export function Sidebar({ projects, code = true, home, agent = null, update = null }: { projects: NavProject[]; code?: boolean; home: string; agent?: SidebarAgent | null; update?: SidebarUpdate }) {
  const path = usePathname();
  const drawer = useDrawer();
  const settings = isSettingsPath(path);

  useEffect(() => setDrawer(false), [path]);
  // Where "back" leaves settings for.
  useEffect(() => {
    if (!settings) sessionStorage.setItem(BACK_KEY, path);
  }, [path, settings]);

  return (
    <>
      {drawer && <div className="fixed inset-0 z-40 bg-black/30 lg:hidden" onClick={() => setDrawer(false)} aria-hidden />}
      <aside
        className={cn(
          "z-50 h-dvh shrink-0 flex-col overflow-hidden border-r border-line bg-sidebar select-none",
          drawer
            ? "fixed inset-y-0 left-0 flex w-72 shadow-2xl lg:hidden"
            : "hidden w-[var(--sidebar-w)] mac:border-black/10 mac:bg-transparent lg:flex dark:mac:border-black/50 [html[data-sidebar=hidden]_&]:lg:hidden",
        )}
      >
        <div data-drag className="flex h-[var(--titlebar-h)] shrink-0 items-center gap-2 pl-4 pr-2 mac:pl-[84px] [html[data-fullscreen]_&]:pl-4">
          <Link href="/" className="flex min-w-0 items-center gap-2 text-ink" title="zenith">
            <ZenithMark size={16} />
            <span className="text-[13px] font-semibold tracking-tight">zenith</span>
          </Link>
          <button type="button" onClick={toggleSidebar} title={tr("Masquer la barre latérale (⌘B)", "Hide sidebar (⌘B)")} className="ml-auto grid size-7 place-items-center rounded-md text-ink-3 transition hover:bg-hover hover:text-ink">
            <PanelLeft className="size-4" />
          </button>
        </div>
        {settings ? <SettingsNav path={path} /> : <MainNav path={path} projects={projects} code={code} home={home} agent={agent} update={update} />}
      </aside>
    </>
  );
}

function MainNav({ path, projects, code, home, agent, update }: { path: string; projects: NavProject[]; code: boolean; home: string; agent: SidebarAgent | null; update: SidebarUpdate }) {
  const router = useRouter();
  const { status, snapshot, ready } = useCode();
  const [folded, toggleFolded] = useStoredSet(FOLDED_KEY);
  const [full, toggleFull] = useStoredSet(FULL_KEY);
  const onCode = appPathOf(path) !== null;

  // zenith code's projects and threads: your agents' folders, your projects', and the others.
  const { byZenithProject, others, life, team, all } = useMemo(() => {
    const threadsOf = new Map<string, CodeThread[]>();
    for (const t of snapshot?.threads ?? []) {
      const k = `${t.environmentId}:${t.projectId}`;
      threadsOf.set(k, [...(threadsOf.get(k) ?? []), t]);
    }
    const entry = (p: CodeProject) => ({ project: p, threads: threadsOf.get(`${p.environmentId}:${p.id}`) ?? [] });
    const byZenithProject = new Map<string, ReturnType<typeof entry>>();
    const others: ReturnType<typeof entry>[] = [];
    let life: ReturnType<typeof entry> | null = null;
    const team = new Map<string, ReturnType<typeof entry>>();
    for (const p of snapshot?.projects ?? []) {
      if (agent && !life && sameDir(p.workspaceRoot, agent.home)) {
        life = entry(p);
        continue;
      }
      const bot = agent?.bots.find((b) => sameDir(p.workspaceRoot, b.home));
      if (bot && !team.has(bot.id)) {
        team.set(bot.id, entry(p));
        continue;
      }
      const owner = projects.find((z) => z.dir && sameDir(z.dir, p.workspaceRoot));
      if (owner && !byZenithProject.has(owner.id)) byZenithProject.set(owner.id, entry(p));
      else others.push(entry(p));
    }
    return { byZenithProject, others, life, team, all: snapshot?.threads ?? [] };
  }, [snapshot, projects, agent]);

  const navigate = useCallback(
    (request: CodeNavigate) => {
      if (!onCode) router.push("/code");
      codeNavigate(request);
    },
    [onCode, router],
  );

  const activeThread = onCode ? snapshot?.activeThread ?? null : null;
  const agentProjects = useMemo(() => new Set([life?.project.id, ...[...team.values()].map((e) => e.project.id)].filter(Boolean)), [life, team]);
  const agentThread = !!activeThread && all.some((t) => threadKey(t) === activeThread && agentProjects.has(t.projectId));
  const space = spaceOf(path, agentThread);
  const codeReady = code && !!status?.running && !!snapshot;
  const isActive = (href: string) => (href === "/" ? path === "/" : path === href || path.startsWith(`${href}/`));
  const codeLink = codeHref(snapshot?.pathname && !snapshot.pathname.startsWith("/pair") && !snapshot.pathname.startsWith("/settings") ? snapshot.pathname : "/");
  const codeThreads = all.filter((t) => !agentProjects.has(t.projectId));

  // Each space reopens where you left it.
  useEffect(() => {
    sessionStorage.setItem(`${SPACE_KEY}${space}`, path);
  }, [space, path]);
  const go = useCallback(
    (s: Space) => {
      const last = sessionStorage.getItem(`${SPACE_KEY}${s}`);
      router.push(last && spaceOf(last) === s ? last : s === "code" ? codeLink : SPACES().find((x) => x.id === s)!.href);
    },
    [router, codeLink],
  );
  // ⌘1 ⌘2 ⌘3 switch spaces.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey) return;
      const s = SPACES()[Number(e.key) - 1];
      if (!s || (s.id === "team" && !agent) || (s.id === "code" && !code)) return;
      e.preventDefault();
      go(s.id);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [go, agent, code]);

  const pages = PAGES().filter((p) => p.space === space);
  const member = (id: string, name: string, title: string | undefined, avatar: Avatar, entry: { threads: CodeThread[] } | null | undefined, mention: string) => {
    const key = `zenith:agent:${id}`;
    return (
      <ProjectBlock
        key={key}
        id={key}
        name={name}
        title={title ? `${name} · ${title}` : name}
        icon={<AgentAvatar avatar={avatar} id={`side-${id}`} size={16} />}
        color={avatar.color}
        threads={codeReady ? entry?.threads ?? [] : []}
        activeThread={activeThread}
        folded={folded.has(key)}
        full={full.has(key)}
        onFold={() => toggleFolded(key)}
        onFull={() => toggleFull(key)}
        onOpen={entry?.threads.length ? undefined : () => openAsk(mention)}
        onNewThread={() => openAsk(mention)}
        newLabel={tr(`Parler à ${name}`, `Talk to ${name}`)}
      />
    );
  };

  return (
    <>
      <SpaceSwitch space={space} team={!!agent} code={code} onGo={go} status={space !== "code" ? topStatus(codeThreads) : null} />

      <div className="flex flex-col gap-px px-2 pb-2">
        {space === "team" && <Row icon={<SquarePen className="size-4" />} label={tr("Nouvelle conversation", "New conversation")} kbd="⌘J" onClick={() => openAsk()} />}
        {space === "code" && ready && <Row icon={<SquarePen className="size-4" />} label={tr("Nouveau thread", "New thread")} onClick={() => navigate({ to: "palette", open: "new-thread-in" })} />}
        <Row icon={<Search className="size-4" />} label={tr("Rechercher", "Search")} kbd="⌘K" onClick={() => window.dispatchEvent(new Event("zenith:command"))} />
      </div>

      <nav className="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto overflow-x-hidden px-2 pb-3">
        {pages.length > 0 && (
          <div className="flex flex-col gap-px">
            {pages.map((p) => (
              <Row key={p.href} href={p.href} icon={<p.icon className="size-4" />} label={p.short ?? p.name} active={isActive(p.href)} />
            ))}
          </div>
        )}

        {space === "home" && projects.length > 0 && (
          <Group label={tr("Projets", "Projects")}>
            {projects.map((p) => (
              <Row key={p.id} href={p.href} icon={<span className="size-2 rounded-full" style={{ background: p.color }} />} label={p.name} active={isActive(p.href)} />
            ))}
          </Group>
        )}

        {space === "team" && agent && (
          <Group label={tr("Tes agents", "Your agents")}>
            {member("life", agent.name, agent.title, agent.avatar, life, "")}
            {agent.bots.map((b) => member(b.id, b.name, b.title, b.avatar, team.get(b.id), `@${b.name.toLowerCase()} `))}
          </Group>
        )}

        {space === "code" && code && (
          <>
            {projects.some((p) => byZenithProject.has(p.id)) && (
              <Group label={tr("Projets", "Projects")}>
                {projects.map((p) => {
                  const c = byZenithProject.get(p.id);
                  if (!c) return null;
                  return (
                    <ProjectBlock
                      key={p.id}
                      id={p.id}
                      name={p.name}
                      color={p.color}
                      threads={codeReady ? c.threads : []}
                      activeThread={activeThread}
                      folded={folded.has(p.id)}
                      full={full.has(p.id)}
                      onFold={() => toggleFolded(p.id)}
                      onFull={() => toggleFull(p.id)}
                      onNewThread={ready ? () => navigate({ to: "new-thread", environmentId: c.project.environmentId, projectId: c.project.id }) : undefined}
                    />
                  );
                })}
              </Group>
            )}
            <Group
              label={projects.some((p) => byZenithProject.has(p.id)) ? tr("Autres dossiers", "Other folders") : tr("Dossiers", "Folders")}
              href={codeLink}
              status={topStatus(codeThreads)}
              health={status?.running ? "up" : status?.starting ? "busy" : status ? "down" : null}
              actions={
                <>
                  {ready && (
                    <IconButton label={tr("Ajouter un projet", "Add a project")} onClick={() => navigate({ to: "palette", open: "add-project" })}>
                      <FolderPlus className="size-3.5" />
                    </IconButton>
                  )}
                  {status?.enabled && status.built && (
                    <IconButton label={tr(`Redémarrer ${BRAND} code`, `Restart ${BRAND} code`)} onClick={() => window.dispatchEvent(new Event("zenith:code-restart"))}>
                      <RotateCw className="size-3.5" />
                    </IconButton>
                  )}
                </>
              }
            >
              {codeReady &&
                others.map(({ project, threads }) => {
                  const id = `code:${project.environmentId}:${project.id}`;
                  const isHome = sameDir(project.workspaceRoot, home);
                  return (
                    <ProjectBlock
                      key={id}
                      id={id}
                      name={isHome ? "zenith" : project.title}
                      title={project.workspaceRoot}
                      color={isHome ? "var(--foreground)" : "var(--ink-3)"}
                      threads={threads}
                      activeThread={activeThread}
                      folded={folded.has(id)}
                      full={full.has(id)}
                      onFold={() => toggleFolded(id)}
                      onFull={() => toggleFull(id)}
                      onNewThread={ready ? () => navigate({ to: "new-thread", environmentId: project.environmentId, projectId: project.id }) : undefined}
                    />
                  );
                })}
              {ready && <Row icon={<GitPullRequest className="size-4" />} label="Pull requests" href={codeHref("/pull-requests")} active={path === "/code/pull-requests"} quiet />}
              {!status?.running && <p className="px-2 py-1 text-xs text-ink-3">{status?.starting ? tr(`${BRAND} code démarre…`, `${BRAND} code is starting…`) : tr(`${BRAND} code ne tourne pas.`, `${BRAND} code isn't running.`)}</p>}
            </Group>
          </>
        )}
      </nav>

      <div className="flex flex-col gap-px border-t border-line px-2 py-2 mac:border-black/5 dark:mac:border-white/5">
        {update && (
          <Row
            icon={<ArrowDownToLine className="size-4" />}
            href="/reglages#maj"
            label={
              update.state === "available"
                ? tr(`Mise à jour disponible`, `Update available`)
                : update.state === "restart"
                  ? tr("Redémarre pour mettre à jour", "Restart to update")
                  : update.state === "error"
                    ? tr("Mise à jour en échec", "Update failed")
                    : tr("Mise à jour en cours…", "Updating…")
            }
            quiet
          />
        )}
        <Row icon={<Settings className="size-4" />} label={tr("Réglages", "Settings")} kbd="⌘," href="/reglages" />
      </div>
    </>
  );
}

/** Aperçu · Équipe · Code: which of zenith's three spaces you are in, and the way to the others. */
function SpaceSwitch({ space, team, code, onGo, status }: { space: Space; team: boolean; code: boolean; onGo: (s: Space) => void; status: CodeThreadStatus | null }) {
  const list = SPACES().filter((s) => (s.id === "team" ? team : s.id === "code" ? code : true));
  if (list.length < 2) return null;
  return (
    <div className="px-2 pb-2">
      <div role="tablist" aria-label={tr("Espaces", "Spaces")} className="grid gap-0.5 rounded-lg bg-muted p-0.5 mac:bg-black/5 dark:mac:bg-white/5" style={{ gridTemplateColumns: `repeat(${list.length}, minmax(0, 1fr))` }}>
        {list.map((s) => {
          const on = s.id === space;
          return (
            <button
              key={s.id}
              type="button"
              role="tab"
              aria-selected={on}
              onClick={() => onGo(s.id)}
              title={`${s.name} (${s.kbd})`}
              className={cn(
                "relative flex h-7 items-center justify-center gap-1.5 rounded-md text-xs transition-colors",
                on ? "bg-surface font-medium text-ink shadow-xs dark:bg-selected" : "text-ink-3 hover:text-ink-2",
              )}
            >
              <s.icon className="size-3.5" />
              {s.name}
              {s.id === "code" && status && NEEDS_YOU.has(status) && <span className="absolute right-1.5 top-1.5 size-1.5 rounded-full" style={{ background: STATUS_STYLE[status].dot }} />}
            </button>
          );
        })}
      </div>
    </div>
  );
}

/** Settings: zenith's sections, then zenith code's, in one list. */
function SettingsNav({ path }: { path: string }) {
  const router = useRouter();
  const back = () => router.push(sessionStorage.getItem(BACK_KEY) ?? "/");
  return (
    <>
      <div className="px-2 pb-2">
        <Row icon={<ChevronLeft className="size-4" />} label={tr("Retour", "Back")} onClick={back} />
      </div>
      <nav className="flex min-h-0 flex-1 flex-col gap-4 overflow-y-auto px-2 pb-3">
        {SETTINGS().map((g) => (
          <Group key={g.label} label={g.label}>
            {g.items.map((s) => (
              <Row key={s.href} href={s.href} label={s.name} active={path === s.href || (s.href === "/code/settings/general" && path === "/code/settings")} />
            ))}
          </Group>
        ))}
      </nav>
    </>
  );
}

/** One line of the sidebar: a link, or a button. */
function Row({
  href,
  onClick,
  icon,
  label,
  kbd,
  active = false,
  quiet = false,
}: {
  href?: string;
  onClick?: () => void;
  icon?: React.ReactNode;
  label: string;
  kbd?: string;
  active?: boolean;
  quiet?: boolean;
}) {
  const className = cn(
    "flex h-7 items-center gap-2 rounded-md px-2 text-left text-[13px] transition-colors",
    active ? "bg-selected font-medium text-ink" : cn(quiet ? "text-ink-3" : "text-ink-2", "hover:bg-hover hover:text-ink"),
  );
  const body = (
    <>
      {icon && <span className="grid size-4 shrink-0 place-items-center text-ink-3 [.bg-selected_&]:text-ink">{icon}</span>}
      <span className="min-w-0 flex-1 truncate">{label}</span>
      {kbd && <kbd className="font-sans text-2xs text-ink-3">{kbd}</kbd>}
    </>
  );
  return href ? (
    <Link href={href} title={label} className={className} aria-current={active ? "page" : undefined}>
      {body}
    </Link>
  ) : (
    <button type="button" onClick={onClick} title={label} className={className}>
      {body}
    </button>
  );
}

function Group({
  label,
  href,
  status,
  health,
  actions,
  children,
}: {
  label: string;
  href?: string;
  status?: CodeThreadStatus | null;
  health?: "up" | "busy" | "down" | null;
  actions?: React.ReactNode;
  children: React.ReactNode;
}) {
  const heading = (
    <>
      {label}
      {health && (
        <span
          className={cn("size-1.5 rounded-full", health === "busy" && "animate-pulse")}
          style={{ background: status ? STATUS_STYLE[status].dot : health === "up" ? "var(--good)" : health === "busy" ? "#0ea5e9" : "var(--ink-3)" }}
        />
      )}
    </>
  );
  return (
    <section className="flex flex-col gap-px">
      <div className="group/heading flex h-6 items-center gap-2 px-2">
        {href ? (
          <Link href={href} className="flex items-center gap-1.5 text-2xs font-medium text-ink-3 transition-colors hover:text-ink">
            {heading}
          </Link>
        ) : (
          <span className="flex items-center gap-1.5 text-2xs font-medium text-ink-3">{heading}</span>
        )}
        {actions && <div className="ml-auto flex items-center opacity-0 transition group-hover/heading:opacity-100 focus-within:opacity-100">{actions}</div>}
      </div>
      {children}
    </section>
  );
}

function ProjectBlock({
  id,
  href,
  name,
  title,
  color,
  active = false,
  threads,
  activeThread,
  folded,
  full,
  onFold,
  onFull,
  onNewThread,
  onOpen,
  newLabel,
  icon,
}: {
  id: string;
  href?: string;
  name: string;
  title?: string;
  /** Drawn instead of the color dot (an agent's face). */
  icon?: React.ReactNode;
  color: string;
  active?: boolean;
  threads: CodeThread[];
  activeThread: string | null;
  folded: boolean;
  full: boolean;
  onFold: () => void;
  onFull: () => void;
  onNewThread?: () => void;
  /** What a click on the name does when it has no page and no thread to open. */
  onOpen?: () => void;
  newLabel?: string;
}) {
  const live = threads.filter((t) => t.section === "pinned" || t.section === "active");
  const top = topStatus(threads);
  const hasThreads = threads.length > 0;
  const target = href ?? (live[0] ? threadHref(live[0]) : undefined);
  const label = (
    <>
      <span className="grid size-4 shrink-0 place-items-center">{icon ?? <span className="size-2 rounded-full" style={{ background: color }} />}</span>
      <span className="min-w-0 flex-1 truncate">{name}</span>
    </>
  );
  const rowClass = "flex h-7 min-w-0 flex-1 items-center gap-2 pl-2 text-left";

  return (
    <div className="flex flex-col" data-project={id}>
      <div className={cn("group/project flex items-center rounded-md text-[13px] transition-colors", active ? "bg-selected font-medium text-ink" : "text-ink-2 hover:bg-hover hover:text-ink")}>
        {target ? (
          <Link href={target} title={title ?? name} className={rowClass}>
            {label}
          </Link>
        ) : (
          <button type="button" onClick={onOpen ?? onFold} title={title ?? name} className={rowClass}>
            {label}
          </button>
        )}
        <div className="flex shrink-0 items-center pr-1">
          {top && (folded || !hasThreads) && <StatusDot status={top} className="mr-1.5 group-hover/project:hidden" />}
          {onNewThread && (
            <IconButton label={newLabel ?? tr(`Nouveau thread dans ${name}`, `New thread in ${name}`)} onClick={onNewThread} className="hidden group-hover/project:grid">
              <Plus className="size-3.5" />
            </IconButton>
          )}
          {hasThreads && (
            <IconButton label={folded ? tr("Afficher les threads", "Show threads") : tr("Masquer les threads", "Hide threads")} onClick={onFold} className="hidden group-hover/project:grid">
              <ChevronRight className={cn("size-3.5 transition-transform", !folded && "rotate-90")} />
            </IconButton>
          )}
        </div>
      </div>
      {hasThreads && !folded && <Threads id={id} threads={threads} activeThread={activeThread} full={full} onFull={onFull} indent />}
    </div>
  );
}

function Threads({ threads, activeThread, full, onFull, indent = false }: { id: string; threads: CodeThread[]; activeThread: string | null; full: boolean; onFull: () => void; indent?: boolean }) {
  const live = threads.filter((t) => t.section === "pinned" || t.section === "active");
  const shown = full ? threads : live.filter((t, i) => i < THREADS_SHOWN || (t.status && NEEDS_YOU.has(t.status)) || threadKey(t) === activeThread);
  const hidden = threads.length - shown.length;
  return (
    <ul className={cn("flex flex-col gap-px", indent && "pl-6")}>
      {shown.map((t) => (
        <ThreadRow key={threadKey(t)} thread={t} active={threadKey(t) === activeThread} />
      ))}
      {(hidden > 0 || full) && (
        <li>
          <button type="button" onClick={onFull} className="h-6 w-full rounded-md px-2 text-left text-xs text-ink-3 transition hover:text-ink-2">
            {full ? tr("Moins", "Show less") : tr(`${hidden} de plus`, `${hidden} more`)}
          </button>
        </li>
      )}
    </ul>
  );
}

function ThreadRow({ thread, active }: { thread: CodeThread; active: boolean }) {
  const quiet = thread.section === "settled" || thread.section === "snoozed";
  return (
    <li>
      <Link
        href={threadHref(thread)}
        title={thread.branch ? `${thread.title} · ${thread.branch}` : thread.title}
        className={cn(
          "flex h-7 items-center gap-2 rounded-md px-2 text-[13px] transition-colors",
          active ? "bg-selected text-ink" : quiet ? "text-ink-3 hover:bg-hover hover:text-ink-2" : "text-ink-2 hover:bg-hover hover:text-ink",
        )}
      >
        <span className="min-w-0 flex-1 truncate">{thread.title}</span>
        {thread.status && <StatusDot status={thread.status} />}
      </Link>
    </li>
  );
}

function StatusDot({ status, className }: { status: CodeThreadStatus; className?: string }) {
  const s = STATUS_STYLE[status];
  const [fr, en] = s.label();
  return <span role="img" aria-label={tr(fr, en)} title={tr(fr, en)} className={cn("inline-block size-1.5 shrink-0 rounded-full", s.pulse && "animate-pulse", className)} style={{ background: s.dot }} />;
}

function IconButton({ label, onClick, className, children }: { label: string; onClick: () => void; className?: string; children: React.ReactNode }) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      onClick={(e) => {
        e.preventDefault();
        e.stopPropagation();
        onClick();
      }}
      className={cn("grid size-6 place-items-center rounded-md text-ink-3 transition hover:bg-hover hover:text-ink", className)}
    >
      {children}
    </button>
  );
}
