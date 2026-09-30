"use client";

import { useRouter } from "next/navigation";
import { useMemo, useState } from "react";
import { ArrowLeft, ArrowRight, Check, CircleAlert, FolderSearch, LoaderCircle, Plus } from "lucide-react";
import { tr } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { avatarOf } from "@/lib/agent/avatar";
import { AgentAvatar } from "@/components/agent/agent-avatar";
import { ZenithMark } from "@/components/shell/logo";
import { currencyOf, freeId, saveConfig, scanFolder, type FoundProject } from "./api";
import { AgentEditor } from "./editors";
import { Button, Field, Switch, TextInput } from "./fields";
import { BOT_TEMPLATES, ROUTINE_TEMPLATES, type BotDraft, type RoutineDraft } from "./templates";
import { YouFields, youSet, type YouDraft } from "./you";

export type WelcomeProps = {
  you: YouDraft;
  root: string;
  found: FoundProject[];
  /** Projects already in the config (a second pass keeps them). */
  existing: { id: string; name: string; dir?: string; repo?: string }[];
  main: BotDraft;
  bots: BotDraft[];
  routines: RoutineDraft[];
  available: { claude: boolean; codex: boolean };
  codeReady: boolean;
};

const STEPS = () => [tr("Bienvenue", "Welcome"), tr("Toi", "You"), tr("Projets", "Projects"), tr("Équipe", "Team"), tr("Routines", "Routines"), tr("Prêt", "Ready")];

/**
 * The first run, in six short steps. Everything is guessed where it can be (your
 * language, your projects, which agents this Mac has), everything is saved as you go,
 * and all of it can be changed later in Settings.
 */
