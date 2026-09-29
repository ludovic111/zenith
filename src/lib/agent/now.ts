import "server-only";
import { createHash } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { PROJECTS } from "../projects";
import { tr } from "../i18n";
import { ago } from "../format";
import { source } from "../source";
import { urgent } from "../subscriptions";
import { isUp, uptime } from "../sources/uptime";
import { life } from "../sources/life";
import { apple, upcomingBirthdays } from "../sources/apple";
import { groupNotifications, notifications } from "../sources/github";
import { askLog } from "./ask";
import { LIFE } from "./target";
import { refreshLifePrompt } from "./tasks";

/**
 * Now: what is waiting for you, from everything zenith knows, each with the request
 * that hands it to an agent. Done and snoozed items are remembered in .data/now.json;
 * an item that comes back changed (a new email, another failure) is a new item.
 */

export type NowKind = "down" | "payment" | "birthday" | "sale" | "reply" | "civic" | "ci" | "refresh";

export type NowItem = {
  id: string;
  kind: NowKind;
  title: string;
  detail: string;
  /** When it happened or is due. */
  at: string | null;
  /** Project it belongs to, for its color. */
  project: string | null;
  /** Where the agent works on it, and what it is asked. */
  target: string;
  prompt: string;
  /** Where to handle it yourself. */
  href: string | null;
  priority: number;
  /** The agent already on it. */
  delegated: { environmentId: string; threadId: string; at: string } | null;
};

type Mark = { done?: string; snoozedUntil?: string };

const FILE = path.join(process.cwd(), ".data", "now.json");
const fold = (s: string) => s.toLowerCase().normalize("NFD").replace(/[\u0300-\u036f]/g, "");
const hash = (s: string) => createHash("sha1").update(s).digest("hex").slice(0, 10);
const DAY = 864e5;

async function marks(): Promise<Record<string, Mark>> {
  try {
    return JSON.parse(await readFile(FILE, "utf8")) as Record<string, Mark>;
  } catch {
    return {};
  }
}

export async function mark(id: string, action: "done" | "snooze" | "restore", hours = 20) {
  const all = await marks();
  if (action === "restore") delete all[id];
  else if (action === "done") all[id] = { done: new Date().toISOString() };
  else all[id] = { snoozedUntil: new Date(Date.now() + hours * 3600e3).toISOString() };
  // Forget marks older than 60 days.
  for (const [k, v] of Object.entries(all)) {
    const t = Date.parse(v.done ?? v.snoozedUntil ?? "");
    if (Number.isFinite(t) && t < Date.now() - 60 * DAY) delete all[k];
  }
  await mkdir(path.dirname(FILE), { recursive: true });
  await writeFile(FILE, JSON.stringify(all, null, 2));
}

const projectOfRepo = (repo: string) => PROJECTS.find((p) => p.repo?.toLowerCase() === repo.toLowerCase()) ?? null;

