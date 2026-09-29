"use client";

import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";
import { useCallback, useEffect, useMemo, useSyncExternalStore } from "react";
import { motion } from "motion/react";
import {
  Bot,
  ChevronRight,
  Command,
  Contact,
  FolderPlus,
  GitPullRequest,
  LayoutGrid,
  PanelLeftClose,
  PanelLeftOpen,
  Plus,
  Radar,
  RotateCw,
  Search,
  Settings2,
  SlidersHorizontal,
  SquareTerminal,
  Sun,
  Wallet,
} from "lucide-react";
import { cn } from "@/lib/utils";
import { tr } from "@/lib/i18n";
import { BRAND } from "@/lib/code/brand";
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
import { ZenithMark } from "./logo";
import { AssistantIcon } from "@/components/assistants/assistant-icon";
import { ASSISTANTS, assistantHref, type AssistantId } from "@/lib/assistants";
import { readPref, subscribePrefs, writePref } from "@/lib/prefs";

type NavProject = { id: string; name: string; href: string; color: string; glow: string; emoji: string; tagline: string; dir: string | null };

const COLLAPSED_KEY = "zenith:sidebar";
const FOLDED_KEY = "zenith:sidebar:folded";
const FULL_KEY = "zenith:sidebar:full";
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
 * zenith's one sidebar: the dashboard's pages, your projects with their zenith code
 * threads live underneath, and zenith code's own projects and settings. It folds to a
 * rail (⌘B); the choice is kept per browser and applied before paint (layout.tsx).
 */
