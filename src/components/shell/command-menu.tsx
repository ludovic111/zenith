"use client";

import { Command, defaultFilter } from "cmdk";
import { useRouter } from "next/navigation";
import { useEffect, useState } from "react";
import { ArrowUpRight, Bot, LoaderCircle, Sparkles, Contact, ExternalLink, FolderPlus, LayoutGrid, MessageSquarePlus, Radar, RefreshCw, RotateCw, Settings2, SlidersHorizontal, SquareTerminal, Sun, Wallet } from "lucide-react";
import { tr } from "@/lib/i18n";
import { AssistantIcon } from "@/components/assistants/assistant-icon";
import { ASSISTANTS, assistantHref, type AssistantId } from "@/lib/assistants";
import { codeHref, codeNavigate, STATUS_STYLE, threadHref, threadKey, useCode } from "@/components/code/store";
import { guessTarget, type AgentTarget, type Provider } from "@/lib/agent/target";
import { askZenith, openAsk } from "@/components/agent/client";

type NavProject = { id: string; name: string; href: string; glow: string; tagline: string };
type ExtLink = { label: string; url: string; project: string; color: string };

type Agent = { targets: AgentTarget[]; provider: Provider } | null;

// The "Ask zenith" row always matches, but last: a sentence nothing else matches lands on it.
const filter = (value: string, search: string, keywords?: string[]) => (value.startsWith("ask:") ? 0.0001 : defaultFilter(value, search, keywords));

