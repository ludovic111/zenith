import type { Metadata } from "next";
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import Link from "next/link";
import { ExternalLink } from "lucide-react";
import { config } from "@/lib/config";
import { PROJECTS } from "@/lib/projects";
import { collect, type SourceGroup, type SourceRow } from "@/lib/extensions";
import { tr } from "@/lib/i18n";
import { dateTime } from "@/lib/format";
import { source, type Source } from "@/lib/source";
import { profile, repo } from "@/lib/sources/github";
import { billing, deployments } from "@/lib/sources/railway";
import { overview } from "@/lib/sources/revenuecat";
import { allLocalRepos } from "@/lib/sources/git";
import { sessions } from "@/lib/sources/agents";
import { notes } from "@/lib/sources/obsidian";
import { apple } from "@/lib/sources/apple";
import { life } from "@/lib/sources/life";
import { air, holidays, water } from "@/lib/sources/environment";
import { departures, transitStop } from "@/lib/sources/transit";
import { hnMentions, localNews } from "@/lib/sources/news";
import { appReviews } from "@/lib/sources/domains";
import { crypto } from "@/lib/sources/markets";
import { openRouter } from "@/lib/sources/openrouter";
import { machine } from "@/lib/sources/machine";
import { Copy } from "@/components/identity/copy";
import { Panel } from "@/components/z/panel";
import { Status } from "@/components/z/status";
import { KeyForm } from "@/components/settings/key-form";

export const dynamic = "force-dynamic";

export function generateMetadata(): Metadata {
  return { title: tr("Sources de données", "Data sources") };
}

/** A settings row; `unused` means nothing in zenith.config.json asks for this source. */
type Row = SourceRow & { unused?: boolean };

const GROUPS: [SourceGroup, () => string][] = [
  ["projects", () => tr("Projets", "Projects")],
  ["around", () => tr("Autour de toi", "Around you")],
  ["app", () => "zenith.app"],
  ["claude", () => tr("Relevés par Claude", "Captured by Claude")],
];

const tilde = (p: string) => (p.startsWith(os.homedir()) ? `~${p.slice(os.homedir().length)}` : p);
const UNUSED: Source<unknown> = { ok: false };
const skip = () => Promise.resolve(UNUSED);

