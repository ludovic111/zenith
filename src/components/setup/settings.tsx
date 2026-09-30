"use client";

import { useRouter } from "next/navigation";
import { useMemo, useState } from "react";
import { FolderSearch, LoaderCircle, Plus, Trash2 } from "lucide-react";
import { tr } from "@/lib/i18n";
import { avatarOf } from "@/lib/agent/avatar";
import { AgentAvatar } from "@/components/agent/agent-avatar";
import { freeId, saveConfig, scanFolder, slug, type FoundProject } from "./api";
import { AgentEditor, ProjectEditor, RoutineEditor, type ProjectDraft } from "./editors";
import { Button, Field, Segmented, TextInput } from "./fields";
import { BOT_TEMPLATES, type BotDraft, type RoutineDraft } from "./templates";
import { SaveBar, YouFields, youSet, type YouDraft } from "./you";

/** A draft of part of the config: what changed, and saving it (the page then reloads its data). */
function useDraft<T>(initial: T, toSet: (v: T) => Record<string, unknown>) {
  const router = useRouter();
  const [base, setBase] = useState(initial);
  const [value, setValue] = useState(initial);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const dirty = JSON.stringify(value) !== JSON.stringify(base);
  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await saveConfig(toSet(value));
      setBase(value);
      setSaved(true);
      window.setTimeout(() => setSaved(false), 2500);
      router.refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const update = (v: T) => {
    setValue(v);
    setError(null);
    setSaved(false);
  };
  return { value, update, bar: <SaveBar dirty={dirty} busy={busy} error={error} saved={saved} onSave={save} onReset={() => update(base)} /> };
}

export function YouSettings({ initial }: { initial: YouDraft }) {
  const { value, update, bar } = useDraft(initial, youSet);
  return (
    <>
      <div className="rounded-xl border border-line bg-surface p-4">
        <YouFields value={value} onChange={update} />
      </div>
      {bar}
    </>
  );
}

/** Drops empty optional fields, so the config stays as short as you'd write it. */
const clean = <T extends object>(o: T): T => Object.fromEntries(Object.entries(o).filter(([, v]) => v !== undefined && v !== "")) as T;

export type TeamDraft = {
  main: BotDraft;
  bots: BotDraft[];
  routines: RoutineDraft[];
  mcp: { name: string; kind: "command" | "url"; value: string }[];
};

function teamSet(t: TeamDraft): Record<string, unknown> {
  const mcp = Object.fromEntries(
    t.mcp
      .filter((m) => m.name.trim() && m.value.trim())
      .map((m) => {
        if (m.kind === "url") return [slug(m.name), { url: m.value.trim() }];
        const [command, ...args] = m.value.trim().split(/\s+/);
        return [slug(m.name), { command, args }];
      }),
  );
  return {
    "agent.name": t.main.name.trim() || "zenith",
    "agent.provider": t.main.provider,
    "agent.shape": t.main.shape ?? null,
    "agent.color": t.main.color ?? null,
    "agent.accessory": t.main.accessory ?? null,
    "agent.bots": t.bots.map(({ id, name, title, role, provider, shape, color, accessory, model, enabled }) => clean({ id, name: name.trim() || id, title: title?.trim(), role: role.trim() || name, provider, shape, color, accessory, model, enabled })),
    "agent.routines": t.routines.map((r) => clean({ ...r })),
    "agent.mcp": mcp,
  };
}

