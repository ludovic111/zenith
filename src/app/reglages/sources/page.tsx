import type { Metadata } from "next";
import { ExternalLink, TriangleAlert } from "lucide-react";
import { config } from "@/lib/config";
import { PROJECTS } from "@/lib/projects";
import { collect, type SourceGroup, type SourceRow } from "@/lib/extensions";
import { plural, tr } from "@/lib/i18n";
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
import { PageHeader } from "@/components/z/panel";
import { Status } from "@/components/z/status";
import { KeyForm } from "@/components/settings/key-form";
import { Code, CodeBlock, Group, tilde } from "@/components/settings/rows";
import { cn } from "@/lib/utils";

export const dynamic = "force-dynamic";

export function generateMetadata(): Metadata {
  return { title: tr("Sources de données", "Data sources") };
}

/** A settings row; `unused` means nothing in zenith.config.json asks for this source. */
type Row = SourceRow & { unused?: boolean };

const GROUPS: [SourceGroup, () => string, () => string][] = [
  ["projects", () => tr("Projets", "Projects"), () => tr("Code, déploiements, revenus, avis", "Code, deployments, revenue, reviews")],
  ["around", () => tr("Autour de toi", "Around you"), () => tr("Notes, météo, actualité, ce Mac", "Notes, weather, news, this Mac")],
  ["app", () => "zenith.app", () => tr("Lu par l'app Mac, en lecture seule", "Read by the Mac app, read-only")],
  ["claude", () => tr("Relevés par Claude", "Captured by Claude"), () => tr("Ce qu'aucune API locale n'expose", "What no local API exposes")],
];

const UNUSED: Source<unknown> = { ok: false };
const skip = () => Promise.resolve(UNUSED);

/** Every data source, tested now: its status, what it feeds, how to connect it. */
async function sourceRows(): Promise<{ rows: Row[]; inConfig: (field: string) => string }> {
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
          {tr("Lit", "Reads")} <Code>~/.claude/projects</Code> {tr("et", "and")} <Code>~/.codex/sessions</Code>{" "}
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
          {tr("Utilise automatiquement", "Uses")} <Code>gh auth token</Code>
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
  return { rows, inConfig };
}

type State = "ok" | "missing" | "error" | "unused";
const stateOf = (r: Row): State => (r.unused ? "unused" : r.src.ok ? "ok" : r.src.missing ? "missing" : "error");
const slug = (name: string) =>
  "src-" +
  name
    .toLowerCase()
    .normalize("NFD")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "");