export default async function Settings() {
  const c = config();
  const loc = c.location;
  // Test targets come from the config: the first project that uses each service.
  const withRepo = PROJECTS.find((p) => p.repo);
  const withRailway = PROJECTS.find((p) => p.railway);
  const withRc = PROJECTS.find((p) => p.revenuecat);
  const withStore = PROJECTS.find((p) => p.appStore);
  const transitOn = Boolean(c.transit || process.env.TRANSIT_STOP);
  const waterOn = Boolean(c.water?.stations.length);

  const [gh, rw, rc, git, ag, ob, ap, lf, aq, wt, trn, hol, news, hn, rv, mk, or, mac, ext] = await Promise.all([
    source<unknown>(() => (withRepo ? repo(withRepo.repo!) : profile())),
    withRailway ? source(() => deployments(withRailway)) : process.env.RAILWAY_TOKEN ? source(billing) : skip(),
    withRc ? source(() => overview(withRc.revenuecat!.projectId)) : skip(),
    source(async () => {
      const r = await allLocalRepos();
      if (!r.length) throw new Error(tr("Aucun dépôt trouvé : renseigne « dir » pour tes projets dans zenith.config.json", 'No repository found: set "dir" for your projects in zenith.config.json'));
      return r;
    }),
    source(async () => {
      const s = await sessions();
      if (!s.length) throw new Error(tr("Aucune session trouvée dans ~/.claude ni ~/.codex", "No session found in ~/.claude or ~/.codex"));
      return s;
    }),
    source(notes),
    source(apple),
    source(life),
    loc ? source(air) : skip(),
    waterOn ? source(water) : skip(),
    transitOn ? source(() => departures()) : skip(),
    loc?.country ? source(holidays) : skip(),
    c.news.length ? source(localNews) : skip(),
    c.watch.length ? source(hnMentions) : skip(),
    withStore ? source(() => appReviews(withStore.appStore!.id, withStore.appStore!.countries)) : skip(),
    source(crypto),
    source(openRouter),
    source(machine),
    collect((e) => e.sources),
  ]);
  const a = ap.ok ? ap.data : null;
  const need = (why: string): Source<unknown> => ({ ok: false, missing: [why] });
  const openApp = tr("ouvrir zenith.app", "open zenith.app");
  const privacy = (fr: string, en: string) => tr(`Accès refusé : Réglages Système → Confidentialité → ${fr} → zenith`, `Access denied: System Settings → Privacy → ${en} → zenith`);
  const calendarSrc: Source<unknown> = !a ? need(openApp) : a.calendar.authorized ? { ok: true, data: a } : { ok: false, error: privacy("Calendriers", "Calendars") };
  const remindersSrc: Source<unknown> = !a ? need(openApp) : a.reminders.authorized ? { ok: true, data: a } : { ok: false, error: privacy("Rappels", "Reminders") };
  const fromApp = (ok: boolean | undefined, denied: string): Source<unknown> =>
    !a ? need(openApp) : ok == null ? need(tr("réinstaller zenith.app (npm run mac:install)", "reinstall zenith.app (npm run mac:install)")) : ok ? { ok: true, data: a } : { ok: false, error: denied };
  const contactsSrc = fromApp(a?.birthdays?.authorized, privacy("Contacts", "Contacts"));
  const musicSrc = fromApp(
    a?.music ? !a.music.denied.length : undefined,
    tr(
      `Accès refusé à ${a?.music?.denied.join(" et ")} : Réglages Système → Confidentialité → Automatisation → zenith`,
      `Access to ${a?.music?.denied.join(" and ")} denied: System Settings → Privacy → Automation → zenith`,
    ),
  );
  const screenSrc = fromApp(a?.screen?.tracking, "");
  const mailSrc: Source<unknown> = !a
    ? need(openApp)
    : !a.mail.running
      ? need(tr("ouvrir Mail", "open Mail"))
      : a.mail.authorized && !a.mail.error
        ? { ok: true, data: a }
        : { ok: false, error: a.mail.error ?? tr("Accès refusé : Réglages Système → Confidentialité → Automatisation → zenith → Mail", "Access denied: System Settings → Privacy → Automation → zenith → Mail") };
  const lifeSrc: Source<unknown> = lf.ok && lf.data ? lf : need(tr("relevé Claude", "Claude snapshot"));
  const node = process.execPath;
  const mcp = `${process.cwd()}/scripts/mcp/zenith-mcp.mjs`;
  const inConfig = (field: string) => tr(`${field} dans zenith.config.json`, `${field} in zenith.config.json`);
  const configNeeded = (field: string): Source<unknown> => need(inConfig(field));

  const rows: Row[] = [
    {
      group: "projects",
      name: tr("Dépôts locaux", "Local repositories"),
      feeds: tr("Commits, branches, travail non commité, versions", "Commits, branches, uncommitted work, versions"),
      vars: tr("projectsRoot et « dir » de chaque projet (config), PROJECTS_ROOT (facultatif)", 'projectsRoot and each project\'s "dir" (config), PROJECTS_ROOT (optional)'),
      how: tr(`Lit les dépôts de ${tilde(c.projectsRoot)}. Rien d'autre à faire.`, `Reads the repositories in ${tilde(c.projectsRoot)}. Nothing else to do.`),
      src: git,
    },
    {
      group: "projects",
      name: "Claude Code & Codex",
      feeds: tr("Sessions d'agents, coûts, lignes écrites, PR ouvertes", "Agent sessions, costs, lines written, opened PRs"),
      vars: tr("CLAUDE_HOME, CODEX_HOME (facultatifs)", "CLAUDE_HOME, CODEX_HOME (optional)"),
      how: (
        <>
          {tr("Lit", "Reads")} <code className="font-mono">~/.claude/projects</code> {tr("et", "and")} <code className="font-mono">~/.codex/sessions</code>{" "}
          {tr("en local. Rien à faire.", "locally. Nothing to do.")}
        </>
      ),
      src: ag,
    },
    {
      group: "projects",
      name: "GitHub",
      feeds: tr(
        "CI, releases & téléchargements, vues, issues, notifications, contributions, nouvelles étoiles, mentions dans les issues des autres",
        "CI, releases & downloads, views, issues, notifications, contributions, new stars, mentions in other people's issues",
      ),
      vars: tr("GITHUB_TOKEN (facultatif)", "GITHUB_TOKEN (optional)"),
      how: (
        <>
          {tr("Utilise automatiquement", "Uses")} <code className="font-mono">gh auth token</code>
          {tr(". Sinon, un jeton fin en lecture seule.", " automatically. Otherwise, a read-only fine-grained token.")}
        </>
      ),
      url: "https://github.com/settings/personal-access-tokens/new",
      src: gh,
      key: "GITHUB_TOKEN",
      placeholder: "github_pat_…",
    },
    {
      group: "projects",
      name: "Railway",
      feeds: tr("Déploiements, trafic HTTP, facture du mois", "Deployments, HTTP traffic, this month's bill"),
      vars: `RAILWAY_TOKEN · ${inConfig("projects[].railway")}`,
      how: tr("Account Settings → Tokens → créer un jeton de compte (pas un jeton de projet).", "Account Settings → Tokens → create an account token (not a project token)."),
      url: "https://railway.com/account/tokens",
      src: rw,
      unused: !withRailway && !process.env.RAILWAY_TOKEN,
      key: "RAILWAY_TOKEN",
    },
    {
      group: "projects",
      name: `RevenueCat${withRc ? ` · ${withRc.name}` : ""}`,
      feeds: tr("MRR, revenu, abonnements, essais", "MRR, revenue, subscriptions, trials"),
      vars: `REVENUECAT_API_KEY · ${inConfig("projects[].revenuecat")}`,
      how: tr(
        "Project settings → API keys → nouvelle clé secrète v2, permissions en lecture seule (Charts & metrics, Customer information).",
        "Project settings → API keys → new secret v2 key, read-only permissions (Charts & metrics, Customer information).",
      ),
      url: withRc ? `https://app.revenuecat.com/projects/${withRc.revenuecat!.projectId}/api-keys` : "https://app.revenuecat.com",
      src: rc,
      unused: !withRc,
      key: "REVENUECAT_API_KEY",
      placeholder: "sk_…",
    },
    {
      group: "projects",
      name: tr(`App Store · avis${withStore ? ` · ${withStore.name}` : ""}`, `App Store · reviews${withStore ? ` · ${withStore.name}` : ""}`),
      feeds: tr("Avis écrits, note et version en ligne", "Written reviews, rating and live version"),
      vars: inConfig("projects[].appStore"),
      how: tr("Flux RSS public d'Apple, sans clé.", "Apple's public RSS feed, no key."),
      src: rv,
      unused: !withStore,
    },
    {
      group: "projects",
      name: "OpenRouter",
      feeds: tr("Solde et dépense de crédits : jour, semaine, mois", "Credit balance and spending: day, week, month"),
      vars: "OPENROUTER_API_KEY",
      how: tr(
        "Settings → Keys → créer une clé (une clé sans limite de crédit suffit, zenith ne fait que lire le solde).",
        "Settings → Keys → create a key (one without a credit limit is enough, zenith only reads the balance).",
      ),
      url: "https://openrouter.ai/settings/keys",
      src: or,
      key: "OPENROUTER_API_KEY",
      placeholder: "sk-or-v1-…",
    },
    {
      group: "projects",
      name: tr("Hacker News · mentions", "Hacker News · mentions"),
      feeds: c.watch.length
        ? tr(`Qui parle de ${c.watch.map((w) => w.label).filter((v, i, l) => l.indexOf(v) === i).join(", ")}, sur 90 jours ; les 10 premiers articles`, `Who mentions ${c.watch.map((w) => w.label).filter((v, i, l) => l.indexOf(v) === i).join(", ")}, over 90 days; the top 10 stories`)
        : tr("Qui parle de tes projets, sur 90 jours ; les 10 premiers articles", "Who mentions your projects, over 90 days; the top 10 stories"),
      vars: inConfig("watch"),
      how: tr("Recherche Algolia et API officielle de Hacker News, sans clé.", "Algolia search and the official Hacker News API, no key."),
      src: c.watch.length ? hn : configNeeded("watch"),
      unused: !c.watch.length,
    },
    {
      group: "around",
      name: "Obsidian",
      feeds: tr(
        `Todo, notes récentes, notes liées à chaque projet ; zenith écrit son résumé dans le dossier « ${c.obsidian.exportDir} » du vault`,
        `To-do, recent notes, notes linked to each project; zenith writes its summary to the "${c.obsidian.exportDir}" folder of the vault`,
      ),
      vars: tr("obsidian.vault (config) ou OBSIDIAN_VAULT ; OBSIDIAN_EXPORT=0 pour ne rien écrire", "obsidian.vault (config) or OBSIDIAN_VAULT; OBSIDIAN_EXPORT=0 to write nothing"),
      how: ob.ok
        ? tr(`Vault « ${ob.data.vault} », ${ob.data.notes.length} notes, lu en direct.`, `Vault "${ob.data.vault}", ${ob.data.notes.length} notes, read live.`)
        : tr("Aucun vault trouvé : ouvre-en un dans Obsidian ou renseigne obsidian.vault.", "No vault found: open one in Obsidian or set obsidian.vault."),
      src: ob,
    },
    {
      group: "around",
      name: tr("Météo, air, UV & pollens", "Weather, air, UV & pollen"),
      feeds: tr(
        "Prévisions, indice européen de qualité de l'air, particules, ozone, UV, pollens",
        "Forecast, European air quality index, particles, ozone, UV, pollen",
      ),
      vars: inConfig("location"),
      how: loc ? tr(`Open-Meteo (modèle CAMS), pour ${loc.name}, sans clé.`, `Open-Meteo (CAMS model), for ${loc.name}, no key.`) : tr("Open-Meteo, sans clé : il suffit d'une position.", "Open-Meteo, no key: it only needs a location."),
      src: loc ? aq : configNeeded("location"),
    },
    {
      group: "around",
      name: tr("Jours fériés", "Public holidays"),
      feeds: tr("Fériés de ton pays et de ta région, aussi dans l'agenda", "Holidays of your country and region, also in the calendar"),
      vars: inConfig("location.country, location.region"),
      how: tr("Nager.Date, sans clé.", "Nager.Date, no key."),
      src: loc?.country ? hol : configNeeded("location.country"),
    },
    ...(transitOn
      ? [
          {
            group: "around" as const,
            name: tr("Transports publics", "Public transport"),
            feeds: tr("Prochains départs depuis ton arrêt (Suisse)", "Next departures from your stop (Switzerland)"),
            vars: tr("transit.stop (config) ou TRANSIT_STOP", "transit.stop (config) or TRANSIT_STOP"),
            how: tr(`Arrêt suivi : « ${transitStop() ?? "—"} ».`, `Stop: "${transitStop() ?? "—"}".`),
            url: "https://transport.opendata.ch",
            src: trn,
          },
        ]
      : []),
    ...(waterOn
      ? [
          {
            group: "around" as const,
            name: tr("Rivières et lacs", "Rivers and lakes"),
            feeds: c.water!.stations.map((s) => s.water).filter((v, i, l) => l.indexOf(v) === i).join(", "),
            vars: inConfig("water.stations"),
            how: tr("Stations hydrologiques de l'OFEV (via existenz.ch), sans clé.", "FOEN hydrology stations (via existenz.ch), no key."),
            src: wt,
          },
        ]
      : []),
    {
      group: "around",
      name: tr("Actualité", "News"),
      feeds: c.news.length ? c.news.map((n) => n.name).join(", ") : tr("Tes flux RSS", "Your RSS feeds"),
      vars: inConfig("news"),
      how: tr("Flux RSS, sans clé.", "RSS feeds, no key."),
      src: c.news.length ? news : configNeeded("news"),
      unused: !c.news.length,
    },
    {
      group: "around",
      name: tr("Marchés", "Markets"),
      feeds: tr(`Solana, Bitcoin, Ether ; taux de change en ${c.currency}`, `Solana, Bitcoin, Ether; exchange rates in ${c.currency}`),
      vars: tr("aucune", "none"),
      how: tr("Ticker public Kraken et taux BCE (Frankfurter), sans clé.", "Kraken public ticker and ECB rates (Frankfurter), no key."),
      src: mk,
    },
    {
      group: "around",
      name: tr("Ce Mac", "This Mac"),
      feeds: tr(
        "Disque, mémoire, charge, batterie, serveurs de dev en marche, paquets Homebrew à mettre à jour",
        "Disk, memory, load, battery, running dev servers, outdated Homebrew packages",
      ),
      vars: tr("aucune", "none"),
      how: tr("Lu localement (sysctl, memory_pressure, pmset, lsof, brew). Rien à faire.", "Read locally (sysctl, memory_pressure, pmset, lsof, brew). Nothing to do."),
      src: mac,
    },
    {
      group: "app",
      name: tr("Calendrier Apple", "Apple Calendar"),
      feeds: tr("Rendez-vous des 14 prochains jours, tous calendriers", "Events of the next 14 days, all calendars"),
      vars: "zenith.app",
      how: tr(
        "zenith.app le lit via EventKit, toutes les 5 minutes, en lecture seule. macOS demande l'autorisation au premier lancement.",
        "zenith.app reads it through EventKit every 5 minutes, read-only. macOS asks for permission on first launch.",
      ),
      src: calendarSrc,
    },
    {
      group: "app",
      name: tr("Rappels Apple", "Apple Reminders"),
      feeds: tr("Rappels non terminés, dans « À faire »", 'Open reminders, in "To do"'),
      vars: "zenith.app",
      how: tr("Même principe que le calendrier.", "Same as the calendar."),
      src: remindersSrc,
    },
    {
      group: "app",
      name: tr("Mail Apple", "Apple Mail"),
      feeds: tr("Non lus par compte et derniers mails non lus", "Unread count per account and latest unread messages"),
      vars: tr("zenith.app + Mail ouvert", "zenith.app + Mail running"),
      how: tr("Lu par AppleScript seulement quand Mail est déjà ouvert : zenith ne l'ouvre jamais à ta place.", "Read with AppleScript only when Mail is already open: zenith never opens it for you."),
      src: mailSrc,
    },
    {
      group: "app",
      name: "Contacts",
      feeds: tr("Anniversaires des 30 prochains jours, aussi dans l'agenda", "Birthdays in the next 30 days, also in the calendar"),
      vars: "zenith.app",
      how: tr("Seulement le nom et la date d'anniversaire, lus avec Contacts ; ni numéro ni adresse.", "Only the name and birthday, read with Contacts; no phone number or address."),
      src: contactsSrc,
    },
    {
      group: "app",
      name: tr("Musique & Spotify", "Music & Spotify"),
      feeds: tr("Morceau en cours et dernières écoutes", "Now playing and recently played"),
      vars: "zenith.app",
      how: tr("Demandé chaque minute par AppleScript, seulement si Musique ou Spotify est déjà ouvert.", "Asked every minute with AppleScript, only if Music or Spotify is already open."),
      src: musicSrc,
    },
    {
      group: "app",
      name: tr("Temps d'écran", "Screen time"),
      feeds: tr("Temps actif devant le Mac, par app, sur 14 jours", "Active time on the Mac, per app, over 14 days"),
      vars: "zenith.app",
      how: tr(
        "zenith.app note l'app au premier plan toutes les 20 s, sauf écran verrouillé ou 3 min sans clavier ni souris. Aucune autorisation nécessaire.",
        "zenith.app notes the frontmost app every 20 s, except when the screen is locked or after 3 min without keyboard or mouse. No permission needed.",
      ),
      src: screenSrc,
    },
    {
      group: "claude",
      name: tr("Google Agenda & Gmail", "Google Calendar & Gmail"),
      feeds: tr("Agenda, mails qui attendent une réponse, colis, ventes, dépenses, administratif", "Calendar, emails waiting for a reply, parcels, sales, spending, paperwork"),
      vars: ".data/life.json",
      how:
        lf.ok && lf.data
          ? tr(`Relevé par Claude le ${dateTime(lf.data.capturedAt)} — demande « mets à jour ma vie dans zenith ».`, `Captured by Claude on ${dateTime(lf.data.capturedAt)} — ask "update my life in zenith".`)
          : tr("Demande à Claude « mets à jour ma vie dans zenith » (voir docs/releves.md).", 'Ask Claude "update my life in zenith" (see docs/releves.md).'),
      src: lifeSrc,
    },
    ...ext,
  ];

  const counted = rows.filter((r) => !r.unused);
  const codeHere = existsSync(path.join(process.cwd(), "src", "lib", "code"));

  return (
    <>
      <header className="mb-8">
        <h1 className="font-display text-4xl font-black tracking-tight sm:text-5xl">{tr("Sources de données", "Data sources")}</h1>
        <p className="mt-2 max-w-2xl font-serif text-xl italic text-ink-2">
          {tr(`${counted.filter((r) => r.src.ok).length} sources branchées sur ${counted.length}.`, `${counted.filter((r) => r.src.ok).length} of ${counted.length} sources connected.`)}{" "}
          {tr("Colle une clé ci-dessous : le serveur local l'écrit dans", "Paste a key below: the local server writes it to")}{" "}
          <code className="font-mono text-base not-italic text-sun">.env.local</code>{" "}
          {tr(
            "(ignoré par git) et la source s'allume tout de suite. Rien n'est envoyé ailleurs qu'à la source concernée.",
            "(ignored by git) and the source lights up right away. Nothing is sent anywhere but to that source.",
          )}
        </p>
      </header>

      <ConfigPanel codeHere={codeHere} />

      {GROUPS.map(([g, label]) => {
        const list = rows.filter((r) => r.group === g);
        if (!list.length) return null;
        const used = list.filter((r) => !r.unused);
        return (
          <section key={g} className="mb-8">
            <h2 className="mb-3 flex items-baseline gap-3 font-display text-lg">
              {label()}
              <span className="font-mono text-xs text-ink-3">
                {used.filter((r) => r.src.ok).length}/{used.length} {tr("branchées", "connected")}
              </span>
            </h2>
            <div className="grid gap-4">
              {list.map((r) => (
                <Panel key={r.name} accent={r.unused ? undefined : r.src.ok ? "var(--good)" : r.src.missing ? "var(--sun)" : "var(--bad)"} className={r.unused ? "opacity-60" : undefined}>
                  <div className="grid gap-4 md:grid-cols-[220px_1fr_auto] md:items-start">
                    <div>
                      <div className="font-display text-base">{r.name}</div>
                      <div className="mt-1">
                        {r.unused ? (
                          <Status health="unknown" label={tr("Non utilisée", "Not used")} />
                        ) : (
                          <Status health={r.src.ok ? "up" : r.src.missing ? "unknown" : "down"} label={r.src.ok ? tr("Branchée", "Connected") : r.src.missing ? tr("À brancher", "To connect") : tr("Erreur", "Error")} />
                        )}
                      </div>
                    </div>
                    <div className="space-y-1.5 text-sm">
                      <div className="text-ink-2">{r.feeds}</div>
                      <div className="text-ink-3">{r.how}</div>
                      <code className="block font-mono text-xs text-sun">{r.vars}</code>
                      {!r.unused && !r.src.ok && r.src.missing && <div className="break-words text-xs text-ink-3">{tr("Manque", "Missing")}
                          {tr(" : ", ": ")}
                          {r.src.missing.map((k) => (/^[a-z]/.test(k) && !k.includes(" ") ? inConfig(k) : k)).join(", ")}</div>}
                      {!r.unused && !r.src.ok && r.src.error && <div className="break-words text-xs text-bad">{r.src.error}</div>}
                      {r.key && <KeyForm name={r.key} secret={r.secret} placeholder={r.src.ok && r.secret !== false ? tr("Remplacer la clé", "Replace the key") : r.placeholder} />}
                    </div>
                    {r.url && (
                      <a href={r.url} target="_blank" rel="noopener noreferrer" className="inline-flex items-center gap-1.5 rounded-full border border-line px-3 py-1.5 text-xs text-ink-2 hover:text-ink">
                        {tr("Obtenir", "Get it")} <ExternalLink className="size-3" />
                      </a>
                    )}
                  </div>
                </Panel>
              ))}
            </div>
          </section>
        );
      })}

      <Panel className="mt-6" kicker={tr("Agents IA", "AI agents")} title={tr("Brancher n'importe quel agent sur zenith", "Connect any agent to zenith")} accent="#D4724F">
        <p className="mb-5 text-sm text-ink-2">
          {tr(
            "zenith résume ta vie et tes projets en Markdown, mis à jour toutes les 10 minutes. Un agent qui le lit sait instantanément où tu en es.",
            "zenith sums up your life and projects in Markdown, refreshed every 10 minutes. An agent that reads it knows at once where you stand.",
          )}
        </p>
        <div className="space-y-5 text-sm">
          <Snippet title={tr("Claude Code (toutes tes sessions)", "Claude Code (all your sessions)")} code={`claude mcp add zenith --scope user -- ${node} ${mcp}`} />
          <Snippet title="Codex" code={`codex mcp add zenith -- ${node} ${mcp}`} />
          <Snippet
            title={tr("Claude Desktop, Cursor… (fichier de config MCP)", "Claude Desktop, Cursor… (MCP config file)")}
            code={JSON.stringify({ mcpServers: { zenith: { command: node, args: [mcp] } } }, null, 2)}
          />
          <Snippet title={tr("Fichiers Markdown (n'importe quel outil)", "Markdown files (any tool)")} code={`${process.cwd()}/context/brief.md`} />
          <Snippet title={tr("En HTTP, depuis ce Mac", "Over HTTP, from this Mac")} code={"http://127.0.0.1:4747/api/context   ·   http://127.0.0.1:4747/llms.txt"} />
        </div>
        <p className="mt-5 text-xs text-ink-3">
          {tr(
            "Outils MCP : zenith_brief (à appeler en premier), zenith_project, zenith_document (vie, argent, annuaire, veille), zenith_search_notes et zenith_read_note (Obsidian). Tout est en lecture seule.",
            "MCP tools: zenith_brief (call it first), zenith_project, zenith_document (life, money, directory, watch), zenith_search_notes and zenith_read_note (Obsidian). Everything is read-only.",
          )}
        </p>
      </Panel>

      <Panel className="mt-6" kicker={tr("Exemple", "Example")} title=".env.local">
        <pre className="overflow-x-auto rounded-2xl bg-black/40 p-4 font-mono text-xs leading-relaxed text-ink-2">
          {[
            "RAILWAY_TOKEN=",
            "REVENUECAT_API_KEY=",
            "OPENROUTER_API_KEY=",
            tr("# GITHUB_TOKEN=          # sinon : gh auth token", "# GITHUB_TOKEN=          # otherwise: gh auth token"),
            tr("# OBSIDIAN_VAULT=        # sinon : obsidian.vault ou le vault ouvert", "# OBSIDIAN_VAULT=        # otherwise: obsidian.vault or the open vault"),
            tr("# ZENITH_CONFIG=         # sinon : zenith.config.json à côté de package.json", "# ZENITH_CONFIG=         # otherwise: zenith.config.json next to package.json"),
          ].join("\n")}
        </pre>
        <p className="mt-3 text-xs text-ink-3">
          {tr("Toutes les variables : ", "Every variable: ")}
          <code className="font-mono">.env.example</code>. {tr("Redémarrer zenith après une modification à la main.", "Restart zenith after editing it by hand.")}
        </p>
      </Panel>
    </>
  );
}