/** Your agent and its team, their routines, and the MCP servers they share. */
export function TeamSettings({ initial, skills, taken, available }: { initial: TeamDraft; skills: string[]; taken: string[]; available: { claude: boolean; codex: boolean } }) {
  const { value: t, update, bar } = useDraft(initial, teamSet);
  const [adding, setAdding] = useState(false);
  const ids = useMemo(() => new Set([...taken, "life", "zenith", "all", ...t.bots.map((b) => b.id)]), [taken, t.bots]);
  const agents = [{ id: "life", name: t.main.name || "zenith" }, ...t.bots.map((b) => ({ id: b.id, name: b.name || b.id }))];
  const addBot = (b: BotDraft) => {
    update({ ...t, bots: [...t.bots, { ...b, id: freeId(b.id || b.title || b.name, ids) }] });
    setAdding(false);
  };
  const setBot = (i: number, b: BotDraft) => update({ ...t, bots: t.bots.map((x, j) => (j === i ? b : x)) });
  const removeBot = (i: number) => {
    const gone = t.bots[i].id;
    update({ ...t, bots: t.bots.filter((_, j) => j !== i), routines: t.routines.filter((r) => r.bot !== gone) });
  };

  return (
    <>
      <section>
        <h2 className="mb-2 px-1 text-[13px] font-semibold text-ink">{tr("Ton agent", "Your agent")}</h2>
        <AgentEditor main value={t.main} onChange={(main) => update({ ...t, main })} available={available} />
      </section>

      <section className="mt-8">
        <div className="mb-2 flex items-end justify-between gap-4 px-1">
          <div>
            <h2 className="text-[13px] font-semibold text-ink">{tr("Son équipe", "Its team")}</h2>
            <p className="mt-0.5 text-xs text-ink-3">{tr("Des agents avec un prénom et un métier. Ils se parlent et ton agent leur confie ce qui est dans leur rôle.", "Agents with a first name and a job. They talk to each other and your agent hands them what fits their role.")}</p>
          </div>
          <Button onClick={() => setAdding((a) => !a)}>
            <Plus className="size-3.5" /> {tr("Ajouter", "Add")}
          </Button>
        </div>
        {adding && <TemplatePicker taken={new Set(t.bots.map((b) => b.id))} onPick={addBot} />}
        <div className="flex flex-col gap-2">
          {t.bots.map((b, i) => (
            <AgentEditor key={b.id} value={b} onChange={(v) => setBot(i, v)} onRemove={() => removeBot(i)} available={available} />
          ))}
          {!t.bots.length && !adding && <p className="rounded-xl border border-dashed border-line px-4 py-6 text-center text-xs text-ink-3">{tr("Pas encore d'équipe : ton agent fait tout seul.", "No team yet: your agent does everything itself.")}</p>}
        </div>
      </section>

      <section className="mt-8">
        <div className="mb-2 flex items-end justify-between gap-4 px-1">
          <div>
            <h2 className="text-[13px] font-semibold text-ink">{tr("Routines", "Routines")}</h2>
            <p className="mt-0.5 text-xs text-ink-3">{tr("Ce que l'équipe fait seule : à heure fixe, ou dès que quelque chose arrive.", "What the team does on its own: at a set time, or as soon as something happens.")}</p>
          </div>
          <Button onClick={() => update({ ...t, routines: [...t.routines, { id: freeId("routine", new Set(t.routines.map((r) => r.id))), name: tr("Nouvelle routine", "New routine"), at: "08:00", prompt: "" }] })}>
            <Plus className="size-3.5" /> {tr("Ajouter", "Add")}
          </Button>
        </div>
        <div className="flex flex-col gap-2">
          {t.routines.map((r, i) => (
            <RoutineEditor
              key={r.id}
              value={r}
              agents={agents}
              skills={skills}
              onChange={(v) => update({ ...t, routines: t.routines.map((x, j) => (j === i ? v : x)) })}
              onRemove={() => update({ ...t, routines: t.routines.filter((_, j) => j !== i) })}
            />
          ))}
          {!t.routines.length && <p className="rounded-xl border border-dashed border-line px-4 py-6 text-center text-xs text-ink-3">{tr("Aucune routine.", "No routine.")}</p>}
        </div>
      </section>

      <section className="mt-8">
        <div className="mb-2 flex items-end justify-between gap-4 px-1">
          <div>
            <h2 className="text-[13px] font-semibold text-ink">{tr("Outils partagés (MCP)", "Shared tools (MCP)")}</h2>
            <p className="mt-0.5 text-xs text-ink-3">{tr("Tout service qui a un serveur MCP devient utilisable par toute l'équipe, sur Claude comme sur Codex.", "Any service with an MCP server becomes usable by the whole team, on Claude and Codex alike.")}</p>
          </div>
          <Button onClick={() => update({ ...t, mcp: [...t.mcp, { name: "", kind: "url", value: "" }] })}>
            <Plus className="size-3.5" /> {tr("Ajouter", "Add")}
          </Button>
        </div>
        <div className="flex flex-col gap-2">
          {t.mcp.map((m, i) => {
            const set = (p: Partial<TeamDraft["mcp"][number]>) => update({ ...t, mcp: t.mcp.map((x, j) => (j === i ? { ...x, ...p } : x)) });
            return (
              <div key={i} className="grid items-end gap-2 rounded-xl border border-line bg-surface p-3 sm:grid-cols-[10rem_auto_1fr_auto]">
                <Field label={tr("Nom", "Name")}>
                  <TextInput value={m.name} onChange={(e) => set({ name: e.target.value })} placeholder="linear" />
                </Field>
                <Segmented value={m.kind} onChange={(kind) => set({ kind })} options={[{ id: "url", label: "URL" }, { id: "command", label: tr("Commande", "Command") }]} />
                <Field label={m.kind === "url" ? "URL" : tr("Commande", "Command")}>
                  <TextInput value={m.value} onChange={(e) => set({ value: e.target.value })} placeholder={m.kind === "url" ? "https://mcp.linear.app/mcp" : "npx @playwright/mcp@latest"} className="font-mono text-xs" />
                </Field>
                <button type="button" onClick={() => update({ ...t, mcp: t.mcp.filter((_, j) => j !== i) })} title={tr("Retirer", "Remove")} className="grid size-8 place-items-center rounded-md text-ink-3 transition hover:bg-hover hover:text-bad">
                  <Trash2 className="size-3.5" />
                </button>
              </div>
            );
          })}
          {!t.mcp.length && <p className="rounded-xl border border-dashed border-line px-4 py-6 text-center text-xs text-ink-3">{tr("Aucun pour l'instant : Linear, Notion, Playwright, ta base de données…", "None yet: Linear, Notion, Playwright, your database…")}</p>}
        </div>
      </section>
      {bar}
    </>
  );
}