async function collectItems(): Promise<Omit<NowItem, "delegated">[]> {
  const [probes, l, a, gh] = await Promise.all([uptime().catch(() => []), source(life), source(apple), source(notifications)]);
  const items: Omit<NowItem, "delegated">[] = [];

  // Down means two failed checks in a row; everything failing at once is this Mac's network.
  const failing = (x: (typeof probes)[number]) => x.samples.length >= 2 && x.samples.slice(-2).every((s) => !isUp(s));
  const down = probes.filter(failing);
  for (const p of down.length === probes.length && probes.length > 1 ? [] : down) {
    const proj = PROJECTS.find((x) => x.id === p.project);
    const since = [...p.samples].reverse().find(isUp);
    items.push({
      id: `down:${hash(p.project + p.url)}`,
      kind: "down",
      title: tr(`${proj?.name ?? p.project} ne répond plus`, `${proj?.name ?? p.project} is down`),
      detail: `${p.label} · ${p.url}`,
      at: since ? new Date(since.t).toISOString() : null,
      project: p.project,
      target: proj ? p.project : LIFE,
      prompt: tr(
        `${p.label} (${p.url}) ne répond plus${since ? ` depuis ${ago(since.t)}` : ""}. Diagnostique : état du service, logs, derniers déploiements et commits. Propose le correctif et applique-le sur une branche ; ne déploie rien en production sans me demander.`,
        `${p.label} (${p.url}) is down${since ? ` since ${ago(since.t)}` : ""}. Diagnose it: service status, logs, latest deploys and commits. Propose the fix and apply it on a branch; don't deploy anything to production without asking me.`,
      ),
      href: p.url,
      priority: 100,
    });
  }

  for (const s of urgent()) {
    const due = s.next_renewal ? Date.parse(s.next_renewal) : null;
    items.push({
      id: `payment:${hash(s.name + s.evidence)}`,
      kind: "payment",
      title: tr(`Paiement en échec · ${s.name}`, `Failed payment · ${s.name}`),
      detail: s.evidence,
      at: s.next_renewal,
      project: s.project,
      target: LIFE,
      prompt: tr(
        `Mon paiement « ${s.name} »${s.vendor ? ` (${s.vendor})` : ""} est en échec : ${s.evidence}. Trouve dans mes e-mails ce qui est demandé exactement (montant, échéance, lien), dis-moi quel moyen de paiement mettre à jour et où${s.manage_url ? ` (page de gestion : ${s.manage_url})` : ""}, en 3 étapes. Ne paie rien toi-même.`,
        `My payment for "${s.name}"${s.vendor ? ` (${s.vendor})` : ""} is failing: ${s.evidence}. Find in my emails exactly what is asked (amount, deadline, link), tell me which payment method to update and where${s.manage_url ? ` (management page: ${s.manage_url})` : ""}, in 3 steps. Don't pay anything yourself.`,
      ),
      href: s.manage_url ?? null,
      priority: 90 + (due && due - Date.now() < 3 * DAY ? 5 : 0),
    });
  }

  for (const b of upcomingBirthdays(a.ok ? a.data : null, 2)) {
    const when = b.inDays === 0 ? tr("aujourd'hui", "today") : b.inDays === 1 ? tr("demain", "tomorrow") : tr("après-demain", "the day after tomorrow");
    items.push({
      id: `birthday:${hash(b.name + b.date)}`,
      kind: "birthday",
      title: tr(`Anniversaire de ${b.name} ${when}`, `${b.name}'s birthday ${when}`),
      detail: b.age ? tr(`${b.age} ans`, `turns ${b.age}`) : tr("Contacts", "Contacts"),
      at: b.date,
      project: null,
      target: LIFE,
      prompt: tr(
        `C'est l'anniversaire de ${b.name} ${when}${b.age ? ` (${b.age} ans)` : ""}. Regarde ce que mes notes et mes e-mails disent de ${b.name}, propose 3 idées de cadeau faisables à temps près de chez moi et écris un message chaleureux que je pourrai envoyer. N'envoie rien.`,
        `It's ${b.name}'s birthday ${when}${b.age ? ` (turning ${b.age})` : ""}. Look at what my notes and emails say about ${b.name}, suggest 3 gift ideas I can still get in time near me, and write a warm message I can send. Don't send anything.`,
      ),
      href: null,
      priority: b.inDays === 0 ? 85 : 72,
    });
  }

  const snap = l.ok ? l.data : null;
  const replies = snap?.inbox.needsReply ?? [];

  // Buyers writing about something you sell: one item per thing sold, all of them at once.
  const words = (x: string) => new Set(fold(x).split(/[^a-z0-9]+/).filter((w) => w.length >= 3));
  const bought = new Set<(typeof replies)[number]>();
  const sold = new Map<string, { item: string; platforms: Set<string> }>();
  for (const s of snap?.sales ?? []) {
    const key = [...words(s.item)].slice(0, 4).join(" ");
    const entry = sold.get(key) ?? { item: s.item, platforms: new Set<string>() };
    entry.platforms.add(s.platform);
    sold.set(key, entry);
  }
  for (const { item, platforms } of sold.values()) {
    const w = words(item);
    const buyers = replies.filter((m) => [...words(m.subject)].filter((x) => w.has(x)).length >= 2);
    if (!buyers.length) continue;
    buyers.forEach((b) => bought.add(b));
    const short = item.split(/\s+/).slice(0, 2).join(" ");
    const names = buyers.map((b) => b.from.replace(/\s*\(.*\)$/, ""));
    const oldest = Math.min(...buyers.map((b) => Date.parse(b.date)));
    const list = buyers.map((b) => `- ${b.from} : « ${b.subject} » — ${b.why}${b.link ? ` (${b.link})` : ""}`).join("\n");
    const listEn = buyers.map((b) => `- ${b.from}: "${b.subject}" — ${b.why}${b.link ? ` (${b.link})` : ""}`).join("\n");
    items.push({
      id: `sale:${hash(item + buyers.map((b) => b.link || b.from).sort().join())}`,
      kind: "sale",
      title:
        buyers.length > 1
          ? tr(`${buyers.length} acheteurs attendent · ${short}`, `${buyers.length} buyers waiting · ${short}`)
          : tr(`Un acheteur attend · ${short}`, `A buyer is waiting · ${short}`),
      detail: `${names.join(", ")} · ${[...platforms].join(", ")}`,
      at: new Date(oldest).toISOString(),
      project: null,
      target: LIFE,
      prompt: tr(
        `Je vends « ${item} » sur ${[...platforms].join(" et ")}. Ces acheteurs attendent ma réponse :\n${list}\n\nLis chaque fil. Dis-moi en une ligne par personne où elle en est (question, offre, relance) et recommande à qui vendre et à quel prix. Puis prépare un brouillon Gmail de réponse dans chaque fil, dans mon ton. N'envoie rien.`,
        `I'm selling "${item}" on ${[...platforms].join(" and ")}. These buyers are waiting for my reply:\n${listEn}\n\nRead each thread. Tell me in one line per person where they stand (question, offer, follow-up) and recommend who to sell to and at what price. Then prepare a Gmail draft reply in each thread, in my voice. Don't send anything.`,
      ),
      href: buyers[0].link || null,
      priority: 66 + Math.min(10, buyers.length * 2),
    });
  }

  for (const m of replies) {
    if (bought.has(m)) continue;
    const age = Date.now() - Date.parse(m.date);
    items.push({
      id: `reply:${hash(m.link || m.from + m.subject)}`,
      kind: "reply",
      title: tr(`Répondre à ${m.from}`, `Reply to ${m.from}`),
      detail: `${m.subject} · ${m.why}`,
      at: m.date,
      project: null,
      target: LIFE,
      prompt: tr(
        `${m.from} attend ma réponse : « ${m.subject} » (${m.why}). Lis tout le fil${m.link ? ` (${m.link})` : ""}, puis rédige une réponse courte dans mon ton et crée-la en brouillon Gmail dans ce fil. Montre-la-moi ; ne l'envoie pas.`,
        `${m.from} is waiting for my reply: "${m.subject}" (${m.why}). Read the whole thread${m.link ? ` (${m.link})` : ""}, then write a short reply in my voice and save it as a Gmail draft in that thread. Show it to me; don't send it.`,
      ),
      href: m.link || null,
      priority: 60 + Math.min(10, Math.floor(age / DAY) * 2),
    });
  }

  // Paperwork: what is due within a month, or arrived in the last ten days; not what is already a reply.
  const replyLinks = new Set(replies.map((m) => m.link).filter(Boolean));
  for (const c of snap?.civic ?? []) {
    if (c.link && replyLinks.has(c.link)) continue;
    const t = c.date ? Date.parse(c.date) : null;
    if (t && (t - Date.now() > 30 * DAY || Date.now() - t > 10 * DAY)) continue;
    items.push({
      id: `civic:${hash(c.title + c.note)}`,
      kind: "civic",
      title: c.title,
      detail: c.note,
      at: c.date,
      project: null,
      target: LIFE,
      prompt: tr(
        `Aide-moi avec ceci : ${c.title} — ${c.note}${c.link ? ` (${c.link})` : ""}. Dis-moi ce qui est attendu de moi et pour quand, et prépare tout ce que tu peux (brouillon de réponse, formulaire, rappel dans l'agenda). N'envoie rien sans me demander.`,
        `Help me with this: ${c.title} — ${c.note}${c.link ? ` (${c.link})` : ""}. Tell me what is expected of me and by when, and prepare whatever you can (draft reply, form, calendar reminder). Don't send anything without asking.`,
      ),
      href: c.link || null,
      priority: t && t > Date.now() && t - Date.now() < 7 * DAY ? 80 : 56,
    });
  }

  // A failing workflow on a main branch, once per repository.
  const seen = new Set<string>();
  for (const n of groupNotifications(gh.ok ? gh.data : [])) {
    if (n.reason !== "ci_activity" || !/fail/i.test(n.title) || !/\b(main|master)\b/i.test(n.title) || seen.has(n.repo)) continue;
    if (Date.now() - Date.parse(n.at) > 14 * DAY) continue;
    seen.add(n.repo);
    const proj = projectOfRepo(n.repo);
    items.push({
      id: `ci:${hash(n.repo + n.title)}`,
      kind: "ci",
      title: tr(`CI en échec · ${proj?.name ?? n.repo}`, `CI failing · ${proj?.name ?? n.repo}`),
      detail: n.count > 1 ? tr(`${n.title} (${n.count} fois)`, `${n.title} (${n.count} times)`) : n.title,
      at: n.at,
      project: proj?.id ?? null,
      target: proj?.id ?? LIFE,
      prompt: tr(
        `Sur ${n.repo}, « ${n.title} ». Trouve pourquoi (gh run list --status failure, gh run view --log-failed), corrige la cause sur une nouvelle branche et ouvre une PR. Ne pousse pas sur main.`,
        `On ${n.repo}: "${n.title}". Find out why (gh run list --status failure, gh run view --log-failed), fix the cause on a new branch and open a PR. Don't push to main.`,
      ),
      href: n.url,
      priority: 50,
    });
  }

  const captured = snap ? Date.parse(snap.capturedAt) : null;
  if (!captured || Date.now() - captured > 20 * 3600e3) {
    const day = new Date().toISOString().slice(0, 10);
    items.push({
      id: `refresh:${day}`,
      kind: "refresh",
      title: tr("Actualiser mes e-mails et mon agenda", "Refresh my email and calendar"),
      detail: captured ? tr(`Dernier relevé ${ago(captured)}`, `Last captured ${ago(captured)}`) : tr("Jamais relevés", "Never captured"),
      at: snap?.capturedAt ?? null,
      project: null,
      target: LIFE,
      prompt: refreshLifePrompt(),
      href: null,
      priority: 45,
    });
  }
  return items;
}

/** Everything waiting for you, most pressing first, without what you handled or snoozed. */
export async function now(): Promise<NowItem[]> {
  const [items, m, log] = await Promise.all([collectItems(), marks(), askLog()]);
  const t = Date.now();
  return items
    .filter((i) => !m[i.id]?.done && !(m[i.id]?.snoozedUntil && Date.parse(m[i.id].snoozedUntil!) > t))
    .map((i) => {
      const d = log.find((e) => e.nowId === i.id);
      return { ...i, delegated: d ? { environmentId: d.environmentId, threadId: d.threadId, at: d.at } : null };
    })
    .sort((x, y) => y.priority - x.priority);
}

export async function findNow(id: string): Promise<NowItem | null> {
  return (await now()).find((i) => i.id === id) ?? null;
}
