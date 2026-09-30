"use client";

import { useState } from "react";
import { ChevronDown, Trash2 } from "lucide-react";
import { tr } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { avatarOf } from "@/lib/agent/avatar";
import { AgentAvatar } from "@/components/agent/agent-avatar";
import { ProviderIcon } from "@/components/agent/provider-icon";
import { AvatarPicker } from "./avatar-picker";
import { Field, Segmented, Select, Switch, TextArea, TextInput } from "./fields";
import type { BotDraft, RoutineDraft } from "./templates";

/** Claude or Codex, with each one's mark. */
export function ProviderChoice({ value, onChange, available }: { value: "claude" | "codex"; onChange: (v: "claude" | "codex") => void; available?: { claude: boolean; codex: boolean } }) {
  return (
    <Segmented
      value={value}
      onChange={onChange}
      options={(["claude", "codex"] as const).map((p) => ({
        id: p,
        label: (
          <>
            <ProviderIcon id={p} size={12} />
            {p === "claude" ? "Claude" : "Codex"}
            {available && !available[p] && <span className="text-2xs text-ink-3">{tr("(absent)", "(missing)")}</span>}
          </>
        ),
      }))}
    />
  );
}

/**
 * One agent: face, first name, job, subscription and role. `main` is your own agent
 * (its folder is your life; no job, no role: it does everything else).
 */
