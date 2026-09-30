import { tr } from "@/lib/i18n";
import type { AvatarAccessory, AvatarShape } from "@/lib/agent/avatar";

/** Agents and routines offered to start from: everything stays editable. */

export type BotDraft = {
  id: string;
  name: string;
  title?: string;
  role: string;
  provider: "claude" | "codex";
  shape?: AvatarShape;
  color?: string;
  accessory?: AvatarAccessory;
  model?: string;
  enabled?: boolean;
};

export const BOT_TEMPLATES = (): (BotDraft & { pitch: string })[] => [
  {
    id: "courrier",
    name: tr("Margot", "Maya"),
    title: tr("Courrier", "Mail"),
    pitch: tr("Trie tes mails, prépare les réponses, suit ton agenda.", "Triages your mail, drafts replies, keeps your calendar."),
    role: tr(
      "Tient ma boîte mail et mon agenda : trie, prépare les réponses en brouillon dans ma voix, suit ce qui attend une réponse, les colis et l'administratif. N'envoie jamais rien sans mon accord.",
      "Keeps my inbox and calendar: triages, drafts replies in my voice, follows what awaits an answer, parcels and paperwork. Never sends anything without my go-ahead.",
    ),
    provider: "claude",
    shape: "pill",
    color: "#EC4899",
    accessory: "bow",
  },
  {
    id: "veille",
    name: "Iris",
    title: tr("Veille", "Radar"),
    pitch: tr("Surveille ce qui se dit de tes projets et de ton marché.", "Watches what is said about your projects and market."),
    role: tr(
      "Surveille ce qui se dit de mes projets et de leur marché (concurrents, Hacker News, GitHub, stores) et ne remonte que ce qui demande une action.",
      "Watches what is said about my projects and their market (competitors, Hacker News, GitHub, stores) and only reports what calls for action.",
    ),
    provider: "codex",
    shape: "blob",
    color: "#0EA5E9",
    accessory: "glasses",
  },
  {
    id: "atelier",
    name: "Hugo",
    title: tr("Atelier", "Workshop"),
    pitch: tr("Répare la CI, relit les PR, garde tes projets en forme.", "Fixes CI, reviews PRs, keeps your projects healthy."),
    role: tr(
      "Veille à la santé technique de mes projets : CI cassée, dépendances, PR à relire, déploiements. Répare dans une branche et ouvre une PR, jamais directement sur main.",
      "Keeps my projects technically healthy: broken CI, dependencies, PRs to review, deployments. Fixes on a branch and opens a PR, never directly on main.",
    ),
    provider: "codex",
    shape: "square",
    color: "#F97316",
    accessory: "antenna",
  },
  {
    id: "tresor",
    name: tr("Félix", "Felix"),
    title: tr("Trésor", "Money"),
    pitch: tr("Suit tes abonnements, tes dépenses et tes revenus.", "Follows your subscriptions, spending and income."),
    role: tr(
      "Suit mon argent : abonnements, paiements en échec, dépenses, revenus. Propose des économies et prépare les démarches, ne paie jamais rien lui-même.",
      "Follows my money: subscriptions, failing payments, spending, income. Suggests savings and prepares the steps, never pays anything itself.",
    ),
    provider: "claude",
    shape: "hexagon",
    color: "#22C55E",
    accessory: "crown",
  },
  {
    id: "plume",
    name: tr("Léa", "Lea"),
    title: tr("Plume", "Writer"),
    pitch: tr("Écrit pour toi : posts, notes de version, docs.", "Writes for you: posts, release notes, docs."),
    role: tr(
      "Écrit pour moi dans mon style : posts, notes de version, pages, documentation. Propose des brouillons, ne publie jamais sans mon accord.",
      "Writes for me in my style: posts, release notes, pages, documentation. Proposes drafts, never publishes without my go-ahead.",
    ),
    provider: "claude",
    shape: "flower",
    color: "#8B5CF6",
    accessory: "sprout",
  },
];

export type RoutineDraft = {
  id: string;
  name?: string;
  at?: string;
  days?: number[];
  on?: string[];
  task?: "refresh-life" | "improve-zenith";
  every?: string;
  from?: string;
  until?: string;
  skill?: string;
  prompt?: string;
  bot?: string;
  project?: string;
  enabled?: boolean;
};