export function Sidebar({ projects, code = true, home, assistants = [] }: { projects: NavProject[]; code?: boolean; home: string; assistants?: AssistantId[] }) {
  const path = usePathname();
  const router = useRouter();
  const { status, snapshot, ready } = useCode();
  const collapsed = useSyncExternalStore(subscribePrefs, () => readPref(COLLAPSED_KEY) === "collapsed", () => false);
  const [folded, toggleFolded] = useStoredSet(FOLDED_KEY);
  const [full, toggleFull] = useStoredSet(FULL_KEY);
  const onCode = appPathOf(path) !== null;

  const toggleCollapsed = useCallback(() => {
    const next = readPref(COLLAPSED_KEY) !== "collapsed";
    if (next) document.documentElement.dataset.sidebar = "collapsed";
    else delete document.documentElement.dataset.sidebar;
    writePref(COLLAPSED_KEY, next ? "collapsed" : "expanded");
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "b" || !(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey) return;
      const t = e.target as HTMLElement | null;
      if (t?.isContentEditable || t?.tagName === "INPUT" || t?.tagName === "TEXTAREA") return;
      e.preventDefault();
      toggleCollapsed();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [toggleCollapsed]);

  // zenith code's projects and threads, matched to zenith's projects by folder.
  const { byZenithProject, others, all } = useMemo(() => {
    const threadsOf = new Map<string, CodeThread[]>();
    for (const t of snapshot?.threads ?? []) {
      const k = `${t.environmentId}:${t.projectId}`;
      threadsOf.set(k, [...(threadsOf.get(k) ?? []), t]);
    }
    const entry = (p: CodeProject) => ({ project: p, threads: threadsOf.get(`${p.environmentId}:${p.id}`) ?? [] });
    const byZenithProject = new Map<string, ReturnType<typeof entry>>();
    const others: ReturnType<typeof entry>[] = [];
    for (const p of snapshot?.projects ?? []) {
      const owner = projects.find((z) => z.dir && sameDir(z.dir, p.workspaceRoot));
      if (owner && !byZenithProject.has(owner.id)) byZenithProject.set(owner.id, entry(p));
      else others.push(entry(p));
    }
    return { byZenithProject, others, all: snapshot?.threads ?? [] };
  }, [snapshot, projects]);

  const navigate = useCallback(
    (request: CodeNavigate) => {
      if (!onCode) router.push("/code");
      codeNavigate(request);
    },
    [onCode, router],
  );

  const activeThread = onCode ? snapshot?.activeThread ?? null : null;
  const codeReady = code && !!status?.running && !!snapshot;
  const codeTop = topStatus(all);

  const pages = [
    { href: "/", name: tr("Vue d'ensemble", "Overview"), glow: "#FFD166", icon: <LayoutGrid className="size-4" /> },
    { href: "/vie", name: tr("Ma vie", "My life"), glow: "#FDBA74", icon: <Sun className="size-4" /> },
    { href: "/veille", name: tr("Veille", "Radar"), glow: "#7dd3fc", icon: <Radar className="size-4" /> },
    { href: "/agents", name: tr("Agents IA", "AI agents"), glow: "#D4724F", icon: <Bot className="size-4" /> },
    { href: "/annuaire", name: tr("Annuaire", "Directory"), glow: "#FFD166", icon: <Contact className="size-4" /> },
    { href: "/abonnements", name: tr("Abonnements", "Subscriptions"), glow: "#34d399", icon: <Wallet className="size-4" /> },
  ];
  const isActive = (href: string) => (href === "/" ? path === "/" : path === href || path.startsWith(`${href}/`));
  const codeLink = codeHref(snapshot?.pathname && !snapshot.pathname.startsWith("/pair") ? snapshot.pathname : "/");

  return (
    <>
      <aside className="sticky top-0 z-40 hidden h-screen w-[var(--zenith-sidebar-w)] shrink-0 flex-col border-r border-line bg-black/20 backdrop-blur-xl transition-[width] duration-200 lg:flex">
        <div className="flex items-center gap-3 px-4 pb-4 pt-5 collapsed:flex-col collapsed:px-0">
          <Link href="/" className="group flex min-w-0 flex-1 items-center gap-3 px-1 collapsed:flex-none collapsed:px-0" title="zenith">
            <ZenithMark size={34} />
            <div className="min-w-0 collapsed:hidden">
              <div className="font-display text-base font-black tracking-[0.18em]">zenith</div>
              <div className="truncate font-serif text-[13px] italic text-ink-3">{tr("tout ce qui brille au-dessus", "everything that shines above")}</div>
            </div>
          </Link>
          <button
            type="button"
            onClick={toggleCollapsed}
            title={`${collapsed ? tr("Déplier la barre", "Expand sidebar") : tr("Replier la barre", "Collapse sidebar")} (⌘B)`}
            aria-label={collapsed ? tr("Déplier la barre", "Expand sidebar") : tr("Replier la barre", "Collapse sidebar")}
            className="grid size-7 shrink-0 place-items-center rounded-lg text-ink-3 transition hover:bg-white/[0.06] hover:text-ink"
          >
            {collapsed ? <PanelLeftOpen className="size-4" /> : <PanelLeftClose className="size-4" />}
          </button>
        </div>

        <nav className="flex min-h-0 flex-1 flex-col gap-5 overflow-y-auto overflow-x-hidden px-3 pb-4 [scrollbar-width:thin] collapsed:px-2">
          <div className="flex flex-col gap-0.5">
            {pages.map((it) => (
              <NavRow key={it.href} href={it.href} name={it.name} glow={it.glow} icon={it.icon} active={isActive(it.href)} />
            ))}
          </div>

          {assistants.length > 0 && (
            <Group label={tr("Assistants", "Assistants")}>
              {assistants.map((id) => (
                <NavRow key={id} href={assistantHref(id)} name={ASSISTANTS[id].name} glow={ASSISTANTS[id].color} icon={<AssistantIcon id={id} size={15} />} active={isActive(assistantHref(id))} />
              ))}
            </Group>
          )}

          {projects.length > 0 && (
            <Group label={tr("Projets", "Projects")}>
              {projects.map((p) => {
                const c = byZenithProject.get(p.id);
                return (
                  <ProjectBlock
                    key={p.id}
                    id={p.id}
                    href={p.href}
                    name={p.name}
                    glow={p.glow}
                    active={isActive(p.href)}
                    threads={codeReady ? c?.threads ?? [] : []}
                    activeThread={activeThread}
                    folded={folded.has(p.id)}
                    full={full.has(p.id)}
                    onFold={() => toggleFolded(p.id)}
                    onFull={() => toggleFull(p.id)}
                    onNewThread={c && ready ? () => navigate({ to: "new-thread", environmentId: c.project.environmentId, projectId: c.project.id }) : undefined}
                  />
                );
              })}
            </Group>
          )}

          {code && (
            <Group
              label="Code"
              href={codeLink}
              active={onCode && !activeThread}
              status={codeTop}
              health={status?.running ? "up" : status?.starting ? "busy" : status ? "down" : null}
              actions={
                <>
                  {ready && (
                    <IconButton label={tr("Chercher dans les threads", "Search threads")} onClick={() => navigate({ to: "palette" })}>
                      <Search className="size-3.5" />
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
                      glow={isHome ? "#FFD166" : "#A5B4FC"}
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
              {ready && (
                <>
                  <SmallRow icon={<FolderPlus className="size-3.5" />} label={tr("Ajouter un projet", "Add a project")} onClick={() => navigate({ to: "palette", open: "add-project" })} />
                  <SmallRow icon={<GitPullRequest className="size-3.5" />} label="Pull requests" onClick={() => router.push(codeHref("/pull-requests"))} active={path === "/code/pull-requests"} />
                </>
              )}
            </Group>
          )}
        </nav>

        <div className="flex flex-col gap-0.5 border-t border-line px-3 py-3 collapsed:px-2">
          <button
            onClick={() => window.dispatchEvent(new Event("zenith:command"))}
            title={tr("Aller à…", "Go to…")}
            className="flex items-center gap-3 rounded-xl px-3 py-2 text-sm text-ink-2 hover:bg-white/[0.04] hover:text-ink collapsed:justify-center collapsed:px-0"
          >
            <Command className="size-4 shrink-0" />
            <span className="collapsed:hidden">{tr("Aller à…", "Go to…")}</span>
            <kbd className="ml-auto rounded-md border border-line px-1.5 py-0.5 font-mono text-[10px] text-ink-3 collapsed:hidden">⌘K</kbd>
          </button>
          <FooterLink href="/reglages" active={path.startsWith("/reglages")} icon={<Settings2 className="size-4 shrink-0" />} label={tr("Sources de données", "Data sources")} />
          {code && <FooterLink href="/code/settings" active={path.startsWith("/code/settings")} icon={<SlidersHorizontal className="size-4 shrink-0" />} label={tr("Réglages de code", "Code settings")} />}
        </div>
      </aside>

      {/* Mobile: bottom bar */}
      <nav className="fixed inset-x-3 bottom-3 z-40 flex items-center gap-1 overflow-x-auto rounded-2xl border border-line bg-[#0b0a14]/90 p-2 backdrop-blur-xl [scrollbar-width:none] lg:hidden">
        {[
          ...pages.slice(0, 2),
          ...projects.map((p) => ({ href: p.href, name: p.name, icon: <span className="size-3 rounded-full" style={{ background: p.glow, boxShadow: `0 0 12px ${p.glow}` }} /> })),
          ...(code ? [{ href: "/code", name: "Code", icon: <SquareTerminal className="size-4" /> }] : []),
          ...assistants.map((id) => ({ href: assistantHref(id), name: ASSISTANTS[id].name, icon: <AssistantIcon id={id} size={16} /> })),
          ...pages.slice(2),
        ].map((it) => (
          <Link
            key={it.href}
            href={it.href === "/code" ? codeLink : it.href}
            aria-label={it.name}
            className={cn("grid size-10 shrink-0 place-items-center rounded-xl", isActive(it.href) && "bg-white/10")}
          >
            {it.icon}
          </Link>
        ))}
      </nav>
    </>
  );
}

function NavRow({ href, name, glow, icon, active }: { href: string; name: string; glow: string; icon: React.ReactNode; active: boolean }) {
  return (
    <Link
      href={href}
      title={name}
      className={cn(
        "relative flex items-center gap-3 rounded-xl px-3 py-2 text-sm transition-colors collapsed:justify-center collapsed:px-0",
        active ? "text-ink" : "text-ink-2 hover:bg-white/[0.04] hover:text-ink",
      )}
    >
      {active && (
        <motion.span
          layoutId="nav-active"
          className="absolute inset-0 rounded-xl border border-white/10"
          style={{ background: `linear-gradient(90deg, ${glow}26, transparent 80%)` }}
          transition={{ type: "spring", stiffness: 380, damping: 32 }}
        />
      )}
      <span className="relative grid size-5 shrink-0 place-items-center">{icon}</span>
      <span className="relative truncate collapsed:hidden">{name}</span>
    </Link>
  );
}

function Group({
  label,
  href,
  active,
  status,
  health,
  actions,
  children,
}: {
  label: string;
  href?: string;
  active?: boolean;
  status?: CodeThreadStatus | null;
  health?: "up" | "busy" | "down" | null;
  actions?: React.ReactNode;
  children: React.ReactNode;
}) {
  const heading = (
    <>
      {href && <SquareTerminal className="hidden size-4 collapsed:block" />}
      <span className="collapsed:hidden">{label}</span>
      {health && (
        <span
          className={cn("size-1.5 rounded-full collapsed:absolute collapsed:right-2 collapsed:top-1.5", health === "busy" && "animate-pulse")}
          style={{ background: status ? STATUS_STYLE[status].dot : health === "up" ? "var(--good)" : health === "busy" ? "#7dd3fc" : "var(--ink-3)" }}
        />
      )}
    </>
  );
  return (
    <section className="flex flex-col gap-0.5">
      <div className="group/heading flex h-7 items-center gap-2 px-3 collapsed:justify-center collapsed:px-0">
        {href ? (
          <Link
            href={href}
            title={label}
            className={cn(
              "relative flex items-center gap-2 rounded-md text-[11px] font-medium uppercase tracking-[0.18em] transition-colors collapsed:grid collapsed:size-9 collapsed:place-items-center collapsed:rounded-xl collapsed:text-ink-2 collapsed:hover:bg-white/[0.04]",
              active ? "text-ink" : "text-ink-3 hover:text-ink",
            )}
          >
            {heading}
          </Link>
        ) : (
          <span className="flex items-center gap-2 text-[11px] font-medium uppercase tracking-[0.18em] text-ink-3 collapsed:hidden">{heading}</span>
        )}
        {actions && <div className="ml-auto flex items-center gap-0.5 opacity-0 transition group-hover/heading:opacity-100 focus-within:opacity-100 collapsed:hidden">{actions}</div>}
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
  glow,
  active = false,
  threads,
  activeThread,
  folded,
  full,
  onFold,
  onFull,
  onNewThread,
}: {
  id: string;
  href?: string;
  name: string;
  title?: string;
  glow: string;
  active?: boolean;
  threads: CodeThread[];
  activeThread: string | null;
  folded: boolean;
  full: boolean;
  onFold: () => void;
  onFull: () => void;
  onNewThread?: () => void;
}) {
  const live = threads.filter((t) => t.section === "pinned" || t.section === "active");
  const shown = full ? threads : live.filter((t, i) => i < THREADS_SHOWN || (t.status && NEEDS_YOU.has(t.status)) || threadKey(t) === activeThread);
  const hidden = threads.length - shown.length;
  const top = topStatus(threads);
  const hasThreads = threads.length > 0;
  const openFirst = !href && live[0] ? threadHref(live[0]) : undefined;
  const target = href ?? openFirst;
  const dot = <span className="size-2.5 rounded-full" style={{ background: glow, boxShadow: `0 0 10px ${glow}` }} />;

  const label = (
    <>
      <span className="relative grid size-5 shrink-0 place-items-center">
        {dot}
        {top && <StatusDot status={top} className="absolute -right-0.5 -top-0.5 hidden ring-2 ring-[#0b0a14] collapsed:block" />}
      </span>
      <span className="relative min-w-0 flex-1 truncate collapsed:hidden">{name}</span>
    </>
  );

  return (
    <div className="flex flex-col" data-project={id}>
      <div
        className={cn(
          "group/project relative flex items-center rounded-xl text-sm transition-colors",
          active ? "bg-white/[0.06] text-ink" : "text-ink-2 hover:bg-white/[0.04] hover:text-ink",
        )}
      >
        {target ? (
          <Link href={target} title={title ?? name} className="flex min-w-0 flex-1 items-center gap-3 py-2 pl-3 collapsed:justify-center collapsed:px-0">
            {label}
          </Link>
        ) : (
          <button type="button" onClick={onFold} title={title ?? name} className="flex min-w-0 flex-1 items-center gap-3 py-2 pl-3 text-left collapsed:justify-center collapsed:px-0">
            {label}
          </button>
        )}
        <div className="flex shrink-0 items-center gap-0.5 pr-1.5 collapsed:hidden">
          {top && folded && <StatusDot status={top} className="mr-1.5" />}
          {onNewThread && (
            <IconButton label={tr(`Nouveau thread dans ${name}`, `New thread in ${name}`)} onClick={onNewThread} className="opacity-0 group-hover/project:opacity-100">
              <Plus className="size-3.5" />
            </IconButton>
          )}
          {hasThreads && (
            <IconButton label={folded ? tr("Afficher les threads", "Show threads") : tr("Masquer les threads", "Hide threads")} onClick={onFold} className="opacity-0 group-hover/project:opacity-100">
              <ChevronRight className={cn("size-3.5 transition-transform", !folded && "rotate-90")} />
            </IconButton>
          )}
        </div>
      </div>

      {hasThreads && !folded && (
        <ul className="mb-1 ml-[21px] flex flex-col gap-px border-l border-line pl-2 collapsed:hidden">
          {shown.map((t) => (
            <ThreadRow key={threadKey(t)} thread={t} active={threadKey(t) === activeThread} />
          ))}
          {(hidden > 0 || full) && (
            <li>
              <button type="button" onClick={onFull} className="w-full rounded-lg px-2 py-1 text-left text-xs text-ink-3 transition hover:text-ink-2">
                {full ? tr("Moins", "Show less") : tr(`${hidden} de plus`, `${hidden} more`)}
              </button>
            </li>
          )}
        </ul>
      )}
    </div>
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
          "flex items-center gap-2 rounded-lg px-2 py-1 text-[13px] transition-colors",
          active ? "bg-white/[0.08] text-ink" : quiet ? "text-ink-3 hover:bg-white/[0.04] hover:text-ink-2" : "text-ink-2 hover:bg-white/[0.04] hover:text-ink",
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
  return (
    <span
      role="img"
      aria-label={tr(fr, en)}
      title={tr(fr, en)}
      className={cn("inline-block size-1.5 shrink-0 rounded-full", s.pulse && "animate-pulse", className)}
      style={{ background: s.dot, boxShadow: `0 0 8px ${s.dot}` }}
    />
  );
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
      className={cn("grid size-6 place-items-center rounded-md text-ink-3 transition hover:bg-white/[0.08] hover:text-ink focus-visible:opacity-100", className)}
    >
      {children}
    </button>
  );
}

function SmallRow({ icon, label, onClick, active }: { icon: React.ReactNode; label: string; onClick: () => void; active?: boolean }) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={label}
      className={cn(
        "flex items-center gap-3 rounded-xl px-3 py-1.5 text-left text-[13px] transition-colors collapsed:hidden",
        active ? "text-ink" : "text-ink-3 hover:bg-white/[0.04] hover:text-ink-2",
      )}
    >
      <span className="grid size-5 shrink-0 place-items-center">{icon}</span>
      {label}
    </button>
  );
}

function FooterLink({ href, active, icon, label }: { href: string; active: boolean; icon: React.ReactNode; label: string }) {
  return (
    <Link
      href={href}
      title={label}
      className={cn(
        "flex items-center gap-3 rounded-xl px-3 py-2 text-sm hover:bg-white/[0.04] collapsed:justify-center collapsed:px-0",
        active ? "text-ink" : "text-ink-2 hover:text-ink",
      )}
    >
      {icon}
      <span className="truncate collapsed:hidden">{label}</span>
    </Link>
  );
}