export function AgentEditor({
  value,
  onChange,
  onRemove,
  main = false,
  available,
  defaultOpen = false,
}: {
  value: BotDraft;
  onChange: (v: BotDraft) => void;
  onRemove?: () => void;
  main?: boolean;
  available?: { claude: boolean; codex: boolean };
  defaultOpen?: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  const [face, setFace] = useState(false);
  const avatar = avatarOf(value.id || "new", value);
  return (
    <div className="rounded-xl border border-line bg-surface">
      <div className="flex items-center gap-3 p-3">
        <button type="button" onClick={() => (setOpen(true), setFace((f) => !f))} title={tr("Changer sa tête", "Change its face")} className="grid size-11 shrink-0 place-items-center rounded-xl transition-transform hover:-translate-y-0.5" style={{ background: `color-mix(in oklab, ${avatar.color} 12%, transparent)` }}>
          <AgentAvatar avatar={avatar} id={`edit-${value.id}`} size={34} blink />
        </button>
        <button type="button" onClick={() => setOpen((o) => !o)} className="min-w-0 flex-1 text-left">
          <div className="flex items-center gap-1.5 text-[13px] font-medium text-ink">
            <span className="truncate">{value.name || tr("Sans nom", "Unnamed")}</span>
            {value.title && <span className="truncate font-normal text-ink-3">· {value.title}</span>}
            {main && <span className="shrink-0 rounded border border-line px-1 text-2xs font-normal text-ink-3">{tr("principal", "main")}</span>}
          </div>
          <div className="flex items-center gap-1 text-2xs text-ink-3">
            <ProviderIcon id={value.provider} size={10} />
            {value.provider === "claude" ? "Claude" : "Codex"}
            {!main && value.role && <span className="truncate">· {value.role}</span>}
          </div>
        </button>
        {onRemove && (
          <button type="button" onClick={onRemove} title={tr("Retirer", "Remove")} className="grid size-7 place-items-center rounded-md text-ink-3 transition hover:bg-hover hover:text-bad">
            <Trash2 className="size-3.5" />
          </button>
        )}
        <button type="button" onClick={() => setOpen((o) => !o)} title={open ? tr("Replier", "Collapse") : tr("Modifier", "Edit")} className="grid size-7 place-items-center rounded-md text-ink-3 transition hover:bg-hover hover:text-ink">
          <ChevronDown className={cn("size-4 transition-transform", open && "rotate-180")} />
        </button>
      </div>
      {open && (
        <div className="flex flex-col gap-3 border-t border-line p-3">
          {face && <AvatarPicker id={value.id || "new"} value={value} onChange={(a) => onChange({ ...value, ...a })} />}
          <div className="grid gap-3 sm:grid-cols-2">
            <Field label={tr("Prénom", "First name")}>
              <TextInput value={value.name} onChange={(e) => onChange({ ...value, name: e.target.value })} placeholder={tr("Margot", "Maya")} />
            </Field>
            {!main && (
              <Field label={tr("Métier", "Job")}>
                <TextInput value={value.title ?? ""} onChange={(e) => onChange({ ...value, title: e.target.value || undefined })} placeholder={tr("Courrier", "Mail")} />
              </Field>
            )}
          </div>
          <Field label={tr("Abonnement", "Subscription")} hint={tr("Claude a tes connecteurs (Gmail, Agenda…) ; Codex a le navigateur et le contrôle du Mac.", "Claude has your connectors (Gmail, Calendar…); Codex has the browser and computer use.")}>
            <ProviderChoice value={value.provider} onChange={(provider) => onChange({ ...value, provider })} available={available} />
          </Field>
          {!main && (
            <Field label={tr("Son rôle", "Its role")} hint={tr("Ce qu'il fait et ce qu'il ne doit jamais faire. Il s'en sert comme personnalité de départ.", "What it does and what it must never do. It becomes its starting personality.")}>
              <TextArea value={value.role} onChange={(e) => onChange({ ...value, role: e.target.value })} rows={3} />
            </Field>
          )}
          {!face && (
            <button type="button" onClick={() => setFace(true)} className="self-start text-xs text-ink-3 underline-offset-4 hover:text-ink hover:underline">
              {tr("Changer sa tête", "Change its face")}
            </button>
          )}
        </div>
      )}
    </div>
  );
}

export type ProjectDraft = { id: string; name: string; tagline?: string; dir?: string; repo?: string; site?: string; [k: string]: unknown };

/** One project: name, tagline, folder, repository and site. Other fields it had are kept. */
export function ProjectEditor({ value, onChange, onRemove, defaultOpen = false }: { value: ProjectDraft; onChange: (v: ProjectDraft) => void; onRemove: () => void; defaultOpen?: boolean }) {
  const [open, setOpen] = useState(defaultOpen);
  const set = (k: keyof ProjectDraft, v: string) => onChange({ ...value, [k]: v.trim() ? v : undefined });
  return (
    <div className="rounded-xl border border-line bg-surface">
      <div className="flex items-center gap-3 px-3 py-2.5">
        <button type="button" onClick={() => setOpen((o) => !o)} className="min-w-0 flex-1 text-left">
          <div className="truncate text-[13px] font-medium text-ink">{value.name || tr("Sans nom", "Unnamed")}</div>
          <div className="truncate text-2xs text-ink-3">{[value.tagline, value.repo, value.dir].filter(Boolean).join(" · ") || tr("Rien de plus pour l'instant", "Nothing more yet")}</div>
        </button>
        <button type="button" onClick={onRemove} title={tr("Retirer", "Remove")} className="grid size-7 place-items-center rounded-md text-ink-3 transition hover:bg-hover hover:text-bad">
          <Trash2 className="size-3.5" />
        </button>
        <button type="button" onClick={() => setOpen((o) => !o)} className="grid size-7 place-items-center rounded-md text-ink-3 transition hover:bg-hover hover:text-ink">
          <ChevronDown className={cn("size-4 transition-transform", open && "rotate-180")} />
        </button>
      </div>
      {open && (
        <div className="grid gap-3 border-t border-line p-3 sm:grid-cols-2">
          <Field label={tr("Nom", "Name")}>
            <TextInput value={value.name} onChange={(e) => onChange({ ...value, name: e.target.value })} />
          </Field>
          <Field label={tr("En une phrase", "In one line")}>
            <TextInput value={value.tagline ?? ""} onChange={(e) => set("tagline", e.target.value)} placeholder={tr("Une app de…", "An app that…")} />
          </Field>
          <Field label={tr("Dossier", "Folder")} hint={tr("Un nom dans ton dossier de projets, ou un chemin", "A name in your projects folder, or a path")}>
            <TextInput value={value.dir ?? ""} onChange={(e) => set("dir", e.target.value)} placeholder="my-app" className="font-mono text-xs" />
          </Field>
          <Field label={tr("Dépôt GitHub", "GitHub repository")}>
            <TextInput value={value.repo ?? ""} onChange={(e) => set("repo", e.target.value)} placeholder="owner/repo" className="font-mono text-xs" />
          </Field>
          <Field label={tr("Site", "Site")} className="sm:col-span-2">
            <TextInput value={value.site ?? ""} onChange={(e) => set("site", e.target.value)} placeholder="https://" className="font-mono text-xs" />
          </Field>
        </div>
      )}
    </div>
  );
}

const DAYS = () => [tr("L", "M"), tr("M", "T"), tr("M", "W"), tr("J", "T"), tr("V", "F"), tr("S", "S"), tr("D", "S")];

export const NOW_KIND_NAMES = (): Record<string, string> => ({
  reply: tr("E-mail à répondre", "Email to answer"),
  sale: tr("Acheteur en attente", "Buyer waiting"),
  ci: tr("CI cassée", "Broken CI"),
  down: tr("Site en panne", "Site down"),
  payment: tr("Paiement en échec", "Failing payment"),
  civic: tr("Administratif", "Paperwork"),
  birthday: tr("Anniversaire", "Birthday"),
  refresh: tr("Relevé en retard", "Stale capture"),
});

/** One routine: when (a time and days, or an event), who, and what. */
export function RoutineEditor({
  value,
  onChange,
  onRemove,
  agents,
  skills,
}: {
  value: RoutineDraft;
  onChange: (v: RoutineDraft) => void;
  onRemove: () => void;
  agents: { id: string; name: string }[];
  skills: string[];
}) {
  const [open, setOpen] = useState(false);
  const event = !!value.on;
  const enabled = value.enabled !== false;
  const who = agents.find((a) => a.id === (value.bot ?? "life"))?.name ?? value.bot ?? "";
  const what = value.task === "refresh-life" ? tr("relevé mails et agenda", "mail and calendar capture") : value.skill ? `skill ${value.skill}` : value.prompt ? value.prompt.slice(0, 40) : event ? tr("la demande de l'élément", "the item's request") : "";
  const when = event
    ? tr(`à chaque ${(value.on ?? []).map((k) => NOW_KIND_NAMES()[k]?.toLowerCase() ?? k).join(", ")}`, `on each ${(value.on ?? []).map((k) => NOW_KIND_NAMES()[k]?.toLowerCase() ?? k).join(", ")}`)
    : `${(value.days?.length ?? 7) === 7 ? tr("tous les jours", "every day") : (value.days ?? []).map((d) => DAYS()[d - 1]).join(" ")} · ${value.at ?? "—"}`;
  return (
    <div className={cn("rounded-xl border border-line bg-surface", !enabled && "opacity-70")}>
      <div className="flex items-center gap-3 px-3 py-2.5">
        <Switch on={enabled} onChange={(on) => onChange({ ...value, enabled: on ? undefined : false })} label={tr("Activée", "On")} />
        <button type="button" onClick={() => setOpen((o) => !o)} className="min-w-0 flex-1 text-left">
          <div className="truncate text-[13px] text-ink">{value.name || value.id}</div>
          <div className="truncate text-2xs text-ink-3">
            {when} · {who}
            {what && ` · ${what}`}
          </div>
        </button>
        <button type="button" onClick={onRemove} title={tr("Supprimer", "Delete")} className="grid size-7 place-items-center rounded-md text-ink-3 transition hover:bg-hover hover:text-bad">
          <Trash2 className="size-3.5" />
        </button>
        <button type="button" onClick={() => setOpen((o) => !o)} className="grid size-7 place-items-center rounded-md text-ink-3 transition hover:bg-hover hover:text-ink">
          <ChevronDown className={cn("size-4 transition-transform", open && "rotate-180")} />
        </button>
      </div>
      {open && (
        <div className="flex flex-col gap-3 border-t border-line p-3">
          <div className="grid gap-3 sm:grid-cols-2">
            <Field label={tr("Nom", "Name")}>
              <TextInput value={value.name ?? ""} onChange={(e) => onChange({ ...value, name: e.target.value || undefined })} />
            </Field>
            <Field label={tr("Qui", "Who")}>
              <Select value={value.bot ?? "life"} onChange={(e) => onChange({ ...value, bot: e.target.value === "life" ? undefined : e.target.value })}>
                {agents.map((a) => (
                  <option key={a.id} value={a.id}>
                    {a.name}
                  </option>
                ))}
              </Select>
            </Field>
          </div>
          <Field label={tr("Quand", "When")}>
            <Segmented
              value={event ? "event" : "time"}
              onChange={(k) => onChange(k === "event" ? { ...value, at: undefined, days: undefined, on: value.on ?? ["reply"] } : { ...value, on: undefined, at: value.at ?? "08:00" })}
              options={[
                { id: "time", label: tr("À heure fixe", "At a set time") },
                { id: "event", label: tr("Quand quelque chose arrive", "When something happens") },
              ]}
            />
          </Field>
          {event ? (
            <div className="flex flex-wrap gap-1.5">
              {Object.entries(NOW_KIND_NAMES()).map(([k, label]) => {
                const on = value.on?.includes(k);
                return (
                  <button
                    key={k}
                    type="button"
                    onClick={() => {
                      const next = on ? (value.on ?? []).filter((x) => x !== k) : [...(value.on ?? []), k];
                      if (next.length) onChange({ ...value, on: next });
                    }}
                    className={cn("h-7 rounded-md border px-2 text-xs transition-colors", on ? "border-ink-3/60 bg-selected text-ink" : "border-line text-ink-3 hover:text-ink-2")}
                  >
                    {label}
                  </button>
                );
              })}
            </div>
          ) : (
            <div className="flex flex-wrap items-center gap-3">
              <TextInput type="time" value={value.at ?? "08:00"} onChange={(e) => onChange({ ...value, at: e.target.value })} className="w-28" />
              <div className="flex gap-1">
                {DAYS().map((d, i) => {
                  const day = i + 1;
                  const days = value.days ?? [1, 2, 3, 4, 5, 6, 7];
                  const on = days.includes(day);
                  return (
                    <button
                      key={i}
                      type="button"
                      onClick={() => {
                        const next = on ? days.filter((x) => x !== day) : [...days, day].sort();
                        if (next.length) onChange({ ...value, days: next.length === 7 ? undefined : next });
                      }}
                      className={cn("grid size-7 place-items-center rounded-md border text-xs transition-colors", on ? "border-ink-3/60 bg-selected text-ink" : "border-line text-ink-3")}
                    >
                      {d}
                    </button>
                  );
                })}
              </div>
            </div>
          )}
          <div className="grid gap-3 sm:grid-cols-2">
            <Field label={tr("Suivre un skill", "Follow a skill")}>
              <Select value={value.task ? "__task" : value.skill ?? ""} onChange={(e) => onChange({ ...value, task: e.target.value === "__task" ? "refresh-life" : undefined, skill: e.target.value && e.target.value !== "__task" ? e.target.value : undefined })}>
                <option value="">{tr("Aucun", "None")}</option>
                <option value="__task">{tr("Relevé mails et agenda (intégré)", "Mail and calendar capture (built in)")}</option>
                {skills.map((s) => (
                  <option key={s} value={s}>
                    {s}
                  </option>
                ))}
              </Select>
            </Field>
            <Field label={tr("Consigne", "Request")} hint={tr("Tes mots, en plus du skill", "Your words, on top of the skill")}>
              <TextInput value={value.prompt ?? ""} onChange={(e) => onChange({ ...value, prompt: e.target.value || undefined })} placeholder={tr("Fais le point sur…", "Review…")} />
            </Field>
          </div>
        </div>
      )}
    </div>
  );
}