/** The team's autonomy, as routines you switch on: learning, looking around, upkeep, improving zenith. */
export const AUTONOMY_ROUTINES = (bots: Set<string>): { key: string; label: string; pitch: string; routine: RoutineDraft }[] => [
  {
    key: "apprendre",
    label: tr("Apprendre de toi", "Learn from you"),
    pitch: tr("Chaque nuit : relit la journée, retient tes préférences, améliore ses skills — et sa façon d'apprendre.", "Every night: rereads the day, keeps your preferences, improves its skills — and its way of learning."),
    routine: { id: "apprendre", name: tr("Apprendre", "Learn"), at: "03:30", skill: "reflect" },
  },
  {
    key: "tour",
    label: tr("Être proactifs", "Be proactive"),
    pitch: tr("Toutes les 3 h en journée : regarde ce qui approche et prépare avant que tu demandes.", "Every 3 h in the day: looks at what's coming and prepares before you ask."),
    routine: { id: "tour", name: tr("Faire le tour", "Look around"), every: "3h", from: "09:00", until: "21:00", skill: "heartbeat" },
  },
  {
    key: "entretien",
    label: tr("Tout garder à jour", "Keep everything up to date"),
    pitch: tr("Le lundi : dépendances et failles de chaque projet, en PR à relire.", "On Mondays: each project's dependencies and vulnerabilities, as PRs to review."),
    routine: { id: "entretien", name: tr("Entretien", "Upkeep"), at: "09:00", days: [1], skill: "upkeep", ...(bots.has("atelier") ? { bot: "atelier" } : {}) },
  },
  {
    key: "ameliorer",
    label: tr("Améliorer zenith", "Improve zenith"),
    pitch: tr("Chaque nuit : une petite amélioration de l'app, tirée de ce qui te gêne.", "Every night: one small improvement to the app, drawn from what bothers you."),
    routine: { id: "ameliorer", name: tr("Améliorer zenith", "Improve zenith"), at: "04:00", task: "improve-zenith", project: "zenith" },
  },
];

/** Routines worth proposing, given which bots exist. */
export const ROUTINE_TEMPLATES = (bots: Set<string>): (RoutineDraft & { pitch: string })[] =>
  [
    { id: "matin", name: tr("Relevé du matin", "Morning capture"), at: "07:30", task: "refresh-life" as const, pitch: tr("Relève tes mails et ton agenda chaque matin (connecteurs Gmail et Google Agenda de Claude).", "Captures your mail and calendar every morning (Claude's Gmail and Google Calendar connectors).") },
    { id: "journee", name: tr("Ma journée", "My day"), at: "07:45", skill: "plan-day", pitch: tr("Prépare ta journée : rendez-vous, priorités.", "Plans your day: appointments, priorities.") },
    { id: "bilan", name: tr("Bilan de la semaine", "Weekly review"), at: "18:00", days: [5], skill: "weekly-review", pitch: tr("Le vendredi soir : ce qui a avancé, ce qui a glissé, lundi.", "Friday evening: what moved, what slipped, Monday.") },
    ...AUTONOMY_ROUTINES(bots)
      .filter((a) => a.key !== "ameliorer")
      .map((a) => ({ ...a.routine, pitch: a.pitch })),
    ...(bots.has("veille") ? [{ id: "veille", name: tr("Veille du matin", "Morning watch"), at: "08:00", days: [1, 2, 3, 4, 5], bot: "veille", skill: "watch", pitch: tr("En semaine, ce qui se dit de tes projets.", "On weekdays, what is said about your projects.") }] : []),
    ...(bots.has("atelier") ? [{ id: "ci", name: tr("Réparer la CI", "Fix the CI"), on: ["ci"], bot: "atelier", pitch: tr("Dès qu'une CI casse, l'atelier s'en occupe.", "As soon as a CI breaks, the workshop takes it.") }] : []),
    ...(bots.has("courrier") ? [{ id: "reponses", name: tr("Brouillons de réponse", "Reply drafts"), on: ["reply", "sale"], bot: "courrier", skill: "reply-email", pitch: tr("Chaque e-mail qui attend une réponse reçoit un brouillon.", "Every email awaiting an answer gets a draft.") }] : []),
  ];