/** Where the config lives, whether it was read, and how to create it. */
function ConfigPanel({ codeHere }: { codeHere: boolean }) {
  const c = config();
  const { file, found, error } = c.meta;
  const accent = error ? "var(--bad)" : found ? "var(--good)" : "var(--sun)";
  const dir = path.dirname(file);
  const example = path.join(dir, "zenith.config.example.json");
  return (
    <Panel className="mb-8" kicker="Configuration" title="zenith.config.json" accent={accent}>
      <div className="grid gap-4 text-sm md:grid-cols-[220px_1fr]">
        <div>
          <Status
            health={error ? "down" : found ? "up" : "unknown"}
            label={error ? tr("Erreurs de validation", "Validation errors") : found ? tr("Lue", "Loaded") : tr("Introuvable", "Not found")}
          />
        </div>
        <div className="space-y-1.5">
          <Copy value={file} mono>
            {tilde(file)}
          </Copy>
          <div className="text-ink-3">
            {tr(
              `${c.projects.length} projet(s) · langue ${c.locale} · fuseau ${c.timezone} · devise ${c.currency}${c.location ? ` · ${c.location.name}` : ""}`,
              `${c.projects.length} project(s) · locale ${c.locale} · time zone ${c.timezone} · currency ${c.currency}${c.location ? ` · ${c.location.name}` : ""}`,
            )}
          </div>
          {error && (
            <div className="space-y-1">
              <div className="text-xs text-ink-3">{tr("zenith a démarré avec une configuration vide. À corriger :", "zenith started with an empty configuration. To fix:")}</div>
              <ul className="list-disc space-y-0.5 pl-5 font-mono text-xs text-bad">
                {error.split(" · ").map((e) => (
                  <li key={e} className="break-words">
                    {e}
                  </li>
                ))}
              </ul>
            </div>
          )}
        </div>
      </div>
      {(!found || error) && (
        <ol className="mt-5 list-decimal space-y-2 pl-5 text-sm text-ink-2">
          {!found && (
            <li>
              {tr("Copie l'exemple (ou mets-le dans ", "Copy the example (or put it in ")}
              <code className="font-mono text-xs">perso/zenith.config.json</code>
              {tr(", à côté de tes extensions privées ; les deux sont ignorés par git) :", ", next to your private extensions; git ignores both):")}
              <div className="mt-1.5 rounded-xl border border-line bg-black/40 px-3 py-2.5">
                <Copy value={`cp "${example}" "${file}"`} mono className="whitespace-pre-wrap break-all">
                  {`cp ${tilde(example)} ${tilde(file)}`}
                </Copy>
              </div>
            </li>
          )}
          <li>
            {tr("Remplis-le : toi (", "Fill it in: you (")}
            <code className="font-mono text-xs">owner</code>
            {tr("), ta ville (", "), your city (")}
            <code className="font-mono text-xs">location</code>
            {tr("), tes projets (", "), your projects (")}
            <code className="font-mono text-xs">projects</code>
            {tr("), tes abonnements… Chaque champ est décrit dans ", "), your subscriptions… Every field is described in ")}
            <code className="font-mono text-xs">docs/configuration.md</code>.
          </li>
          <li>
            {tr("Garde la ligne ", "Keep the line ")}
            <code className="font-mono text-xs">&quot;$schema&quot;: &quot;./zenith.schema.json&quot;</code>
            {tr(" : ton éditeur complète et vérifie chaque champ. Le schéma se régénère avec ", ": your editor completes and checks every field. Regenerate the schema with ")}
            <code className="font-mono text-xs">npm run config:schema</code>.
          </li>
          <li>
            {tr("Redémarre zenith (", "Restart zenith (")}
            <code className="font-mono text-xs">npm run dev</code>
            {tr(", ou ", ", or ")}
            <code className="font-mono text-xs">npm run mac:install</code>
            {tr(" pour l'app) : la configuration est lue au démarrage.", " for the app): the configuration is read at startup.")}
          </li>
        </ol>
      )}
      {found && !error && (
        <p className="mt-4 text-xs text-ink-3">
          {tr("La configuration est lue au démarrage : redémarre zenith après l'avoir modifiée. Référence : ", "The configuration is read at startup: restart zenith after editing it. Reference: ")}
          <code className="font-mono">docs/configuration.md</code>
          {tr(" · extensions privées : ", " · private extensions: ")}
          <code className="font-mono">docs/extensions.md</code>.
        </p>
      )}
      {codeHere && (
        <div className="mt-4 flex flex-wrap items-center gap-3 border-t border-line pt-4 text-sm">
          <Status health={c.code.enabled ? "up" : "unknown"} label={c.code.enabled ? tr("zenith code activé", "zenith code enabled") : tr("zenith code désactivé", "zenith code disabled")} />
          <span className="font-mono text-xs text-ink-3">code.enabled · code.port {c.code.port}</span>
          {c.code.enabled && (
            <Link href="/code" className="inline-flex items-center gap-1.5 rounded-full border border-line px-3 py-1.5 text-xs text-ink-2 hover:text-ink">
              {tr("Ouvrir zenith code", "Open zenith code")}
            </Link>
          )}
        </div>
      )}
    </Panel>
  );
}

function Snippet({ title, code }: { title: string; code: string }) {
  return (
    <div>
      <div className="mb-1.5 text-[11px] uppercase tracking-[0.18em] text-ink-3">{title}</div>
      <div className="rounded-xl border border-line bg-black/40 px-3 py-2.5">
        <Copy value={code} mono className="whitespace-pre-wrap break-all">
          {code}
        </Copy>
      </div>
    </div>
  );
}