export function CommandMenu({ projects, links, code = true, assistants = [], agent = null }: { projects: NavProject[]; links: ExtLink[]; code?: boolean; assistants?: AssistantId[]; agent?: Agent }) {
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
    "[&_[cmdk-group-heading]]:px-3 [&_[cmdk-group-heading]]:py-2 [&_[cmdk-group-heading]]:text-[11px] [&_[cmdk-group-heading]]:uppercase [&_[cmdk-group-heading]]:tracking-widest [&_[cmdk-group-heading]]:text-ink-3";
  const item =
    "flex cursor-pointer items-center gap-3 rounded-xl px-3 py-2.5 text-sm text-ink-2 data-[selected=true]:bg-white/[0.07] data-[selected=true]:text-ink";

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
      overlayClassName="fixed inset-0 z-50 bg-black/60 backdrop-blur-sm"
      contentClassName="fixed left-1/2 top-[18vh] z-50 w-[min(640px,calc(100vw-2rem))] -translate-x-1/2 overflow-hidden rounded-3xl border border-white/10 bg-[#100e1c]/95 shadow-2xl shadow-black/60 backdrop-blur-xl"
    >
      <Command.Input
        autoFocus
        value={search}
        onValueChange={(v) => {
          setSearch(v);
          setAskError(null);
        }}
        placeholder={agent ? tr("Aller quelque part, ou demander quelque chose à zenith…", "Go somewhere, or ask zenith anything…") : tr("Projet, thread, lien, action…", "Project, thread, link, action…")}
        className="w-full border-b border-line bg-transparent px-5 py-4 text-base outline-none placeholder:text-ink-3"
      />
      <Command.List className="max-h-[50vh] overflow-y-auto p-2">
        <Command.Empty className="px-4 py-6 text-center text-sm text-ink-3">{tr("Rien sous ce ciel.", "Nothing under this sky.")}</Command.Empty>
        {agent && search.trim() && (
          <Command.Group heading="zenith" className={heading}>
            <Command.Item value={`ask:${search}`} className={item} onSelect={() => void ask()} disabled={asking}>
              {asking ? <LoaderCircle className="size-4 shrink-0 animate-spin text-sun" /> : <Sparkles className="size-4 shrink-0 text-sun" />}
              <span className="min-w-0 truncate">
                {tr("Demander à zenith : ", "Ask zenith: ")}
                <span className="text-ink">{search.trim()}</span>
              </span>
              {askTarget && (
                <span className="ml-auto inline-flex shrink-0 items-center gap-1.5 text-xs text-ink-3">
                  <span className="size-1.5 rounded-full" style={{ background: askTarget.glow }} /> {askTarget.name}
                </span>
              )}
            </Command.Item>
            {askError && <div className="px-3 pb-2 text-xs text-bad">{askError}</div>}
          </Command.Group>
        )}
        <Command.Group heading={tr("Projets", "Projects")} className={heading}>
          <Command.Item className={item} onSelect={() => go(() => router.push("/"))}>
            <LayoutGrid className="size-4" /> {tr("Vue d'ensemble", "Overview")}
          </Command.Item>
          <Command.Item value={tr("Ma vie agenda météo mails colis dépenses rythme", "My life calendar weather mail parcels spending rhythm")} className={item} onSelect={() => go(() => router.push("/vie"))}>
            <Sun className="size-4" /> {tr("Ma vie", "My life")}
            <span className="ml-auto text-xs text-ink-3">{tr("agenda · météo · à faire", "calendar · weather · to do")}</span>
          </Command.Item>
          {projects.map((p) => (
            <Command.Item key={p.id} value={`${p.name} ${p.tagline}`} className={item} onSelect={() => go(() => router.push(p.href))}>
              <span className="size-2.5 rounded-full" style={{ background: p.glow, boxShadow: `0 0 10px ${p.glow}` }} />
              {p.name}
              <span className="ml-auto text-xs text-ink-3">{p.tagline}</span>
            </Command.Item>
          ))}
          {code && (
            <Command.Item value={tr("zenith code coder agents terminal éditeur", "zenith code coding agents terminal editor")} className={item} onSelect={() => go(() => router.push(codeHref(snapshot?.pathname ?? "/")))}>
              <SquareTerminal className="size-4" /> zenith code
              <span className="ml-auto text-xs text-ink-3">{tr("coder avec les agents", "code with agents")}</span>
            </Command.Item>
          )}
          {assistants.map((id) => (
            <Command.Item key={id} value={`${ASSISTANTS[id].name} ${tr("assistant chat IA", "assistant AI chat")}`} className={item} onSelect={() => go(() => router.push(assistantHref(id)))}>
              <AssistantIcon id={id} size={16} /> {ASSISTANTS[id].name}
              <span className="ml-auto text-xs text-ink-3">{tr("app de bureau", "desktop app")}</span>
            </Command.Item>
          ))}
          <Command.Item value={tr("Veille mentions GitHub notifications actualité Hacker News marchés crypto Mac Homebrew", "Radar mentions GitHub notifications news Hacker News markets crypto Mac Homebrew")} className={item} onSelect={() => go(() => router.push("/veille"))}>
            <Radar className="size-4" /> {tr("Veille", "Radar")}
            <span className="ml-auto text-xs text-ink-3">{tr("mentions · actu · ce Mac", "mentions · news · this Mac")}</span>
          </Command.Item>
          <Command.Item value={tr("Agents IA Claude Codex sessions", "AI agents Claude Codex sessions")} className={item} onSelect={() => go(() => router.push("/agents"))}>
            <Bot className="size-4" /> {tr("Agents IA", "AI agents")}
            <span className="ml-auto text-xs text-ink-3">Claude Code · Codex</span>
          </Command.Item>
          <Command.Item value={tr("Annuaire réseaux sociaux e-mails domaines comptes", "Directory social networks emails domains accounts")} className={item} onSelect={() => go(() => router.push("/annuaire"))}>
            <Contact className="size-4" /> {tr("Annuaire", "Directory")}
            <span className="ml-auto text-xs text-ink-3">{tr("réseaux · e-mails · domaines", "socials · emails · domains")}</span>
          </Command.Item>
          <Command.Item value={tr("Abonnements frais dépenses factures ChatGPT Claude limites", "Subscriptions fees spending bills ChatGPT Claude limits")} className={item} onSelect={() => go(() => router.push("/abonnements"))}>
            <Wallet className="size-4" /> {tr("Abonnements", "Subscriptions")}
            <span className="ml-auto text-xs text-ink-3">{tr("frais · limites IA", "fees · AI limits")}</span>
          </Command.Item>
        </Command.Group>
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
                <ArrowUpRight className="size-4" style={{ color: l.color }} />
                {l.label}
                <span className="ml-auto text-xs text-ink-3">{l.project}</span>
              </Command.Item>
            ))}
          </Command.Group>
        )}
        <Command.Group heading={tr("Actions", "Actions")} className={heading}>
          {agent && (
            <Command.Item value={tr("demander à zenith agent assistant question tâche", "ask zenith agent assistant question task")} className={item} onSelect={() => go(() => openAsk())}>
              <Sparkles className="size-4 text-sun" /> {tr("Demander à zenith…", "Ask zenith…")}
              <kbd className="ml-auto rounded-md border border-line px-1.5 py-0.5 font-mono text-[10px] text-ink-3">⌘J</kbd>
            </Command.Item>
          )}
          <Command.Item className={item} onSelect={() => go(() => router.refresh())}>
            <RefreshCw className="size-4" /> {tr("Rafraîchir les données", "Refresh data")}
          </Command.Item>
          <Command.Item className={item} onSelect={() => go(() => router.push("/reglages"))}>
            <Settings2 className="size-4" /> {tr("Sources de données", "Data sources")}
          </Command.Item>
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
          {code && (
            <Command.Item value={tr("réglages de code providers modèles thème", "code settings providers models theme")} className={item} onSelect={() => go(() => router.push("/code/settings"))}>
              <SlidersHorizontal className="size-4" /> {tr("Réglages de zenith code", "zenith code settings")}
            </Command.Item>
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
