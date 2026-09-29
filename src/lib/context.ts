import "server-only";
import { mkdir, readdir, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { config } from "./config";
import { PROJECTS, projectDir, type Project, type ProjectId } from "./projects";
import { OWNER, identityOf } from "./identity";
import { SUBSCRIPTIONS, CATEGORIES, monthly, urgent } from "./subscriptions";
import { collect } from "./extensions";
import { source, type Source } from "./source";
import { isFr, l10n, tr } from "./i18n";
import { ago as agoFmt } from "./format";
import { uptime } from "./sources/uptime";
import { localRepo, projectVersion } from "./sources/git";
import { githubMentions, groupNotifications, notifications, openIssues, openPulls, profile, recentStars, runs } from "./sources/github";
import { billing, deployments, traffic } from "./sources/railway";
import { domainInfo } from "./sources/domains";
import { isLive, sessions } from "./sources/agents";
import { claudePlan, codexPlan, fx, type Rates } from "./sources/plans";
import { life, rhythm } from "./sources/life";
import { apple, screenDays, screenToday, upcomingBirthdays } from "./sources/apple";
import { air, aqiLabel, nextHolidays, pollenLevel, water } from "./sources/environment";
import { departures } from "./sources/transit";
import { hackerNews, hnMentions, localNews } from "./sources/news";
import { crypto } from "./sources/markets";
import { brewOutdated, devServers, machine } from "./sources/machine";
import { notes, todo, vaultPath, EXPORT_DIR } from "./sources/obsidian";
import { weather, describe } from "./sources/weather";

/**
 * The brief for AI agents: everything zenith knows, in Markdown, most important first.
 * Every source is optional: a failing one becomes an "unavailable" line, never an error.
 * File names stay the same in every language so tools and links never break.
 */

const dt = (t: string | number, o: Intl.DateTimeFormatOptions = { weekday: "short", day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" }) =>
  new Intl.DateTimeFormat(l10n().locale, { timeZone: l10n().timeZone, ...o }).format(new Date(t));
const d = (t: string | number) => dt(t, { day: "numeric", month: "short", year: "numeric" });
const money = (v: number | null, c: string) =>
  v == null ? tr("prix inconnu", "unknown price") : new Intl.NumberFormat(l10n().locale, { style: "currency", currency: c, maximumFractionDigits: 2 }).format(v);
const ago = (t: number | string) => agoFmt(t);
const val = <T,>(s: Source<T>): T | null => (s.ok ? s.data : null);
/** Env vars are UPPER_CASE; config keys are lowercase and dotted ("location.country"). */
const isConfigKey = (k: string) => /^[a-z]/.test(k);
const why = (s: Source<unknown>) => {
  if (s.ok) return "";
  if (!s.missing) return tr(`inaccessible (${s.error})`, `unavailable (${s.error})`);
  const cfg = s.missing.filter(isConfigKey);
  const env = s.missing.filter((k) => !isConfigKey(k));
  return [
    env.length ? tr(`à brancher (${env.join(", ")})`, `not connected (${env.join(", ")})`) : null,
    cfg.length ? tr(`à renseigner dans zenith.config.json (${cfg.join(", ")})`, `set ${cfg.join(", ")} in zenith.config.json`) : null,
  ]
    .filter(Boolean)
    .join(", ");
};
/** « quote » in French, "quote" in English. */
const q = (s: string) => (isFr() ? `« ${s} »` : `"${s}"`);
/** "Label : value" in French, "Label: value" in English. */
const colon = () => (isFr() ? " : " : ": ");
/** List separator: " ; " in French, "; " in English. */
const semi = () => (isFr() ? " ; " : "; ");
/** "42 %" in French, "42%" in English. */
const pc = (n: number | string) => (isFr() ? `${n} %` : `${n}%`);
/** Billing period, after an amount. */
const per = (period: string) =>
  ({ month: tr("/mois", "/month"), year: tr("/an", "/year"), week: tr("/semaine", "/week"), usage: tr(" à l'usage", " usage-based") })[period] ?? ` ${period}`;
const STATUS = (status: string) => ({ active: tr("actif", "active"), failing: tr("en échec", "failing"), cancelled: tr("arrêté", "cancelled"), unknown: tr("à vérifier", "to check") })[status] ?? status;
const hours = (seconds: number) => Math.round((seconds / 3600) * 10) / 10;
const tilde = (p: string) => (p.startsWith(os.homedir()) ? `~${p.slice(os.homedir().length)}` : p);
const agentName = (a: string) => (a === "claude" ? "Claude Code" : "Codex");


const place = () => config().location?.name ?? "";
const ghLogin = () => config().owner.socials.find((s) => s.network === "github")?.handle.replace(/^@/, "") ?? config().owner.sponsors ?? null;
const watchTerms = () => config().watch.map((w) => w.term);
const hasTransit = () => Boolean(config().transit || process.env.TRANSIT_STOP);
const hasWater = () => Boolean(config().water?.stations.length);
const skip = <T,>(): Promise<Source<T>> => Promise.resolve({ ok: false, missing: ["zenith.config.json"] });

async function health(p: Project) {
  const probes = (await uptime()).filter((u) => u.project === p.id);
  if (!probes.length || probes.some((u) => u.up == null)) return tr("état inconnu", "status unknown");
  return probes.every((u) => u.up)
    ? tr("en ligne", "online") + ` (${probes.map((u) => `${u.label} ${u.last?.ms ?? "?"} ms`).join(", ")})`
    : tr("HORS LIGNE", "OFFLINE") + `${colon()}${probes.filter((u) => !u.up).map((u) => u.label).join(", ")}`;
}
const isDown = (h: string) => h.startsWith(tr("HORS LIGNE", "OFFLINE"));

/** The 3 to 5 numbers that sum up a project: its repo, then what extensions know, then traffic. */
async function keyFacts(p: Project): Promise<string[]> {
  const out: string[] = [];
  const [repo, ext] = await Promise.all([
    projectDir(p) ? source(() => localRepo(p.id)) : skip<Awaited<ReturnType<typeof localRepo>>>(),
    collect((e) => (e.facts ? () => e.facts!(p) : undefined)),
  ]);
  const r = val(repo);
  if (r) {
    const n = r.commits.filter((c) => c.at > Date.now() - 30 * 864e5).length;
    const last = r.commits[0];
    out.push(
      tr(
        `${n} commits en 30 j, dernier ${last ? ago(last.at) : "—"} (« ${last?.subject ?? ""} »)`,
        `${n} commits in 30 d, last ${last ? ago(last.at) : "—"} ("${last?.subject ?? ""}")`,
      ),
    );
    if (r.dirty) out.push(tr(`${r.dirty} fichier(s) non commité(s) sur ${r.branch}`, `${r.dirty} uncommitted file(s) on ${r.branch}`));
  }
  out.push(...ext.filter(Boolean));
  if (p.railway) {
    const t = val(await source(() => traffic(p)));
    if (t) out.push(tr(`Trafic : ${t.requests} requêtes, ${t.visitors} IP uniques depuis ${ago(t.since)}`, `Traffic: ${t.requests} requests, ${t.visitors} unique IPs since ${ago(t.since)}`));
  }
  return out;
}

/** Extra Markdown sections that extensions add to a project's document. */
async function extraContext(p: Project) {
  const parts = await collect((e) => (e.context ? async () => [await e.context!(p)] : undefined));
  return parts.map((s) => s.trim()).filter(Boolean);
}

function todosFor(id: ProjectId) {
  return [...identityOf(id).todo, ...urgent().filter((s) => s.project === id).map((s) => `${s.name}${colon()}${s.evidence}`)];
}

/** Money in and out reported by extensions (RevenueCat, Sponsors, your own). */
async function moneyLines() {
  const rows = await collect((e) => e.money);
  return rows.map((r) => ({ ...r, text: `${r.label} ${r.value == null ? "—" : money(r.value, r.currency)}${r.hint ? ` (${r.hint})` : ""}` }));
}

function subscriptionsPerMonth(rates: Rates) {
  const table = rates.toBase;
  const active = SUBSCRIPTIONS.filter((s) => s.status === "active" || s.status === "failing");
  return { active, total: active.reduce((acc, s) => acc + monthly(s) * (table[s.currency] ?? 1), 0) };
}

const usesRailway = () => PROJECTS.some((p) => p.railway) || Boolean(process.env.RAILWAY_TOKEN);

export async function brief() {
  const loc = config().location;
  const [w, l, a, t, rh, agents, rates, claude, codex, bill, aq, hol, men, notif, flows] = await Promise.all([
    loc ? source(weather) : skip<Awaited<ReturnType<typeof weather>>>(),
    source(life),
    source(apple),
    source(todo),
    source(rhythm),
    source(sessions),
    fx(),
    source(claudePlan),
    source(codexPlan),
    usesRailway() ? source(billing) : skip<Awaited<ReturnType<typeof billing>>>(),
    loc ? source(air) : skip<Awaited<ReturnType<typeof air>>>(),
    loc?.country ? source(() => nextHolidays(2)) : skip<Awaited<ReturnType<typeof nextHolidays>>>(),
    watchTerms().length ? source(hnMentions) : skip<Awaited<ReturnType<typeof hnMentions>>>(),
    source(notifications),
    moneyLines(),
  ]);
  const lines: string[] = [];
  const now = dt(Date.now(), { weekday: "long", day: "numeric", month: "long", year: "numeric", hour: "2-digit", minute: "2-digit" });
  const tz = l10n().timeZone;
  lines.push(OWNER.name ? tr(`# zenith — brief de ${OWNER.name}`, `# zenith — ${OWNER.name}'s brief`) : "# zenith — brief", "");
  lines.push(
    tr(
      `> Généré le ${now} (heure locale, ${tz}) par zenith, son tableau de bord privé. Détail : projets/*.md, vie.md, argent.md, annuaire.md, veille.md.`,
      `> Generated on ${now} (local time, ${tz}) by zenith, their private dashboard. Details: projets/*.md (projects), vie.md (life), argent.md (money), annuaire.md (directory), veille.md (watch).`,
    ),
    "",
  );

  const who: string[] = [];
  if (OWNER.name) who.push(`${OWNER.name}${OWNER.place ? `, ${OWNER.place}` : ""}.`);
  who.push(tr("Développe ses projets avec des agents IA (Claude Code, Codex).", "Builds their projects with AI agents (Claude Code, Codex)."));
  if (OWNER.emails.length) who.push(tr("E-mails : ", "Emails: ") + OWNER.emails.map((e) => e.address).join(", ") + ".");
  if (OWNER.socials.length) who.push(tr("Réseaux : ", "Socials: ") + OWNER.socials.map((s) => `${s.network} ${s.handle}`).join(", ") + ".");
  if (ghLogin()) who.push(`GitHub${colon()}${ghLogin()}.`);
  lines.push(tr("## Qui", "## Who"), "", who.join(" "), "");

  const healths = await Promise.all(PROJECTS.map(async (p) => [p, await health(p)] as const));
  lines.push(tr("## Urgent", "## Urgent"), "");
  const urgentLines = [
    ...urgent().map((s) => `- ⚠️ ${s.name}${colon()}${s.evidence}`),
    ...healths.filter(([, h]) => isDown(h)).map(([p, h]) => `- ⚠️ ${p.name} ${h}`),
  ];
  lines.push(...(urgentLines.length ? urgentLines : [tr("- Rien d'urgent.", "- Nothing urgent.")]), "");

  lines.push(tr("## Projets", "## Projects"), "");
  if (!PROJECTS.length) lines.push(tr("Aucun projet configuré : ajoute-les dans zenith.config.json (`projects`).", "No projects configured: add them to zenith.config.json (`projects`)."), "");
  for (const [p, h] of healths) {
    const [v, facts] = await Promise.all([projectVersion(p.id), keyFacts(p)]);
    lines.push(`### ${p.name}${v ? ` v${v}` : ""}${p.tagline ? ` — ${p.tagline}` : ""}`, "", `- ${tr("État", "Status")}${colon()}${h}${p.site ? ` · ${p.site}` : ""}`, ...facts.map((f) => `- ${f}`));
    const todos = todosFor(p.id);
    if (todos.length) lines.push(`- ${tr("À faire", "To do")}${colon()}${todos.join(semi())}`);
    const dir = projectDir(p);
    const where = [dir ? tilde(dir) : null, p.repo ? `(github.com/${p.repo})` : null].filter(Boolean).join(" ");
    lines.push(`- ${where ? `${tr("Dépôt", "Repository")}${colon()}${where} · ` : ""}${tr("fiche", "details")}${colon()}projets/${p.id}.md`, "");
  }

  lines.push(tr("## Aujourd'hui", "## Today"), "");
  const wx = val(w);
  const ax = val(aq);
  if (wx)
    lines.push(
      `- ${tr("Météo", "Weather")}${place() ? ` ${place()}` : ""}${colon()}${Math.round(wx.now.temp)}°, ${describe(wx.now.code, wx.now.isDay).label.toLowerCase()}${semi()}${Math.round(wx.days[0].min)}°/${Math.round(wx.days[0].max)}°${
        ax
          ? `${semi()}${tr("air", "air")} ${aqiLabel(ax.aqi).label.toLowerCase()}, ${tr("UV max", "max UV")} ${Math.round(ax.uvMax)}${ax.pollen.length ? `, ${tr("pollens", "pollen")}${colon()}${ax.pollen.map((p) => `${p.label.toLowerCase()} ${pollenLevel(p.value)}`).join(", ")}` : ""}`
          : ""
      }.`,
    );
  const bdays = upcomingBirthdays(val(a), 7);
  if (bdays.length)
    lines.push(
      `- ${tr("Anniversaires", "Birthdays")}${colon()}${bdays
        .map((b) => `${b.name} ${b.inDays === 0 ? tr("AUJOURD'HUI", "TODAY") : tr(`dans ${b.inDays} j`, `in ${b.inDays} d`)}${b.age ? tr(` (${b.age} ans)`, ` (${b.age})`) : ""}`)
        .join(semi())}`,
    );
  const hl = val(hol);
  if (hl?.length) lines.push(`- ${tr("Prochains fériés", "Next public holidays")}${place() ? tr(` à ${place()}`, ` in ${place()}`) : ""}${colon()}${hl.map((h) => `${h.name} (${d(h.date)})`).join(", ")}`);
  const events = [...(val(l)?.agenda ?? []).map((e) => ({ ...e, src: "Google" })), ...(val(a)?.calendar.events ?? []).map((e) => ({ ...e, src: "Apple" }))]
    .filter((e) => new Date(e.end ?? e.start).getTime() > Date.now())
    .sort((x, y) => x.start.localeCompare(y.start));
  lines.push(
    events.length
      ? `- ${tr("Agenda", "Calendar")}${colon()}${events.slice(0, 6).map((e) => `${e.title} (${e.allDay ? d(e.start) : dt(e.start)})`).join(semi())}`
      : tr("- Agenda : rien de prévu sur 14 jours.", "- Calendar: nothing planned in the next 14 days."),
  );
  const todoItems = val(t)?.items.filter((i) => !i.done) ?? [];
  if (todoItems.length) lines.push(`- ${tr("Todo Obsidian", "Obsidian to-do")}${colon()}${todoItems.map((i) => i.text).join(semi())}`);
  const rem = val(a)?.reminders.items ?? [];
  if (rem.length) lines.push(`- ${tr("Rappels Apple", "Apple Reminders")}${colon()}${rem.slice(0, 8).map((r) => r.title).join(semi())}`);
  const snap = val(l);
  if (snap) {
    if (snap.inbox.needsReply.length) lines.push(`- ${tr("Attendent une réponse", "Waiting for a reply")}${colon()}${snap.inbox.needsReply.map((m) => `${m.from} (${m.subject})`).join(semi())}`);
    if (snap.sales.length)
      lines.push(`- ${tr("En vente", "For sale")}${colon()}${snap.sales.map((s) => tr(`${s.item} sur ${s.platform} (${s.messages} messages)`, `${s.item} on ${s.platform} (${s.messages} messages)`)).join(semi())}`);
    if (snap.notes.length) lines.push(...snap.notes.map((n) => `- ${n}`));
  }
  const r = val(rh);
  if (r)
    lines.push(
      tr(
        `- Rythme : ${r.weekHours} h de travail des agents et ${r.weekCommits} commits sur 7 jours, ${r.nightHours} h après minuit, ${r.daysOff} jour(s) off.`,
        `- Rhythm: ${r.weekHours} h of agent work and ${r.weekCommits} commits over 7 days, ${r.nightHours} h after midnight, ${r.daysOff} day(s) off.`,
      ),
    );
  const sc = screenToday(val(a));
  if (sc) {
    const top = sc.apps.slice(0, 3).map((x) => `${x.name} ${Math.round(x.seconds / 60)} min`).join(", ");
    lines.push(tr(`- Écran aujourd'hui : ${hours(sc.total)} h actives, surtout ${top}.`, `- Screen time today: ${hours(sc.total)} h active, mostly ${top}.`));
  }
  lines.push("");

  const mentions = val(men) ?? [];
  const nt = groupNotifications(val(notif) ?? []);
  if (mentions.length || nt.length) {
    lines.push(tr("## Veille", "## Watch"), "");
    if (mentions.length)
      lines.push(
        `- ${tr("On parle de toi sur Hacker News", "You're mentioned on Hacker News")}${colon()}${mentions
          .slice(0, 3)
          .map((m) => `${m.label} ${tr("dans", "in")} ${q(m.title)} (${ago(m.at)}) ${m.url}`)
          .join(semi())}`,
      );
    if (nt.length)
      lines.push(
        `- ${tr("Notifications GitHub non lues", "Unread GitHub notifications")}${colon()}${nt
          .slice(0, 5)
          .map((n) => `${n.repo} ${q(n.title)} (${n.reason}${n.count > 1 ? tr(`, ${n.count} fois`, `, ${n.count} times`) : ""})`)
          .join(semi())}`,
      );
    lines.push(tr("- Détail : veille.md", "- Details: veille.md"), "");
  }

  lines.push(tr("## Agents IA", "## AI agents"), "");
  const ag = val(agents) ?? [];
  const live = ag.filter(isLive);
  lines.push(`- ${tr(`${live.length} session(s) active(s)`, `${live.length} active session(s)`)}${live.length ? `${colon()}${live.map((s) => `${agentName(s.agent)} ${q(s.title)}`).join(semi())}` : ""}.`);
  for (const pl of [val(claude), val(codex)].filter((x) => x != null)) {
    // A window that has reset since a frozen snapshot says nothing about current usage.
    const win = pl.windows.map((x) =>
      !pl.live && x.resetsAt && new Date(x.resetsAt).getTime() < Date.now() ? tr(`${x.label} rechargée depuis`, `${x.label} reset since`) : `${x.label} ${pc(x.percentUsed)}`,
    );
    lines.push(`- ${pl.plan}${colon()}${win.join(", ")} (${tr("relevé", "captured")} ${ago(pl.capturedAt)})`);
  }
  lines.push("");

  lines.push(tr("## Argent", "## Money"), "");
  const cur = rates.base;
  const { active, total } = subscriptionsPerMonth(rates);
  const ins = flows.filter((x) => x.sign === 1);
  const outs = flows.filter((x) => x.sign === -1);
  if (ins.length) lines.push(`- ${tr("Entrées", "Income")}${colon()}${ins.map((x) => x.text).join(semi())}.`);
  const railway = usesRailway() ? `${semi()}${tr("Railway ce mois", "Railway this month")} ${val(bill) ? money(val(bill)!.currentUsage, "USD") : why(bill)}` : "";
  const extraOut = outs.length ? `${semi()}${outs.map((x) => x.text).join(semi())}` : "";
  lines.push(
    tr(
      `- Sorties : ~${Math.round(total)} ${cur}/mois d'abonnements (${active.length})${railway}${extraOut}.`,
      `- Spending: ~${Math.round(total)} ${cur}/month of subscriptions (${active.length})${railway}${extraOut}.`,
    ),
  );
  const month = snap?.spending.months[0];
  if (month) {
    const spent = money(Math.round(month.categories.reduce((x, c) => x + c.amount, 0)), snap!.spending.currency || cur);
    lines.push(tr(`- Dépenses perso ${month.month} : ${spent} hors abonnements.`, `- Personal spending ${month.month}: ${spent} excluding subscriptions.`));
  }
  lines.push("");
  return lines.join("\n");
}

export async function projectDoc(p: Project) {
  const id = identityOf(p.id);
  const [h, v, facts, extra, repo, prs, issues, ci, deps, ag, nts] = await Promise.all([
    health(p),
    projectVersion(p.id),
    keyFacts(p),
    extraContext(p),
    projectDir(p) ? source(() => localRepo(p.id)) : skip<Awaited<ReturnType<typeof localRepo>>>(),
    p.repo ? source(() => openPulls(p.repo!)) : skip<Awaited<ReturnType<typeof openPulls>>>(),
    p.repo ? source(() => openIssues(p.repo!)) : skip<Awaited<ReturnType<typeof openIssues>>>(),
    p.repo ? source(() => runs(p.repo!)) : skip<Awaited<ReturnType<typeof runs>>>(),
    p.railway ? source(() => deployments(p)) : Promise.resolve(null),
    source(sessions),
    source(notes),
  ]);
  const L: string[] = [
    `# ${p.name}${v ? ` v${v}` : ""}`,
    "",
    `> ${p.tagline ? `${p.tagline}. ` : ""}${tr(`Fiche générée par zenith le ${dt(Date.now())}.`, `Generated by zenith on ${dt(Date.now())}.`)}`,
    "",
  ];
  L.push(tr("## État", "## Status"), "", `- ${h}`);
  if (p.site) L.push(`- ${tr("Site", "Site")}${colon()}${p.site}`);
  L.push(...facts.map((f) => `- ${f}`), "");
  const todos = todosFor(p.id);
  if (todos.length) L.push(tr("## À faire", "## To do"), "", ...todos.map((t) => `- [ ] ${t}`), "");
  const ident = [
    ...id.names.concat(id.ids).map((f) => `- ${f.label}${colon()}${f.value}`),
    ...(id.domains.length ? [`- ${tr("Domaines", "Domains")}${colon()}${id.domains.join(", ")}`] : []),
    ...(id.emails.length ? [`- ${tr("E-mails", "Emails")}${colon()}${id.emails.map((e) => `${e.address} (${e.role})`).join(", ")}`] : []),
    ...(id.socials.length ? [`- ${tr("Réseaux", "Socials")}${colon()}${id.socials.map((s) => `${s.network} ${s.handle} ${s.url}`).join(", ")}`] : []),
    ...(id.stores.length ? [`- Stores${colon()}${id.stores.map((s) => `${s.label} ${s.value ?? ""} ${s.url ?? ""}`.trim()).join(", ")}`] : []),
    ...(id.services.length ? [`- Services${colon()}${id.services.map((s) => `${s.label}${s.value ? ` (${s.value})` : ""}`).join(", ")}`] : []),
  ];
  if (ident.length) L.push(tr("## Identité", "## Identity"), "", ...ident, "");
  const subs = SUBSCRIPTIONS.filter((s) => s.project === p.id);
  if (subs.length) L.push(tr("## Coûts", "## Costs"), "", ...subs.map((s) => `- ${s.name}${colon()}${money(s.amount, s.currency)}${per(s.period)} (${STATUS(s.status)})`), "");
  for (const block of extra) L.push(block, "");
  const rp = val(repo);
  const dir = projectDir(p);
  if (rp && dir)
    L.push(
      tr("## Code", "## Code"),
      "",
      tr(
        `- Dossier : ${tilde(dir)}, branche ${rp.branch}, ${rp.dirty} fichier(s) non commité(s), ${rp.ahead} commit(s) non poussé(s)`,
        `- Folder: ${tilde(dir)}, branch ${rp.branch}, ${rp.dirty} uncommitted file(s), ${rp.ahead} unpushed commit(s)`,
      ),
      "",
      tr("Derniers commits :", "Latest commits:"),
      ...rp.commits.slice(0, 12).map((c) => `- ${d(c.at)} · ${c.subject}`),
      "",
    );
  const pr = val(prs) ?? [];
  const is = val(issues) ?? [];
  if (pr.length || is.length) L.push(tr("## Ouvert sur GitHub", "## Open on GitHub"), "", ...pr.map((x) => `- PR #${x.number} ${x.title}`), ...is.map((x) => `- Issue #${x.number} ${x.title}`), "");
  const c = val(ci);
  if (c?.length) L.push(tr("## Intégration continue", "## Continuous integration"), "", ...c.slice(0, 5).map((r) => `- ${r.conclusion ?? r.status} · ${r.display_title} (${ago(r.created_at)})`), "");
  const dp = deps ? val(deps) : null;
  if (dp?.length) L.push(tr("## Déploiements Railway", "## Railway deployments"), "", ...dp.slice(0, 5).map((x) => `- ${x.status} ${ago(x.createdAt)} · ${x.meta?.commitMessage?.split("\n")[0] ?? ""}`), "");
  const sess = (val(ag) ?? []).filter((s) => s.project === p.id).slice(0, 8);
  if (sess.length)
    L.push(
      tr("## Sessions d'agents récentes", "## Recent agent sessions"),
      "",
      ...sess.map((s) => `- ${agentName(s.agent)} · ${q(s.title)} · ${ago(s.end)}${s.prs.length ? ` · PR ${s.prs.map((x) => `#${x.number}`).join(", ")}` : ""}`),
      "",
    );
  const n = (val(nts)?.notes ?? []).filter((x) => x.project === p.id);
  if (n.length)
    L.push(tr("## Notes Obsidian liées", "## Related Obsidian notes"), "", ...n.map((x) => `- ${x.path} (${tr("modifiée", "modified")} ${ago(x.modified)})${colon()}${x.excerpt.slice(0, 160)}`), "");
  return L.join("\n");
}

export async function lifeDoc() {
  const loc = config().location;
  const [l, a, t, n, rh, aq, wt, hol, board] = await Promise.all([
    source(life),
    source(apple),
    source(todo),
    source(notes),
    source(rhythm),
    loc ? source(air) : skip<Awaited<ReturnType<typeof air>>>(),
    hasWater() ? source(water) : skip<Awaited<ReturnType<typeof water>>>(),
    loc?.country ? source(() => nextHolidays(5)) : skip<Awaited<ReturnType<typeof nextHolidays>>>(),
    hasTransit() ? source(() => departures()) : skip<Awaited<ReturnType<typeof departures>>>(),
  ]);
  const never = tr("(jamais)", "(never)");
  const L: string[] = [
    tr("# Vie", "# Life"),
    "",
    tr(
      `> Généré par zenith le ${dt(Date.now())}. Agenda Google et Gmail relevés par Claude ${val(l) ? ago(val(l)!.capturedAt) : never} ; Apple relevé par zenith.app ${val(a) ? ago(val(a)!.capturedAt) : never}.`,
      `> Generated by zenith on ${dt(Date.now())}. Google Calendar and Gmail captured by Claude ${val(l) ? ago(val(l)!.capturedAt) : never}; Apple data captured by zenith.app ${val(a) ? ago(val(a)!.capturedAt) : never}.`,
    ),
    "",
  ];
  const snap = val(l);
  const ap = val(a);
  L.push(tr("## Agenda (14 jours)", "## Calendar (14 days)"), "");
  const events = [...(snap?.agenda ?? []).map((e) => ({ ...e, src: "Google" })), ...(ap?.calendar.events ?? []).map((e) => ({ ...e, src: "Apple" }))].sort((x, y) =>
    x.start.localeCompare(y.start),
  );
  L.push(
    ...(events.length
      ? events.map((e) => `- ${e.allDay ? d(e.start) : dt(e.start)} · ${e.title}${e.location ? ` · ${e.location}` : ""} (${e.src} · ${e.calendar})`)
      : [tr("- Rien de prévu.", "- Nothing planned.")]),
    "",
  );
  L.push(tr("## À faire", "## To do"), "");
  L.push(...(val(t)?.items.map((i) => `- [${i.done ? "x" : " "}] ${i.text} (${tr("Todo Obsidian", "Obsidian to-do")})`) ?? []));
  L.push(...(ap?.reminders.items.map((r) => `- [ ] ${r.title}${r.due ? ` · ${tr("échéance", "due")} ${d(r.due)}` : ""} (${tr("Rappels", "Reminders")} · ${r.list})`) ?? []));
  L.push(...urgent().map((s) => `- [ ] ${s.name}${colon()}${s.evidence}`), "");
  if (snap)
    L.push(
      tr("## E-mails", "## Email"),
      "",
      tr(`- Gmail : ${snap.inbox.unread} non lus, ${snap.inbox.unreadImportant} importants`, `- Gmail: ${snap.inbox.unread} unread, ${snap.inbox.unreadImportant} important`),
      ...snap.inbox.needsReply.map((m) => `- ${tr("Attend une réponse", "Waiting for a reply")}${colon()}${m.from} — ${m.subject} (${m.why})`),
    );
  if (ap?.mail.running)
    L.push(
      ...ap.mail.accounts.map((x) => `- ${tr("Mail Apple", "Apple Mail")} · ${x.name}${colon()}${tr(`${x.unread} non lus`, `${x.unread} unread`)}`),
      ...ap.mail.recent.slice(0, 8).map((m) => `- ${tr("Non lu", "Unread")} (${m.account})${colon()}${m.from} — ${m.subject}`),
    );
  L.push("");
  if (snap) {
    const cur = snap.spending.currency || l10n().currency;
    if (snap.sales.length)
      L.push(
        tr("## Ventes", "## Sales"),
        "",
        ...snap.sales.map((s) =>
          tr(`- ${s.item} sur ${s.platform} : ${s.messages} messages, dernier ${ago(s.lastMessageAt)}`, `- ${s.item} on ${s.platform}: ${s.messages} messages, last ${ago(s.lastMessageAt)}`),
        ),
        "",
      );
    if (snap.civic.length) L.push(tr("## Administratif", "## Paperwork"), "", ...snap.civic.map((c) => `- ${c.title}${c.date ? ` (${d(c.date)})` : ""}${colon()}${c.note}`), "");
    if (snap.spending.months.length)
      L.push(
        tr("## Dépenses perso (hors abonnements)", "## Personal spending (excluding subscriptions)"),
        "",
        ...snap.spending.months.map(
          (m) => `- ${m.month}${colon()}${money(Math.round(m.categories.reduce((x, c) => x + c.amount, 0)), cur)} — ${m.categories.map((c) => `${c.label} ${Math.round(c.amount)}`).join(", ")}`,
        ),
        "",
      );
  }
  const r = val(rh);
  if (r)
    L.push(
      tr("## Rythme", "## Rhythm"),
      "",
      tr(
        `- ${r.weekHours} h de travail des agents et ${r.weekCommits} commits sur 7 jours ; ${r.nightHours} h après minuit ; ${r.daysOff} jour(s) off.`,
        `- ${r.weekHours} h of agent work and ${r.weekCommits} commits over 7 days; ${r.nightHours} h after midnight; ${r.daysOff} day(s) off.`,
      ),
      "",
    );
  const ax = val(aq);
  const wx = val(wt) ?? [];
  if (ax || wx.length) {
    L.push(`${tr("## Environnement", "## Environment")}${place() ? tr(` à ${place()}`, ` · ${place()}`) : ""}`, "");
    if (ax)
      L.push(
        tr(
          `- Air : ${aqiLabel(ax.aqi).label} (indice européen ${ax.aqi}), PM2,5 ${ax.pm25} µg/m³, UV max ${Math.round(ax.uvMax)}`,
          `- Air: ${aqiLabel(ax.aqi).label} (European AQI ${ax.aqi}), PM2.5 ${ax.pm25} µg/m³, max UV ${Math.round(ax.uvMax)}`,
        ),
        `- ${tr("Pollens", "Pollen")}${colon()}${
          ax.pollen.length
            ? ax.pollen.map((p) => `${p.label} ${pollenLevel(p.value)} (${Math.round(p.value)} ${tr("grains/m³", "grains/m³")})`).join(", ")
            : tr("rien de notable", "nothing notable")
        }`,
      );
    L.push(
      ...wx.map(
        (x) =>
          `- ${x.water} (${x.station})${colon()}${[
            x.temp != null ? `${x.temp.toFixed(1)}°` : null,
            x.flow == null ? null : x.level ? `${tr("niveau", "level")} ${x.flow} m` : `${tr("débit", "flow")} ${Math.round(x.flow)} m³/s`,
          ]
            .filter(Boolean)
            .join(", ")}`,
      ),
      "",
    );
  }
  const hl = val(hol);
  if (hl?.length) L.push(`${tr("## Jours fériés", "## Public holidays")}${place() ? tr(` à ${place()}`, ` · ${place()}`) : ""}`, "", ...hl.map((h) => `- ${d(h.date)}${colon()}${h.name}`), "");
  const bdays = upcomingBirthdays(ap, 30);
  if (bdays.length)
    L.push(tr("## Anniversaires (30 jours, Contacts)", "## Birthdays (30 days, Contacts)"), "", ...bdays.map((b) => `- ${d(b.date)}${colon()}${b.name}${b.age ? tr(`, ${b.age} ans`, `, turns ${b.age}`) : ""}`), "");
  const bd = val(board);
  if (bd?.departures.length)
    L.push(
      `${tr("## Prochains départs", "## Next departures")} · ${bd.station}`,
      "",
      ...bd.departures.slice(0, 6).map((x) => `- ${dt(x.at, { hour: "2-digit", minute: "2-digit" })} ${x.line} → ${x.to}${x.delay ? ` (+${x.delay} min)` : ""}`),
      "",
    );
  const days = screenDays(ap);
  if (days.length) {
    const week = days.slice(-7);
    const apps = new Map<string, number>();
    for (const day of week) for (const x of day.apps) apps.set(x.name, (apps.get(x.name) ?? 0) + x.seconds);
    L.push(
      tr("## Temps d'écran (zenith.app)", "## Screen time (zenith.app)"),
      "",
      ...week.map((x) => `- ${x.date}${colon()}${hours(x.total)} h`),
      `- ${tr("Apps sur 7 jours", "Apps over 7 days")}${colon()}${[...apps.entries()]
        .sort((x, y) => y[1] - x[1])
        .slice(0, 8)
        .map(([k, v]) => `${k} ${hours(v)} h`)
        .join(", ")}`,
      "",
    );
  }
  const tracks = ap?.music?.recent ?? [];
  if (tracks.length) L.push(tr("## Musique écoutée récemment", "## Recently played"), "", ...tracks.slice(0, 10).map((x) => `- ${x.title} — ${x.artist} (${x.app}, ${ago(x.at)})`), "");
  const nn = val(n);
  if (nn)
    L.push(
      tr("## Carnet Obsidian", "## Obsidian notebook"),
      "",
      tr(`Vault ${nn.vault} (${tilde(nn.root)}), ${nn.notes.length} notes. Les plus récentes :`, `Vault ${nn.vault} (${tilde(nn.root)}), ${nn.notes.length} notes. Most recent:`),
      ...nn.notes.slice(0, 12).map((x) => `- ${x.path} (${ago(x.modified)})${colon()}${x.excerpt.slice(0, 140)}`),
      "",
    );
  return L.join("\n");
}

export async function moneyDoc() {
  const [rates, bill, flows] = await Promise.all([fx(), usesRailway() ? source(billing) : skip<Awaited<ReturnType<typeof billing>>>(), moneyLines()]);
  const cur = rates.base;
  const L = [
    tr("# Argent", "# Money"),
    "",
    tr(
      `> Généré par zenith le ${dt(Date.now())}. Abonnements listés dans zenith.config.json ; change BCE du ${rates.date ?? "jour"}.`,
      `> Generated by zenith on ${dt(Date.now())}. Subscriptions listed in zenith.config.json; ECB exchange rates of ${rates.date ?? "today"}.`,
    ),
    "",
  ];
  const ins = flows.filter((x) => x.sign === 1);
  const outs = flows.filter((x) => x.sign === -1);
  L.push(tr("## Entrées", "## Income"), "", ...(ins.length ? ins.map((x) => `- ${x.text}`) : [tr("- Aucune source de revenu branchée.", "- No income source connected.")]), "");
  if (outs.length) L.push(tr("## Autres sorties", "## Other spending"), "", ...outs.map((x) => `- ${x.text}`), "");
  L.push(tr("## Abonnements actifs", "## Active subscriptions"), "");
  for (const c of CATEGORIES()) {
    const list = SUBSCRIPTIONS.filter((s) => s.category === c.id && (s.status === "active" || s.status === "failing"));
    if (!list.length) continue;
    L.push(
      `### ${c.label}`,
      "",
      ...list.map(
        (s) =>
          `- ${s.name}${s.vendor ? ` (${s.vendor})` : ""}${colon()}${money(s.amount, s.currency)}${per(s.period)}${s.next_renewal ? tr(`, prochain ${d(s.next_renewal)}`, `, next ${d(s.next_renewal)}`) : ""}${
            s.status === "failing" ? tr(" — ⚠️ PAIEMENT EN ÉCHEC", " — ⚠️ PAYMENT FAILING") : ""
          }`,
      ),
      "",
    );
  }
  const { total } = subscriptionsPerMonth(rates);
  const railway = usesRailway()
    ? tr(` Railway, période en cours : ${val(bill) ? money(val(bill)!.currentUsage, "USD") : why(bill)}.`, ` Railway, current period: ${val(bill) ? money(val(bill)!.currentUsage, "USD") : why(bill)}.`)
    : "";
  L.push(tr(`Total ≈ ${Math.round(total)} ${cur}/mois.`, `Total ≈ ${Math.round(total)} ${cur}/month.`) + railway, "");
  const stopped = SUBSCRIPTIONS.filter((s) => s.status === "cancelled" || s.status === "unknown");
  if (stopped.length) L.push(tr("## Arrêtés ou à vérifier", "## Cancelled or to check"), "", ...stopped.map((s) => `- ${s.name}${colon()}${s.evidence}`), "");
  return L.join("\n");
}

export async function watchDoc() {
  const terms = watchTerms();
  const repos = PROJECTS.flatMap((p) => (p.repo ? [p.repo] : []));
  const [men, ghm, nt, stars, prof, news, hn, mk, rates, m, servers, brew] = await Promise.all([
    terms.length ? source(hnMentions) : skip<Awaited<ReturnType<typeof hnMentions>>>(),
    terms.length ? source(() => githubMentions(terms)) : skip<Awaited<ReturnType<typeof githubMentions>>>(),
    source(notifications),
    repos.length ? source(() => recentStars(repos)) : skip<Awaited<ReturnType<typeof recentStars>>>(),
    source(profile),
    config().news.length ? source(localNews) : skip<Awaited<ReturnType<typeof localNews>>>(),
    source(hackerNews),
    source(crypto),
    fx(),
    source(machine),
    source(devServers),
    source(brewOutdated),
  ]);
  const L = [
    tr("# Veille", "# Watch"),
    "",
    tr(`> Généré par zenith le ${dt(Date.now())}. Mentions, GitHub, actualité, marchés et état du Mac.`, `> Generated by zenith on ${dt(Date.now())}. Mentions, GitHub, news, markets and this Mac.`),
    "",
  ];
  L.push(tr("## On parle de toi", "## Mentions"), "");
  const all = [
    ...(val(men) ?? []).map((x) => `- ${d(x.at)} · Hacker News · ${x.label}${colon()}${q(x.title)} — ${x.excerpt.slice(0, 160)} ${x.url}`),
    ...(val(ghm) ?? []).map((x) => `- ${d(x.at)} · GitHub ${x.repo} (${x.kind === "pr" ? "PR" : "issue"})${colon()}${q(x.title)} ${x.url}`),
  ];
  const none = terms.length
    ? tr(`- Aucune mention (${terms.join(", ")}) sur Hacker News ni dans les issues des autres. ${why(men)}`, `- No mention (${terms.join(", ")}) on Hacker News or in other people's issues. ${why(men)}`)
    : tr("- Aucun terme à surveiller : ajoute-en dans zenith.config.json (`watch`).", "- Nothing to watch: add terms to zenith.config.json (`watch`).");
  L.push(...(all.length ? all : [none.trimEnd()]), "");
  const g = val(prof);
  L.push("## GitHub", "");
  if (g)
    L.push(
      tr(
        `- @${g.login} : ${g.contributions} contributions sur 12 mois, série de ${g.streak} jour(s), ${g.stars} étoiles sur ${g.repos} dépôts, ${g.followers} abonnés`,
        `- @${g.login}: ${g.contributions} contributions over 12 months, ${g.streak}-day streak, ${g.stars} stars across ${g.repos} repositories, ${g.followers} followers`,
      ),
    );
  L.push(
    ...groupNotifications(val(nt) ?? []).map(
      (x) => `- ${tr("Notification non lue", "Unread notification")}${colon()}${x.repo} ${q(x.title)} (${x.type}, ${x.reason}, ${ago(x.at)}${x.count > 1 ? tr(`, ${x.count} fois`, `, ${x.count} times`) : ""}) ${x.url}`,
    ),
  );
  L.push(...(val(stars) ?? []).map((x) => tr(`- Étoile de ${x.user} sur ${x.repo} (${ago(x.at)})`, `- Star from ${x.user} on ${x.repo} (${ago(x.at)})`)), "");
  const mc = val(m);
  if (mc) {
    L.push(
      tr("## Ce Mac", "## This Mac"),
      "",
      `- ${mc.model} · ${mc.chip} · macOS ${mc.macos} · ${mc.memoryGB} ${tr("Go", "GB")} (${mc.memoryFree == null ? "?" : pc(mc.memoryFree)} ${tr("libres", "free")})`,
      tr(`- Disque : ${Math.round(mc.disk.freeGB)} Go libres sur ${Math.round(mc.disk.totalGB)}`, `- Disk: ${Math.round(mc.disk.freeGB)} GB free of ${Math.round(mc.disk.totalGB)}`),
    );
    if (mc.battery) L.push(`- ${tr("Batterie", "Battery")}${colon()}${pc(mc.battery.percent)}${mc.battery.charging ? tr(" (en charge)", " (charging)") : ""}`);
    const sv = val(servers) ?? [];
    if (sv.length) L.push(`- ${tr("Serveurs de dev", "Dev servers")}${colon()}${sv.map((x) => `:${x.port} ${x.project ?? x.command}`).join(", ")}`);
    const b = val(brew);
    if (b?.length) L.push(`- ${tr("Homebrew à mettre à jour", "Homebrew updates")}${colon()}${b.map((x) => `${x.name} ${x.current}→${x.latest}`).join(", ")}`);
    L.push("");
  }
  const table = rates.toBase;
  const cur = rates.base;
  const fxLine = ["EUR", "USD"]
    .filter((c) => c !== cur && table[c])
    .map((c) => `1 ${c} = ${table[c].toFixed(4)} ${cur}`)
    .join(semi());
  L.push(
    tr("## Marchés", "## Markets"),
    "",
    ...(fxLine ? [`- ${fxLine}`] : []),
    ...(val(mk) ?? []).map((x) => `- ${x.name}${colon()}${money(x.usd, "USD")} (${x.change >= 0 ? "+" : ""}${pc((x.change * 100).toFixed(1))})`),
    "",
  );
  const headlines = val(news) ?? [];
  if (headlines.length) L.push(tr("## Actualité", "## News"), "", ...headlines.slice(0, 10).map((x) => `- ${x.title} (${x.source}) ${x.url}`), "");
  L.push("## Hacker News", "", ...(val(hn) ?? []).map((x) => tr(`- ${x.title} (${x.score} points) ${x.url}`, `- ${x.title} (${x.score} points) ${x.url}`)), "");
  return L.join("\n");
}

export async function directoryDoc() {
  const L = [tr("# Annuaire", "# Directory"), ""];
  if (OWNER.name || OWNER.emails.length || OWNER.socials.length)
    L.push(`## ${OWNER.name || tr("Moi", "Me")}`, "", ...OWNER.emails.map((e) => `- ${e.address}${e.role ? ` — ${e.role}` : ""}`), ...OWNER.socials.map((s) => `- ${s.network} ${s.handle} — ${s.url}`), "");
  for (const p of PROJECTS) {
    const id = identityOf(p.id);
    const P: string[] = id.names.map((f) => `- ${f.label}${colon()}${f.value}`);
    for (const dom of id.domains) {
      const info = await domainInfo(dom).catch(() => null);
      P.push(
        `- ${tr("Domaine", "Domain")} ${dom}${
          info
            ? tr(
                ` : ${info.registrar ?? "?"}, renouvellement ${info.expires ? d(info.expires) : "?"}, e-mail ${info.mail?.provider || "aucun"}`,
                `: ${info.registrar ?? "?"}, renews ${info.expires ? d(info.expires) : "?"}, email ${info.mail?.provider || "none"}`,
              )
            : ""
        }`,
      );
    }
    P.push(
      ...id.emails.map((e) => `- ${tr("E-mail", "Email")} ${e.address}${e.role ? ` (${e.role})` : ""}`),
      ...id.socials.map((s) => `- ${s.network} ${s.handle}`),
      ...id.services.map((s) => `- ${s.label}${s.value ? `${colon()}${s.value}` : ""}`),
    );
    if (P.length) L.push(`## ${p.name}`, "", ...P, "");
  }
  return L.join("\n");
}

/** Document names, without .md. The same in every language. */
export const DOCS: string[] = ["brief", "vie", "argent", "annuaire", "veille", ...PROJECTS.map((p) => `projets/${p.id}`)];

/** What each document holds, for indexes (README.md, llms.txt, MCP). */
export function describeDoc(name: string) {
  const p = PROJECTS.find((x) => `projets/${x.id}` === name);
  if (p) return tr(`${p.name} : état, chiffres, à faire, code, déploiements, sessions d'agents, notes`, `${p.name}: status, numbers, to-dos, code, deployments, agent sessions, notes`);
  const map: Record<string, [string, string]> = {
    brief: ["Tout en une page", "Everything on one page"],
    vie: [
      "Agenda (Google + Apple), à faire (Obsidian + Rappels), e-mails, ventes, administratif, dépenses, rythme, air et eaux, fériés, anniversaires, transports, temps d'écran, musique, carnet",
      "Life: calendar (Google + Apple), to-dos (Obsidian + Reminders), email, sales, paperwork, spending, rhythm, air and water, holidays, birthdays, transit, screen time, music, notebook",
    ],
    argent: ["Entrées, abonnements, paiements en échec", "Money: income, subscriptions, failing payments"],
    annuaire: ["Noms, domaines, e-mails, réseaux, services", "Directory: names, domains, emails, socials, services"],
    veille: [
      "Mentions de tes projets (Hacker News, GitHub), notifications et étoiles GitHub, actualité, marchés, état du Mac",
      "Watch: mentions of your projects (Hacker News, GitHub), GitHub notifications and stars, news, markets, this Mac",
    ],
  };
  return map[name] ? tr(...map[name]) : name;
}

export async function doc(name: string): Promise<string | null> {
  if (name === "brief") return brief();
  if (name === "vie") return lifeDoc();
  if (name === "argent") return moneyDoc();
  if (name === "annuaire") return directoryDoc();
  if (name === "veille") return watchDoc();
  const p = PROJECTS.find((x) => `projets/${x.id}` === name);
  return p ? projectDoc(p) : null;
}

function index() {
  return [
    tr("# zenith — contexte pour agents IA", "# zenith — context for AI agents"),
    "",
    tr(
      `Mis à jour le ${dt(Date.now())}. Commence par brief.md : il résume projets, urgences, agenda, argent et agents en une page.`,
      `Updated on ${dt(Date.now())}. Start with brief.md: it sums up projects, urgent items, calendar, money and agents on one page.`,
    ),
    "",
    tr("| Fichier | Contenu |", "| File | Contents |"),
    "| --- | --- |",
    ...DOCS.map((name) => `| ${name}.md | ${describeDoc(name)} |`),
    "",
    tr(
      "En direct : http://127.0.0.1:4747/api/context/<fichier sans .md> ou le serveur MCP « zenith ».",
      'Live: http://127.0.0.1:4747/api/context/<file without .md> or the "zenith" MCP server.',
    ),
  ].join("\n");
}

/** Writes every document to context/ and to zenith's folder in the Obsidian vault. */
export async function writeContext() {
  const files: [string, string][] = [["README.md", index()]];
  for (const name of DOCS) {
    const body = await doc(name).catch((e) => `# ${name}\n\n${tr("Inaccessible", "Unavailable")}${colon()}${e instanceof Error ? e.message : e}`);
    if (body) files.push([`${name}.md`, body]);
  }
  const targets = [path.join(process.cwd(), "context")];
  if (process.env.OBSIDIAN_EXPORT !== "0") {
    const vault = await vaultPath().catch(() => null);
    if (vault) targets.push(path.join(vault, EXPORT_DIR));
  }
  for (const dir of targets) {
    await rm(path.join(dir, "projets"), { recursive: true, force: true }).catch(() => {});
    for (const [rel, body] of files) {
      const out = path.join(dir, rel);
      await mkdir(path.dirname(out), { recursive: true });
      const front = dir.endsWith(EXPORT_DIR)
        ? tr(`---\ngenere_par: zenith\nmis_a_jour: ${new Date().toISOString()}\n---\n\n`, `---\ngenerated_by: zenith\nupdated: ${new Date().toISOString()}\n---\n\n`)
        : "";
      await writeFile(out, front + body);
    }
  }
  return { files: files.map(([f]) => f), targets, at: new Date().toISOString(), entries: (await readdir(targets[0])).length };
}
