"use client";

import { Command, defaultFilter } from "cmdk";
import { useRouter } from "next/navigation";
import { useEffect, useState } from "react";
import { ArrowUpRight, LoaderCircle, ExternalLink, FolderPlus, MessageSquarePlus, RefreshCw, RotateCw, Settings, SquarePen, SquareTerminal } from "lucide-react";
import { tr } from "@/lib/i18n";
import { PAGES, SETTINGS } from "@/lib/nav";
import { codeHref, codeNavigate, STATUS_STYLE, threadHref, threadKey, useCode } from "@/components/code/store";
import { guessTarget, type AgentTarget, type Provider } from "@/lib/agent/target";
import { askZenith, openAsk } from "@/components/agent/client";

type NavProject = { id: string; name: string; href: string; color: string; tagline: string };
type ExtLink = { label: string; url: string; project: string; color: string };

type Agent = { targets: AgentTarget[]; provider: Provider } | null;

// The "Ask zenith" row always matches, but last: a sentence nothing else matches lands on it.
const filter = (value: string, search: string, keywords?: string[]) => (value.startsWith("ask:") ? 0.0001 : defaultFilter(value, search, keywords));

export function CommandMenu({ projects, links, code = true, agent = null }: { projects: NavProject[]; links: ExtLink[]; code?: boolean; agent?: Agent }) {
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState("");
  const [asking, setAsking] = useState(false);
  const [askError, setAskError] = useState<string | null>(null);
  const router = useRouter();
  const { status, snapshot, ready } = useCode();
  const codeProjects = new Map((snapshot?.projects ?? []).map((p) => [`${p.environmentId}:${p.id}`, p.title]));
  const threads = snapshot?.threads.filter((t) => t.section !== "settled").slice(0, 40) ?? [];
  const inCode = (fn: () => void) => {
    router.push(codeHref(snapshot?.pathname ?? "/"));
    fn();
  };

  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if (e.key === "k" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        setOpen((o) => !o);
      }
    };
    const toggle = () => setOpen(true);
    window.addEventListener("keydown", key);
    window.addEventListener("zenith:command", toggle);
    return () => {
      window.removeEventListener("keydown", key);
      window.removeEventListener("zenith:command", toggle);
    };
  }, []);

  const go = (fn: () => void) => {
    setOpen(false);
    fn();
  };

  const askTarget = agent && search.trim() ? agent.targets.find((t) => t.id === guessTarget(search, agent.targets)) : null;
  async function ask() {
    if (!agent || asking || !search.trim()) return;
    setAsking(true);
    setAskError(null);
    try {
      const r = await askZenith({ prompt: search.trim(), provider: agent.provider, source: "command" });
      setOpen(false);
      setSearch("");
      router.push(r.href);
    } catch (e) {
      setAskError(e instanceof Error ? e.message : String(e));
    } finally {
      setAsking(false);
    }
  }

  const heading =
    "[&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:pb-1 [&_[cmdk-group-heading]]:pt-2 [&_[cmdk-group-heading]]:text-2xs [&_[cmdk-group-heading]]:font-medium [&_[cmdk-group-heading]]:text-ink-3";
  const item =
    "flex h-8 cursor-pointer items-center gap-2.5 rounded-md px-2 text-[13px] text-ink-2 [&>svg]:text-ink-3 data-[selected=true]:bg-selected data-[selected=true]:text-ink";

  return (
    <Command.Dialog
      open={open}
      onOpenChange={(o) => {
        setOpen(o);
        if (!o) {
          setSearch("");
          setAskError(null);
        }
      }}
      filter={filter}
      label={tr("Aller à", "Go to")}
      overlayClassName="fixed inset-0 z-50 bg-black/20 dark:bg-black/50"
      contentClassName="fixed left-1/2 top-[14vh] z-50 w-[min(600px,calc(100vw-2rem))] -translate-x-1/2 overflow-hidden rounded-xl border border-line bg-popover shadow-2xl shadow-black/20"
    >
      <Command.Input
        autoFocus
        value={search}
        onValueChange={(v) => {
          setSearch(v);
          setAskError(null);
        }}
        placeholder={agent ? tr("Aller quelque part, ou demander quelque chose à zenith…", "Go somewhere, or ask zenith anything…") : tr("Projet, thread, lien, action…", "Project, thread, link, action…")}
        className="h-12 w-full border-b border-line bg-transparent px-4 text-sm outline-none placeholder:text-ink-3"
      />
      <Command.List className="max-h-[min(420px,55vh)] overflow-y-auto p-1.5">
        <Command.Empty className="px-4 py-6 text-center text-[13px] text-ink-3">{tr("Aucun résultat.", "No results.")}</Command.Empty>
        {agent && search.trim() && (
          <Command.Group heading="zenith" className={heading}>
            <Command.Item value={`ask:${search}`} className={item} onSelect={() => void ask()} disabled={asking}>
              {asking ? <LoaderCircle className="size-4 shrink-0 animate-spin" /> : <SquarePen className="size-4 shrink-0" />}
              <span className="min-w-0 truncate">
                {tr("Demander à zenith : ", "Ask zenith: ")}
                <span className="text-ink">{search.trim()}</span>
              </span>
              {askTarget && (
                <span className="ml-auto inline-flex shrink-0 items-center gap-1.5 text-xs text-ink-3">
                  <span className="size-1.5 rounded-full" style={{ background: askTarget.color }} /> {askTarget.name}
                </span>
              )}
            </Command.Item>
            {askError && <div className="px-3 pb-2 text-xs text-bad">{askError}</div>}
          </Command.Group>
        )}
        <Command.Group heading={tr("Pages", "Pages")} className={heading}>
          {PAGES().map((p) => (
            <Command.Item key={p.href} value={`${p.name} ${p.keywords}`} className={item} onSelect={() => go(() => router.push(p.href))}>
              <p.icon className="size-4" /> {p.name}
            </Command.Item>
          ))}
          {code && (
            <Command.Item value={tr("zenith code coder agents terminal éditeur", "zenith code coding agents terminal editor")} className={item} onSelect={() => go(() => router.push(codeHref(snapshot?.pathname ?? "/")))}>
              <SquareTerminal className="size-4" /> Code
            </Command.Item>
          )}
        </Command.Group>
        {projects.length > 0 && (
          <Command.Group heading={tr("Projets", "Projects")} className={heading}>
            {projects.map((p) => (
              <Command.Item key={p.id} value={`${p.name} ${p.tagline}`} className={item} onSelect={() => go(() => router.push(p.href))}>
                <span className="grid size-4 place-items-center">
                  <span className="size-2 rounded-full" style={{ background: p.color }} />
                </span>
                {p.name}
                <span className="ml-auto truncate text-xs text-ink-3">{p.tagline}</span>
              </Command.Item>
            ))}
          </Command.Group>
        )}
        {code && threads.length > 0 && (
          <Command.Group heading="Threads" className={heading}>
            {threads.map((t) => {
              const project = codeProjects.get(`${t.environmentId}:${t.projectId}`) ?? "";
              const s = t.status ? STATUS_STYLE[t.status] : null;
              return (
                <Command.Item key={threadKey(t)} value={`thread ${t.title} ${project} ${t.branch ?? ""} ${threadKey(t)}`} className={item} onSelect={() => go(() => router.push(threadHref(t)))}>
                  <SquareTerminal className="size-4 shrink-0" />
                  <span className="min-w-0 truncate">{t.title}</span>
                  {s && <span className="size-1.5 shrink-0 rounded-full" style={{ background: s.dot }} title={tr(...s.label())} />}
                  <span className="ml-auto shrink-0 text-xs text-ink-3">{project}</span>
                </Command.Item>
              );
            })}
          </Command.Group>
        )}
        {links.length > 0 && (
            <Command.Group heading={tr("Ouvrir", "Open")} className={heading}>
            {links.map((l) => (
              <Command.Item key={l.url + l.project} value={`${l.project} ${l.label}`} className={item} onSelect={() => go(() => window.open(l.url, "_blank", "noopener"))}>
                <ArrowUpRight className="size-4" />
                {l.label}
                <span className="ml-auto text-xs text-ink-3">{l.project}</span>
              </Command.Item>
            ))}
          </Command.Group>
        )}
        <Command.Group heading={tr("Actions", "Actions")} className={heading}>
          {agent && (
            <Command.Item value={tr("demander à zenith agent assistant question tâche", "ask zenith agent assistant question task")} className={item} onSelect={() => go(() => openAsk())}>
              <SquarePen className="size-4" /> {tr("Demander à zenith…", "Ask zenith…")}
              <kbd className="ml-auto font-sans text-2xs text-ink-3">⌘J</kbd>
            </Command.Item>
          )}
          <Command.Item className={item} onSelect={() => go(() => router.refresh())}>
            <RefreshCw className="size-4" /> {tr("Rafraîchir les données", "Refresh data")}
          </Command.Item>
          {SETTINGS().flatMap((g) =>
            g.items.map((x) => (
              <Command.Item key={x.href} value={`${tr("réglages", "settings")} ${g.label} ${x.name}`} className={item} onSelect={() => go(() => router.push(x.href))}>
                <Settings className="size-4" /> {tr("Réglages", "Settings")} · {g.label === "Code" ? `Code · ${x.name}` : x.name}
              </Command.Item>
            )),
          )}
          {code && ready && (
            <>
              <Command.Item value={tr("nouveau thread code agent", "new thread code agent")} className={item} onSelect={() => go(() => inCode(() => codeNavigate({ to: "palette", open: "new-thread-in" })))}>
                <MessageSquarePlus className="size-4" /> {tr("Nouveau thread…", "New thread…")}
              </Command.Item>
              <Command.Item value={tr("ajouter un projet code dossier", "add a project code folder")} className={item} onSelect={() => go(() => inCode(() => codeNavigate({ to: "palette", open: "add-project" })))}>
                <FolderPlus className="size-4" /> {tr("Ajouter un projet à zenith code", "Add a project to zenith code")}
              </Command.Item>
            </>
          )}
          {code && status?.enabled && status.built && (
            <Command.Item value={tr("redémarrer zenith code", "restart zenith code")} className={item} onSelect={() => go(() => window.dispatchEvent(new Event("zenith:code-restart")))}>
              <RotateCw className="size-4" /> {tr("Redémarrer zenith code", "Restart zenith code")}
            </Command.Item>
          )}
          {code && status?.running && (
            <Command.Item value={tr("ouvrir zenith code dans sa propre fenêtre", "open zenith code in its own window")} className={item} onSelect={() => go(() => window.open("/api/code/open", "_blank", "noopener"))}>
              <ExternalLink className="size-4" /> {tr("Ouvrir zenith code dans sa propre fenêtre", "Open zenith code in its own window")}
            </Command.Item>
          )}
        </Command.Group>
      </Command.List>
    </Command.Dialog>
  );
}