/** Ready-made agents to start from, or a blank one. */
export function TemplatePicker({ taken, onPick }: { taken: Set<string>; onPick: (b: BotDraft) => void }) {
  return (
    <div className="mb-3 grid gap-2 rounded-xl border border-line bg-muted/40 p-2 sm:grid-cols-2">
      {BOT_TEMPLATES()
        .filter((b) => !taken.has(b.id))
        .map(({ pitch, ...b }) => (
          <button key={b.id} type="button" onClick={() => onPick(b)} className="flex items-start gap-2.5 rounded-lg p-2 text-left transition-colors hover:bg-surface">
            <TemplateFace b={b} />
            <span className="min-w-0">
              <span className="block text-[13px] text-ink">
                {b.name} <span className="text-ink-3">· {b.title}</span>
              </span>
              <span className="block text-xs text-ink-3">{pitch}</span>
            </span>
          </button>
        ))}
      <button type="button" onClick={() => onPick({ id: "agent", name: "", title: "", role: "", provider: "claude" })} className="flex items-center gap-2.5 rounded-lg p-2 text-left text-[13px] text-ink-2 transition-colors hover:bg-surface">
        <span className="grid size-8 place-items-center rounded-lg border border-dashed border-line">
          <Plus className="size-4 text-ink-3" />
        </span>
        {tr("Un agent à toi, de zéro", "Your own agent, from scratch")}
      </button>
    </div>
  );
}

function TemplateFace({ b }: { b: BotDraft }) {
  return <AgentAvatar avatar={avatarOf(b.id, b)} id={`tpl-${b.id}`} size={32} />;
}

/** Your projects: edit, remove, or add from your code folder. Fields the editor doesn't show are kept. */
export function ProjectsSettings({ initial, root }: { initial: ProjectDraft[]; root: string }) {
  const { value: list, update, bar } = useDraft(initial, (l) => ({ projects: l.map((p) => clean(p)) }));
  const [found, setFound] = useState<FoundProject[] | null>(null);
  const [folder, setFolder] = useState(root);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const ids = new Set(list.map((p) => p.id));
  const scan = async () => {
    setBusy(true);
    setError(null);
    try {
      setFound((await scanFolder(folder)).projects);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  const fresh = (found ?? []).filter((f) => !list.some((p) => p.dir === f.dir || (f.repo && p.repo === f.repo)));
  const add = (f: FoundProject) => update([...list, clean({ id: freeId(f.id, ids), name: f.name, tagline: f.tagline || undefined, dir: f.dir, repo: f.repo ?? undefined, site: f.site ?? undefined })]);

  return (
    <>
      <div className="flex flex-col gap-2">
        {list.map((p, i) => (
          <ProjectEditor key={p.id} value={p} onChange={(v) => update(list.map((x, j) => (j === i ? v : x)))} onRemove={() => update(list.filter((_, j) => j !== i))} />
        ))}
        {!list.length && <p className="rounded-xl border border-dashed border-line px-4 py-6 text-center text-xs text-ink-3">{tr("Aucun projet pour l'instant.", "No project yet.")}</p>}
      </div>

      <section className="mt-8">
        <h2 className="mb-2 px-1 text-[13px] font-semibold text-ink">{tr("Ajouter", "Add")}</h2>
        <div className="rounded-xl border border-line bg-surface p-3">
          <div className="flex flex-wrap items-end gap-2">
            <Field label={tr("Dossier de tes projets", "Your projects folder")} className="min-w-0 flex-1">
              <TextInput value={folder} onChange={(e) => setFolder(e.target.value)} className="font-mono text-xs" />
            </Field>
            <Button onClick={scan} disabled={busy}>
              {busy ? <LoaderCircle className="size-3.5 animate-spin" /> : <FolderSearch className="size-3.5" />}
              {tr("Chercher", "Look")}
            </Button>
            <Button onClick={() => update([...list, { id: freeId(tr("projet", "project"), ids), name: tr("Nouveau projet", "New project") }])}>
              <Plus className="size-3.5" /> {tr("À la main", "By hand")}
            </Button>
          </div>
          {error && <p className="mt-2 text-xs text-bad">{error}</p>}
          {found && (
            <ul className="mt-3 flex flex-col gap-1">
              {fresh.map((f) => (
                <li key={f.dir} className="flex items-center gap-3 rounded-lg px-2 py-1.5 hover:bg-hover">
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-[13px] text-ink">{f.name}</div>
                    <div className="truncate text-2xs text-ink-3">{[f.repo, f.tagline].filter(Boolean).join(" · ") || f.dir}</div>
                  </div>
                  <Button onClick={() => add(f)}>
                    <Plus className="size-3.5" /> {tr("Ajouter", "Add")}
                  </Button>
                </li>
              ))}
              {!fresh.length && <li className="px-2 py-1.5 text-xs text-ink-3">{tr("Rien de nouveau dans ce dossier.", "Nothing new in this folder.")}</li>}
            </ul>
          )}
        </div>
      </section>
      {bar}
    </>
  );
}