export default async function Sources() {
  const { rows, inConfig } = await sourceRows();
  const counted = rows.filter((r) => !r.unused);
  const ok = counted.filter((r) => r.src.ok).length;
  const failing = counted.filter((r) => stateOf(r) === "error");

  return (
    <div className="mx-auto max-w-4xl">
      <PageHeader
        title={tr("Sources de données", "Data sources")}
        description={
          <>
            {tr("Colle une clé : le serveur local l'écrit dans ", "Paste a key: the local server writes it to ")}
            <Code>.env.local</Code>
            {tr(
              " (ignoré par git) et la source s'allume tout de suite. Rien ne part ailleurs qu'à la source concernée.",
              " (ignored by git) and the source lights up at once. Nothing is sent anywhere but to that source.",
            )}
          </>
        }
        action={
          <div className="text-right">
            <div className="text-xl font-semibold tracking-tight text-ink tabular">
              {ok}
              <span className="text-ink-3">/{counted.length}</span>
            </div>
            <div className="text-xs text-ink-3">{tr("branchées", "connected")}</div>
          </div>
        }
      />

      <div className="mb-8 h-1 overflow-hidden rounded-full bg-muted" aria-hidden>
        <div className="h-full rounded-full bg-good" style={{ width: `${counted.length ? (ok / counted.length) * 100 : 0}%` }} />
      </div>

      {failing.length > 0 && (
        <div className="mb-8 rounded-xl border border-bad/25 bg-bad/5 px-4 py-3">
          <div className="flex items-center gap-2 text-[13px] font-medium text-ink">
            <TriangleAlert className="size-3.5 text-bad" />
            {failing.length} {plural(failing.length, ["source en erreur", "sources en erreur"], ["source failing", "sources failing"])}
          </div>
          <ul className="mt-1.5 space-y-1 text-xs">
            {failing.map((r) => (
              <li key={r.name} className="flex min-w-0 gap-2">
                <a href={`#${slug(r.name)}`} className="shrink-0 text-ink-2 underline-offset-4 hover:text-ink hover:underline">
                  {r.name}
                </a>
                <span className="truncate text-ink-3">{r.src.ok ? "" : r.src.error}</span>
              </li>
            ))}
          </ul>
        </div>
      )}

      {GROUPS.map(([g, label, hint]) => {
        const list = rows.filter((r) => r.group === g).sort((a, b) => Number(!!a.unused) - Number(!!b.unused));
        if (!list.length) return null;
        const used = list.filter((r) => !r.unused);
        return (
          <Group
            key={g}
            title={label()}
            description={hint()}
            action={
              <span className="tabular">
                {used.filter((r) => r.src.ok).length}/{used.length}
              </span>
            }
          >
            {list.map((r) => (
              <SourceLine key={r.name} r={r} inConfig={inConfig} />
            ))}
          </Group>
        );
      })}

      <Group title={tr("Exemple de .env.local", "Example .env.local")} description={tr("Toutes les variables sont dans .env.example. Redémarre zenith après une modification à la main.", "Every variable is in .env.example. Restart zenith after editing it by hand.")}>
        <div className="p-3">
          <CodeBlock
            code={[
              "RAILWAY_TOKEN=",
              "REVENUECAT_API_KEY=",
              "OPENROUTER_API_KEY=",
              tr("# GITHUB_TOKEN=          # sinon : gh auth token", "# GITHUB_TOKEN=          # otherwise: gh auth token"),
              tr("# OBSIDIAN_VAULT=        # sinon : obsidian.vault ou le vault ouvert", "# OBSIDIAN_VAULT=        # otherwise: obsidian.vault or the open vault"),
              tr("# ZENITH_CONFIG=         # sinon : zenith.config.json à côté de package.json", "# ZENITH_CONFIG=         # otherwise: zenith.config.json next to package.json"),
            ].join("\n")}
          />
        </div>
      </Group>
    </div>
  );
}

const LABEL: Record<State, () => string> = {
  ok: () => tr("Branchée", "Connected"),
  missing: () => tr("À brancher", "To connect"),
  error: () => tr("Erreur", "Error"),
  unused: () => tr("Non utilisée", "Not used"),
};

function SourceLine({ r, inConfig }: { r: Row; inConfig: (field: string) => string }) {
  const s = stateOf(r);
  return (
    <div id={slug(r.name)} className={cn("grid scroll-mt-20 gap-x-6 gap-y-2 px-4 py-3 md:grid-cols-[13rem_1fr_auto]", s === "unused" && "opacity-60")}>
      <div className="min-w-0">
        <div className="truncate text-[13px] font-medium text-ink">{r.name}</div>
        <Status className="mt-1" health={s === "ok" ? "up" : s === "error" ? "down" : s === "missing" ? "warn" : "unknown"} label={LABEL[s]()} />
      </div>
      <div className="min-w-0 space-y-1">
        <div className="text-[13px] text-ink-2">{r.feeds}</div>
        <div className="text-xs text-ink-3">{r.how}</div>
        <div className="break-words font-mono text-2xs text-ink-3">{r.vars}</div>
        {s === "missing" && !r.src.ok && r.src.missing && (
          <div className="break-words text-xs text-warn">
            {tr("Manque : ", "Missing: ")}
            {r.src.missing.map((k) => (/^[a-z]/.test(k) && !k.includes(" ") ? inConfig(k) : k)).join(", ")}
          </div>
        )}
        {s === "error" && !r.src.ok && r.src.error && <div className="break-words text-xs text-bad">{r.src.error}</div>}
        {r.key && <KeyForm name={r.key} secret={r.secret} collapsed={r.src.ok} placeholder={r.src.ok && r.secret !== false ? tr("Nouvelle clé", "New key") : r.placeholder} />}
      </div>
      <div className="md:pt-0.5">
        {r.url && (
          <a
            href={r.url}
            target="_blank"
            rel="noopener noreferrer"
            className="inline-flex h-7 items-center gap-1.5 rounded-md border border-line bg-surface px-2 text-xs text-ink-2 transition-colors hover:bg-hover hover:text-ink"
          >
            {tr("Obtenir", "Get it")} <ExternalLink className="size-3 text-ink-3" />
          </a>
        )}
      </div>
    </div>
  );
}