export function Welcome(props: WelcomeProps) {
  const router = useRouter();
  const [step, setStep] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [you, setYou] = useState(props.you);
  const [root, setRoot] = useState(props.root);
  const [found, setFound] = useState(props.found);
  const [picked, setPicked] = useState<Record<string, { on: boolean; name: string }>>(() => Object.fromEntries(props.found.map((f) => [f.dir, { on: true, name: f.name }])));
  const [main, setMain] = useState<BotDraft>({ ...props.main, provider: props.available.claude || !props.available.codex ? props.main.provider : "codex" });
  const [bots, setBots] = useState<BotDraft[]>(props.bots);
  const [routines, setRoutines] = useState<Record<string, boolean>>({});

  const templates = useMemo(() => ROUTINE_TEMPLATES(new Set(bots.map((b) => b.id))), [bots]);
  // On unless you turned it off; the mail capture only when Claude (and its connectors) is here.
  const isOn = (t: RoutineDraft) => routines[t.id] ?? (props.routines.some((r) => r.id === t.id) || t.task !== "refresh-life" || props.available.claude);

  const run = async (fn: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await fn();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const next = () =>
    run(async () => {
      if (step === 0) await saveConfig({ locale: you.locale });
      if (step === 1) {
        if (!you.name.trim()) throw new Error(tr("Dis-moi comment tu t'appelles.", "Tell me your name."));
        await saveConfig(youSet(you));
      }
      if (step === 2) {
        const taken = new Set(props.existing.map((p) => p.id));
        const added = found
          .filter((f) => picked[f.dir]?.on && !props.existing.some((p) => p.dir === f.dir || (f.repo && p.repo === f.repo)))
          .map((f) => {
            const id = freeId(f.id, taken);
            taken.add(id);
            return Object.fromEntries(Object.entries({ id, name: picked[f.dir].name.trim() || f.name, tagline: f.tagline, dir: f.dir, repo: f.repo, site: f.site }).filter(([, v]) => v));
          });
        const raw = await fetchRawProjects();
        await saveConfig({ projectsRoot: root, projects: [...raw, ...added] });
      }
      if (step === 3)
        await saveConfig({
          "agent.name": main.name.trim() || "zenith",
          "agent.provider": main.provider,
          "agent.shape": main.shape ?? null,
          "agent.color": main.color ?? null,
          "agent.accessory": main.accessory ?? null,
          "agent.bots": bots.map((b) => Object.fromEntries(Object.entries({ ...b, name: b.name.trim() || b.id, role: b.role.trim() || b.name }).filter(([, v]) => v !== undefined && v !== ""))),
        });
      if (step === 4) {
        const keep = props.routines.filter((r) => !templates.some((t) => t.id === r.id));
        const chosen = templates.filter(isOn).map((r) => Object.fromEntries(Object.entries(r).filter(([k, v]) => k !== "pitch" && v !== undefined)));
        await saveConfig({ "agent.routines": [...keep, ...chosen] });
      }
      if (step === 5) {
        router.push("/");
        router.refresh();
        return;
      }
      setStep((s) => s + 1);
      router.refresh();
    });

  const rescan = () =>
    run(async () => {
      const r = await scanFolder(root);
      setFound(r.projects);
      setPicked(Object.fromEntries(r.projects.map((f) => [f.dir, { on: true, name: f.name }])));
    });

  return (
    <div className="fixed inset-0 z-[60] flex flex-col overflow-y-auto bg-background">
      <div data-drag className="h-[var(--titlebar-h)] shrink-0" />
      <div className="mx-auto flex w-full max-w-2xl flex-1 flex-col px-5 pb-10">
        <ol className="mb-8 flex items-center gap-1.5" aria-label={tr("Étapes", "Steps")}>
          {STEPS().map((s, i) => (
            <li key={s} className="flex flex-1 flex-col gap-1.5">
              <span className={cn("h-1 rounded-full transition-colors", i <= step ? "bg-primary" : "bg-line")} />
              <span className={cn("hidden text-2xs sm:block", i === step ? "text-ink" : "text-ink-3")}>{s}</span>
            </li>
          ))}
        </ol>

        <div className="flex-1">
          {step === 0 && (
            <Intro
              locale={you.locale}
              onLocale={(locale) => {
                const currency = you.location ? you.currency : (currencyOf(locale.split("-")[1]) ?? you.currency);
                setYou({ ...you, locale, currency });
                run(async () => {
                  await saveConfig({ locale, currency });
                  router.refresh();
                });
              }}
            />
          )}

          {step === 1 && (
            <StepShell title={tr("Faisons connaissance", "Let's get acquainted")} lead={tr("zenith reste sur ce Mac : ces infos ne vont nulle part ailleurs.", "zenith stays on this Mac: none of this goes anywhere else.")}>
              <YouFields value={you} onChange={setYou} />
            </StepShell>
          )}

          {step === 2 && (
            <StepShell title={tr("Tes projets", "Your projects")} lead={tr("zenith suit leurs commits, leur CI, leurs sites, et tes agents y travaillent. Voici ce que j'ai trouvé.", "zenith follows their commits, CI and sites, and your agents work in them. Here is what I found.")}>
              <div className="flex items-end gap-2">
                <Field label={tr("Dossier de tes projets", "Your projects folder")} className="min-w-0 flex-1">
                  <TextInput value={root} onChange={(e) => setRoot(e.target.value)} className="font-mono text-xs" />
                </Field>
                <Button onClick={rescan} disabled={busy}>
                  <FolderSearch className="size-3.5" /> {tr("Chercher", "Look")}
                </Button>
              </div>
              <ul className="mt-4 flex flex-col gap-1.5">
                {props.existing.map((p) => (
                  <li key={p.id} className="flex items-center gap-3 rounded-lg border border-line bg-surface px-3 py-2 text-[13px] text-ink-2">
                    <Check className="size-4 text-good" /> {p.name} <span className="text-xs text-ink-3">{tr("déjà suivi", "already followed")}</span>
                  </li>
                ))}
                {found
                  .filter((f) => !props.existing.some((p) => p.dir === f.dir || (f.repo && p.repo === f.repo)))
                  .map((f) => {
                    const p = picked[f.dir] ?? { on: false, name: f.name };
                    return (
                      <li key={f.dir} className={cn("flex items-center gap-3 rounded-lg border border-line bg-surface px-3 py-2", !p.on && "opacity-60")}>
                        <Switch on={p.on} onChange={(on) => setPicked({ ...picked, [f.dir]: { ...p, on } })} label={f.name} />
                        <div className="min-w-0 flex-1">
                          <input value={p.name} onChange={(e) => setPicked({ ...picked, [f.dir]: { ...p, name: e.target.value } })} className="w-full bg-transparent text-[13px] text-ink outline-none" />
                          <div className="truncate text-2xs text-ink-3">{[f.repo, f.tagline].filter(Boolean).join(" · ") || f.dir}</div>
                        </div>
                      </li>
                    );
                  })}
                {!found.length && !props.existing.length && (
                  <li className="rounded-lg border border-dashed border-line px-4 py-6 text-center text-xs text-ink-3">
                    {tr("Aucun dépôt git dans ce dossier. Indique celui où tu codes, ou passe : tu pourras en ajouter dans Réglages → Projets.", "No git repository in this folder. Point to where you code, or skip: you can add some in Settings → Projects.")}
                  </li>
                )}
              </ul>
            </StepShell>
          )}

          {step === 3 && (
            <StepShell
              title={tr("Ton équipe", "Your team")}
              lead={tr(
                "Ton agent s'occupe de tout ce que tu lui demandes. Donne-lui un nom, et ajoute s'il le faut des coéquipiers : chacun a un métier, une tête et tourne sur ton abonnement Claude ou ChatGPT.",
                "Your agent takes care of whatever you ask. Give it a name, and add teammates if you like: each has a job, a face, and runs on your Claude or ChatGPT subscription.",
              )}
            >
              {!props.available.claude && !props.available.codex && (
                <p className="mb-3 flex items-start gap-2 rounded-lg border border-line bg-surface px-3 py-2 text-xs text-ink-2">
                  <CircleAlert className="mt-px size-3.5 shrink-0 text-bad" />
                  {tr("Ni Claude Code ni Codex sur ce Mac : installe l'un des deux (claude.ai/code, ou l'app ChatGPT) pour que l'équipe puisse travailler.", "Neither Claude Code nor Codex on this Mac: install one (claude.ai/code, or the ChatGPT app) so the team can work.")}
                </p>
              )}
              <AgentEditor main value={main} onChange={setMain} available={props.available} defaultOpen />
              <div className="mt-6 grid gap-2 sm:grid-cols-2">
                {BOT_TEMPLATES().map(({ pitch, ...t }) => {
                  const on = bots.some((b) => b.id === t.id);
                  return (
                    <button
                      key={t.id}
                      type="button"
                      onClick={() => setBots(on ? bots.filter((b) => b.id !== t.id) : [...bots, { ...t, provider: props.available[t.provider] || !props.available[t.provider === "claude" ? "codex" : "claude"] ? t.provider : t.provider === "claude" ? "codex" : "claude" }])}
                      className={cn("flex items-start gap-3 rounded-xl border p-3 text-left transition-colors", on ? "border-primary/60 bg-primary/5" : "border-line bg-surface hover:bg-hover")}
                    >
                      <AgentAvatar avatar={avatarOf(t.id, t)} id={`wtpl-${t.id}`} size={36} blink={on} />
                      <span className="min-w-0 flex-1">
                        <span className="block text-[13px] text-ink">
                          {t.name} <span className="text-ink-3">· {t.title}</span>
                        </span>
                        <span className="block text-xs text-ink-3">{pitch}</span>
                      </span>
                      <span className={cn("grid size-5 shrink-0 place-items-center rounded-full border", on ? "border-primary bg-primary text-primary-foreground" : "border-line")}>{on && <Check className="size-3" />}</span>
                    </button>
                  );
                })}
              </div>
              {bots.length > 0 && (
                <div className="mt-4 flex flex-col gap-2">
                  <p className="px-1 text-xs text-ink-3">{tr("Change leur prénom, leur abonnement ou leur tête :", "Change their name, subscription or face:")}</p>
                  {bots.map((b, i) => (
                    <AgentEditor key={b.id} value={b} available={props.available} onChange={(v) => setBots(bots.map((x, j) => (j === i ? v : x)))} onRemove={() => setBots(bots.filter((_, j) => j !== i))} />
                  ))}
                </div>
              )}
              <Button
                className="mt-3"
                onClick={() => setBots([...bots, { id: freeId("agent", new Set(bots.map((b) => b.id))), name: "", title: "", role: "", provider: main.provider }])}
              >
                <Plus className="size-3.5" /> {tr("Un agent à toi", "Your own agent")}
              </Button>
            </StepShell>
          )}

          {step === 4 && (
            <StepShell title={tr("Ce qu'ils font seuls", "What they do on their own")} lead={tr("Des routines : à heure fixe, ou dès que quelque chose arrive. Tu en ajouteras d'autres dans Réglages → Équipe.", "Routines: at a set time, or as soon as something happens. Add more in Settings → Team.")}>
              <ul className="flex flex-col gap-1.5">
                {templates.map((t) => {
                  const on = isOn(t);
                  return (
                    <li key={t.id} className="flex items-center gap-3 rounded-lg border border-line bg-surface px-3 py-2.5">
                      <Switch on={on} onChange={(v) => setRoutines({ ...routines, [t.id]: v })} label={t.name} />
                      <div className="min-w-0 flex-1">
                        <div className="text-[13px] text-ink">
                          {t.name}
                          <span className="ml-2 text-xs text-ink-3">{t.at ?? tr("sur événement", "on event")}</span>
                        </div>
                        <div className="text-xs text-ink-3">{t.pitch}</div>
                      </div>
                    </li>
                  );
                })}
              </ul>
            </StepShell>
          )}

          {step === 5 && (
            <StepShell title={tr(`C'est prêt${you.name ? `, ${you.name.split(/\s+/)[0]}` : ""}.`, `All set${you.name ? `, ${you.name.split(/\s+/)[0]}` : ""}.`)} lead={tr("Trois espaces, un sélecteur en haut de la barre latérale :", "Three spaces, one switcher at the top of the sidebar:")}>
              <ul className="flex flex-col gap-2 text-[13px] text-ink-2">
                <li>
                  <b className="font-medium text-ink">{tr("Aperçu", "Overview")}</b> · {tr("ta journée, tes projets, ton argent, ce qui t'attend.", "your day, projects, money, what is waiting.")}
                </li>
                <li>
                  <b className="font-medium text-ink">{tr("Équipe", "Team")}</b> · {tr("parle à", "talk to")} {[main.name || "zenith", ...bots.map((b) => b.name)].filter(Boolean).join(", ")} (⌘J).
                </li>
                <li>
                  <b className="font-medium text-ink">Code</b> · {tr("tes fils de code avec Claude Code et Codex.", "your coding threads with Claude Code and Codex.")}
                </li>
              </ul>
              <div className="mt-6 flex items-center gap-2">
                {[{ ...main, id: "life" }, ...bots].map((b) => (
                  <AgentAvatar key={b.id} avatar={avatarOf(b.id, b)} id={`done-${b.id}`} size={36} blink />
                ))}
              </div>
              {!props.codeReady && (
                <p className="mt-6 flex items-start gap-2 rounded-lg border border-line bg-surface px-3 py-2 text-xs text-ink-2">
                  <CircleAlert className="mt-px size-3.5 shrink-0 text-ink-3" />
                  {tr("Tes agents travaillent dans zenith code, qui n'est pas encore lancé : installe l'app avec « npm run mac:install ».", "Your agents work in zenith code, which isn't running yet: install the app with \"npm run mac:install\".")}
                </p>
              )}
              <p className="mt-4 text-xs text-ink-3">{tr("Pour lire tes mails et ton agenda, branche Gmail et Google Agenda dans Claude (claude.ai → Réglages → Connecteurs).", "To read your mail and calendar, connect Gmail and Google Calendar in Claude (claude.ai → Settings → Connectors).")}</p>
            </StepShell>
          )}
        </div>

        {error && <p className="mt-4 text-xs text-bad">{error}</p>}
        <div className="mt-8 flex items-center gap-2">
          {step > 0 && (
            <Button onClick={() => setStep((s) => s - 1)} disabled={busy}>
              <ArrowLeft className="size-3.5" /> {tr("Retour", "Back")}
            </Button>
          )}
          {step > 1 && step < 5 && (
            <button type="button" onClick={() => setStep((s) => s + 1)} disabled={busy} className="ml-auto text-xs text-ink-3 hover:text-ink">
              {tr("Passer", "Skip")}
            </button>
          )}
          <Button variant="primary" onClick={next} disabled={busy} className={cn(step <= 1 || step === 5 ? "ml-auto" : "")}>
            {busy && <LoaderCircle className="size-3.5 animate-spin" />}
            {step === 0 ? tr("Commencer", "Start") : step === 5 ? tr("Ouvrir zenith", "Open zenith") : tr("Continuer", "Continue")}
            {step < 5 && <ArrowRight className="size-3.5" />}
          </Button>
        </div>
      </div>
    </div>
  );
}

/** The projects as they are in the file now (kept whole when new ones are added). */
async function fetchRawProjects(): Promise<Record<string, unknown>[]> {
  const res = await fetch("/api/config");
  if (!res.ok) return [];
  return ((await res.json()) as { projects?: Record<string, unknown>[] }).projects ?? [];
}

function StepShell({ title, lead, children }: { title: string; lead?: string; children: React.ReactNode }) {
  return (
    <section>
      <h1 className="text-2xl font-semibold tracking-tight text-ink">{title}</h1>
      {lead && <p className="mt-2 max-w-xl text-sm text-ink-3">{lead}</p>}
      <div className="mt-6">{children}</div>
    </section>
  );
}

function Intro({ locale, onLocale }: { locale: string; onLocale: (l: string) => void }) {
  const fr = locale.startsWith("fr");
  return (
    <section className="flex flex-col items-start pt-6">
      <ZenithMark size={40} />
      <h1 className="mt-6 text-3xl font-semibold tracking-tight text-ink">{tr("Bienvenue dans zenith", "Welcome to zenith")}</h1>
      <p className="mt-3 max-w-lg text-[15px] leading-relaxed text-ink-2">
        {tr(
          "Un ciel privé au-dessus de tes projets et de ta journée — et une équipe d'agents IA qui agit pour toi, sur tes abonnements Claude et ChatGPT. Tout reste sur ce Mac.",
          "A private sky over your projects and your day — and a team of AI agents that acts for you, on your Claude and ChatGPT subscriptions. Everything stays on this Mac.",
        )}
      </p>
      <div className="mt-8 flex gap-2">
        {[
          { id: fr ? locale : "fr-FR", label: "Français" },
          { id: fr ? "en-US" : locale, label: "English" },
        ].map((l) => (
          <button
            key={l.label}
            type="button"
            onClick={() => onLocale(l.id)}
            className={cn("h-10 rounded-lg border px-4 text-[13px] transition-colors", locale.startsWith(l.id.slice(0, 2)) ? "border-primary bg-primary/5 text-ink" : "border-line bg-surface text-ink-2 hover:bg-hover")}
          >
            {l.label}
          </button>
        ))}
      </div>
    </section>
  );
}
