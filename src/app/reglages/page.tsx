import type { Metadata } from "next";
import { existsSync } from "node:fs";
import path from "node:path";
import Link from "next/link";
import { ChevronRight } from "lucide-react";
import { config } from "@/lib/config";
import { PROJECTS } from "@/lib/projects";
import { plural, tr } from "@/lib/i18n";
import { ago } from "@/lib/format";
import { source } from "@/lib/source";
import { apple } from "@/lib/sources/apple";
import { codeStatus } from "@/lib/code/manager";
import { agentUi } from "@/lib/agent/ui";
import { PageHeader } from "@/components/z/panel";
import { Status } from "@/components/z/status";
import { ProviderIcon } from "@/components/agent/provider-icon";
import { RoutinesList } from "@/components/agent/routines-panel";
import { routineViews } from "@/components/agent/routine-view";
import { CodeActions, CodeState } from "@/components/settings/code-actions";
import { KeyForm } from "@/components/settings/key-form";
import { gatewayStatus } from "@/lib/agent/gateway";
import { agentName, bots } from "@/lib/agent/team";
import { Code, CodeBlock, Group, Mono, Row, tilde, Toggle } from "@/components/settings/rows";

export const dynamic = "force-dynamic";

export function generateMetadata(): Metadata {
  return { title: tr("Réglages", "Settings") };
}

export default async function General() {
  const c = config();
  const codeHere = existsSync(path.join(process.cwd(), "src", "lib", "code"));
  const [mac, list] = await Promise.all([source(apple), routineViews()]);
  const team = bots();
  const gw = gatewayStatus();
  const ui = agentUi();
  const code = codeStatus();
  const node = process.execPath;
  const mcp = `${process.cwd()}/scripts/mcp/zenith-mcp.mjs`;
  const yesNo: [string, string] = [tr("Activé", "On"), tr("Désactivé", "Off")];
  const snapshot = mac.ok ? mac.data : null;

  return (
    <div className="mx-auto max-w-3xl">
      <PageHeader title={tr("Général", "General")} description={tr("Ce que zenith lit au démarrage, ses agents et son app Mac.", "What zenith reads at startup, its agents and its Mac app.")} />

      <ConfigGroup />

      <Group title={tr("Préférences", "Preferences")} description={tr("Dans zenith.config.json.", "In zenith.config.json.")}>
        <Row label={tr("Langue", "Language")} description="locale">
          <span className="font-mono text-xs">{c.locale}</span>
        </Row>
        <Row label={tr("Fuseau horaire", "Time zone")} description="timezone">
          <span className="font-mono text-xs">{c.timezone}</span>
        </Row>
        <Row label={tr("Devise", "Currency")} description={tr("Tous les totaux y sont convertis", "Every total is converted to it")}>
          <span className="font-mono text-xs">{c.currency}</span>
        </Row>
        <Row label={tr("Lieu", "Location")} description={tr("Météo, air, fériés", "Weather, air, holidays")}>
          {c.location ? (
            <span>
              {c.location.name}
              {c.location.region || c.location.country ? <span className="ml-1.5 font-mono text-xs text-ink-3">{c.location.region ?? c.location.country}</span> : null}
            </span>
          ) : (
            <span className="text-ink-3">{tr("Non renseigné", "Not set")}</span>
          )}
        </Row>
        <Row label={tr("Projets", "Projects")} description={tilde(c.projectsRoot)}>
          {PROJECTS.length ? (
            <span className="flex flex-wrap items-center justify-end gap-x-3 gap-y-1">
              {PROJECTS.map((p) => (
                <Link key={p.id} href={`/p/${p.id}`} className="inline-flex items-center gap-1.5 text-ink-2 hover:text-ink">
                  <span className="size-2 rounded-full" style={{ background: p.color }} />
                  {p.name}
                </Link>
              ))}
            </span>
          ) : (
            <span className="text-ink-3">{tr("Aucun", "None")}</span>
          )}
        </Row>
        <Link href="/reglages/sources" className="flex min-h-12 items-center justify-between gap-4 px-4 py-3 transition-colors hover:bg-hover">
          <div>
            <div className="text-[13px] text-ink">{tr("Sources de données", "Data sources")}</div>
            <div className="mt-0.5 text-xs text-ink-3">{tr("Clés d'API, comptes et ce que chaque source alimente", "API keys, accounts and what each source feeds")}</div>
          </div>
          <ChevronRight className="size-4 text-ink-3" />
        </Link>
      </Group>

      {codeHere && (
        <Group title="zenith code" description={tr("L'espace de code, un fork de T3 Code lancé avec zenith.", "The coding workspace, a T3 Code fork started with zenith.")}>
          <Row label={tr("État", "Status")} description={c.code.enabled ? tr("Démarre et redémarre avec zenith", "Starts and restarts with zenith") : undefined}>
            <CodeState initial={{ enabled: code.enabled, built: code.built, running: code.running, starting: code.starting, version: code.version, lastError: code.lastError }} />
          </Row>
          <Row label={tr("Activé", "Enabled")} description="code.enabled">
            <Toggle on={c.code.enabled} labels={yesNo} />
          </Row>
          <Row label="Port" description="code.port">
            <span className="font-mono text-xs tabular">{c.code.port}</span>
          </Row>
          <Row label={tr("Données", "Data")} description={tr("Fils, réglages et pièces jointes", "Threads, settings and attachments")}>
            <Mono value={code.home}>{tilde(code.home)}</Mono>
          </Row>
          {c.code.enabled && (
            <Row label={tr("Actions", "Actions")} description={tr("Ses propres réglages sont dans la section Code de la barre latérale.", "Its own settings are in the sidebar's Code section.")}>
              <CodeActions />
            </Row>
          )}
        </Group>
      )}

      <Group
        title={tr("Agent zenith", "zenith agent")}
        description={tr(
          "« Demande à zenith », son équipe, la liste Maintenant, les routines, Telegram. Passe par zenith code, sur tes abonnements Claude et ChatGPT.",
          '"Ask zenith", its team, the Now list, routines, Telegram. Runs through zenith code, on your Claude and ChatGPT subscriptions.',
        )}
      >
        <Row label={tr("Activé", "Enabled")} description={c.agent.enabled && !c.code.enabled ? tr("Inactif tant que zenith code est désactivé", "Inactive while zenith code is disabled") : "agent.enabled"}>
          <Toggle on={ui.enabled} labels={yesNo} />
        </Row>
        <Row label={tr("Fournisseur", "Provider")} description={c.agent.model ? `agent.provider · ${c.agent.model}` : "agent.provider"}>
          <span className="inline-flex items-center gap-2">
            <ProviderIcon id={c.agent.provider} size={14} className="text-ink-2" />
            {c.agent.provider === "claude" ? "Claude Code" : "Codex"}
          </span>
        </Row>
        <Row label={tr("Nom", "Name")} description="agent.name">
          {agentName()}
        </Row>
        <Row label={tr("Dossier", "Folder")} description={tr("Son bureau : SOUL.md (personnalité), USER.md (toi), MEMORY.md, skills/", "Its desk: SOUL.md (personality), USER.md (you), MEMORY.md, skills/")}>
          <Mono value={ui.home}>{tilde(ui.home)}</Mono>
        </Row>
        <Row label={tr("Équipe", "Team")} description="agent.bots">
          {team.length ? (
            <Link href="/agents" className="inline-flex flex-wrap items-center justify-end gap-x-3 gap-y-1 hover:text-ink">
              {team.map((b) => (
                <span key={b.id} className="inline-flex items-center gap-1.5">
                  {b.emoji && <span>{b.emoji}</span>}
                  {b.name}
                  <ProviderIcon id={b.provider} size={11} className="text-ink-3" />
                </span>
              ))}
            </Link>
          ) : (
            <span className="text-ink-3">{tr("Aucun bot", "No bot")}</span>
          )}
        </Row>
        <Row
          stack
          label="Telegram"
          description={
            gw.running
              ? tr(`À l'écoute · ${gw.chats} chat${gw.chats > 1 ? "s" : ""} autorisé${gw.chats > 1 ? "s" : ""} (agent.gateway.telegram.chats)`, `Listening · ${gw.chats} allowed chat${gw.chats === 1 ? "" : "s"} (agent.gateway.telegram.chats)`)
              : gw.configured
                ? tr("Colle le jeton de ton bot (@BotFather), puis envoie-lui /start : il te donne l'id du chat à autoriser.", "Paste your bot's token (@BotFather), then send it /start: it tells you the chat id to allow.")
                : tr("Parle à ton agent depuis ton téléphone : ajoute \"gateway\": { \"telegram\": { \"chats\": [] } } dans agent, puis colle ici le jeton de ton bot.", 'Talk to your agent from your phone: add "gateway": { "telegram": { "chats": [] } } to agent, then paste your bot\'s token here.')
          }
        >
          <KeyForm name="TELEGRAM_BOT_TOKEN" collapsed={gw.token} placeholder={gw.token ? tr("Nouveau jeton", "New token") : "123456:ABC…"} />
        </Row>
        <div>
          <div className="flex items-baseline justify-between gap-4 px-4 pb-1 pt-3">
            <div className="text-[13px] text-ink">
              {tr("Routines", "Routines")}
              {list.length > 0 && <span className="ml-2 text-xs text-ink-3 tabular">{list.filter((r) => r.enabled).length}/{list.length}</span>}
            </div>
            <span className="text-xs text-ink-3">agent.routines</span>
          </div>
          {list.length ? (
            <RoutinesList routines={list} />
          ) : (
            <p className="px-4 pb-3 text-xs text-ink-3">
              {tr("Des agents qui travaillent seuls, à heure fixe. Par exemple : ", "Agents that work on their own, at a set time. For example: ")}
              <Code>{`"agent": { "routines": [{ "id": "matin", "at": "07:30", "task": "refresh-life" }] }`}</Code>
            </p>
          )}
        </div>
      </Group>

      <Group
        title={tr("Brancher tes agents IA", "Connect your AI agents")}
        description={tr(
          "zenith résume ta vie et tes projets en Markdown, toutes les 10 minutes. Un agent qui le lit sait où tu en es.",
          "zenith sums up your life and projects in Markdown, every 10 minutes. An agent that reads it knows where you stand.",
        )}
      >
        <Row stack label={tr("Claude Code", "Claude Code")} description={tr("Pour toutes tes sessions", "For all your sessions")}>
          <CodeBlock code={`claude mcp add zenith --scope user -- ${node} ${mcp}`} />
        </Row>
        <Row stack label="Codex">
          <CodeBlock code={`codex mcp add zenith -- ${node} ${mcp}`} />
        </Row>
        <Row stack label={tr("Claude Desktop, Cursor…", "Claude Desktop, Cursor…")} description={tr("Fichier de config MCP", "MCP config file")}>
          <CodeBlock code={JSON.stringify({ mcpServers: { zenith: { command: node, args: [mcp] } } }, null, 2)} />
        </Row>
        <Row stack label={tr("Fichiers Markdown", "Markdown files")} description={tr("N'importe quel outil", "Any tool")}>
          <CodeBlock code={`${process.cwd()}/context/brief.md`} display={`${tilde(process.cwd())}/context/brief.md`} />
        </Row>
        <Row stack label="HTTP" description={tr("Depuis ce Mac", "From this Mac")}>
          <div className="grid gap-2 sm:grid-cols-2">
            <CodeBlock code="http://127.0.0.1:4747/api/context" />
            <CodeBlock code="http://127.0.0.1:4747/llms.txt" />
          </div>
        </Row>
        <p className="px-4 py-3 text-xs text-ink-3">
          {tr("Pour lire : ", "To read: ")}
          <Code>zenith_brief</Code> ({tr("en premier", "first")}), <Code>zenith_project</Code>, <Code>zenith_document</Code>, <Code>zenith_search_notes</Code>, <Code>zenith_read_note</Code>.{" "}
          {tr("Pour agir (zenith ouvert) : ", "To act (zenith running): ")}
          <Code>zenith_now</Code>, <Code>zenith_delegate</Code>, <Code>zenith_agent</Code>, <Code>zenith_done</Code>.
        </p>
      </Group>

      <Group title={tr("App Mac", "Mac app")} description={tr("zenith.app lit ce que seul macOS sait et l'envoie au serveur local.", "zenith.app reads what only macOS knows and sends it to the local server.")}>
        <Row label={tr("Dernier relevé", "Last snapshot")} description={tr("Toutes les 5 minutes, dans .data/apple.json", "Every 5 minutes, into .data/apple.json")}>
          {snapshot ? (
            <Status health={Date.now() - new Date(snapshot.capturedAt).getTime() < 20 * 60e3 ? "up" : "warn"} label={ago(snapshot.capturedAt)} />
          ) : (
            <Status health="unknown" label={tr("Jamais lancée", "Never run")} />
          )}
        </Row>
        <Row
          label={tr("Ce qu'elle lit", "What it reads")}
          description={tr(
            "Calendrier, Rappels, Mail (s'il est ouvert), anniversaires, musique, temps d'écran. En lecture seule ; les autorisations sont dans Réglages Système → Confidentialité et sécurité.",
            "Calendar, Reminders, Mail (when running), birthdays, music, screen time. Read-only; permissions live in System Settings → Privacy & Security.",
          )}
        />
        <Row label={tr("Identifiant", "Bundle id")} description={tr("Le changer réinitialise les autorisations macOS", "Changing it resets macOS permissions")}>
          <span className="font-mono text-xs">{c.mac.bundleId}</span>
        </Row>
        <Row stack label={tr("Installer ou mettre à jour", "Install or update")} description={tr("Construit zenith et zenith code, compile l'app, garde le serveur en marche dès l'ouverture de session. À relancer après chaque mise à jour.", "Builds zenith and zenith code, compiles the app, keeps the server running from login. Run it again after every update.")}>
          <CodeBlock code="npm run mac:install" />
        </Row>
      </Group>

      <Group title={tr("Confidentialité", "Privacy")}>
        <Row label={tr("Local uniquement", "Local only")} description={tr("Le serveur n'écoute que 127.0.0.1 et refuse tout autre en-tête Host.", "The server only listens on 127.0.0.1 and rejects any other Host header.")} />
        <Row label={tr("Clés", "Keys")} description={tr("Restent côté serveur, dans .env.local ; rien ne part ailleurs qu'aux API que tu as branchées.", "Stay server-side, in .env.local; nothing goes anywhere but the APIs you connected.")} />
        <Row
          label={tr("Ignoré par git", "Ignored by git")}
          description={tr("Tes données personnelles ne quittent pas ce Mac.", "Your personal data never leaves this Mac.")}
        >
          <span className="flex flex-wrap justify-end gap-1">
            {["perso/", "zenith.config.json", ".env.local", ".data/", "context/"].map((f) => (
              <Code key={f}>{f}</Code>
            ))}
          </span>
        </Row>
        <Row stack label={tr("Avant de publier", "Before publishing")} description={tr("Cherche tes valeurs personnelles et tes secrets dans les fichiers suivis ; --install le lance avant chaque git push.", "Looks for your personal values and secrets in tracked files; --install runs it before each git push.")}>
          <CodeBlock code="npm run privacy" />
        </Row>
      </Group>

      <p className="mt-6 px-1 text-xs text-ink-3">
        {PROJECTS.length} {plural(PROJECTS.length, ["projet", "projets"], ["project", "projects"])} · {tr("référence complète : ", "full reference: ")}
        <Code>docs/configuration.md</Code>
      </p>
    </div>
  );
}

/** Where the config lives, whether it was read, and how to create it. */
function ConfigGroup() {
  const c = config();
  const { file, found, error } = c.meta;
  const example = path.join(path.dirname(file), "zenith.config.example.json");
  return (
    <Group title={tr("Configuration", "Configuration")} description={tr("Lue au démarrage : redémarre zenith après l'avoir modifiée.", "Read at startup: restart zenith after editing it.")}>
      <Row label={tr("Fichier", "File")} description={tr("Modifiable à la main ; ton éditeur le vérifie grâce à zenith.schema.json", "Edit it by hand; your editor checks it with zenith.schema.json")}>
        <Mono value={file}>{tilde(file)}</Mono>
      </Row>
      <Row label={tr("État", "Status")}>
        <Status health={error ? "down" : found ? "up" : "warn"} label={error ? tr("Erreurs de validation", "Validation errors") : found ? tr("Lue", "Loaded") : tr("Introuvable", "Not found")} />
      </Row>
      {error && (
        <div className="bg-bad/5 px-4 py-3">
          <div className="text-xs text-ink-2">{tr("zenith a démarré avec une configuration vide. À corriger :", "zenith started with an empty configuration. To fix:")}</div>
          <ul className="mt-1.5 space-y-0.5 font-mono text-xs text-bad">
            {error.split(" · ").map((e) => (
              <li key={e} className="break-words">
                {e}
              </li>
            ))}
          </ul>
        </div>
      )}
      {(!found || error) && (
        <ol className="list-decimal space-y-3 py-3 pl-9 pr-4 text-[13px] text-ink-2 marker:text-ink-3">
          {!found && (
            <li>
              {tr("Copie l'exemple (ou mets-le dans ", "Copy the example (or put it in ")}
              <Code>perso/zenith.config.json</Code>
              {tr(", à côté de tes extensions privées ; les deux sont ignorés par git) :", ", next to your private extensions; git ignores both):")}
              <div className="mt-1.5">
                <CodeBlock code={`cp "${example}" "${file}"`} display={`cp ${tilde(example)} ${tilde(file)}`} />
              </div>
            </li>
          )}
          <li>
            {tr("Remplis-le : toi (", "Fill it in: you (")}
            <Code>owner</Code>
            {tr("), ta ville (", "), your city (")}
            <Code>location</Code>
            {tr("), tes projets (", "), your projects (")}
            <Code>projects</Code>
            {tr("), tes abonnements… Chaque champ est décrit dans ", "), your subscriptions… Every field is described in ")}
            <Code>docs/configuration.md</Code>.
          </li>
          <li>
            {tr("Garde la ligne ", "Keep the line ")}
            <Code>&quot;$schema&quot;: &quot;./zenith.schema.json&quot;</Code>
            {tr(" pour l'autocomplétion. Le schéma se régénère avec ", " for completion. Regenerate the schema with ")}
            <Code>npm run config:schema</Code>.
          </li>
          <li>
            {tr("Redémarre zenith (", "Restart zenith (")}
            <Code>npm run dev</Code>
            {tr(", ou ", ", or ")}
            <Code>npm run mac:install</Code>
            {tr(" pour l'app).", " for the app).")}
          </li>
        </ol>
      )}
      {found && !error && (
        <Row label={tr("Extensions privées", "Private extensions")} description={tr("Pages et sources propres à tes projets, dans perso/", "Pages and sources specific to your projects, in perso/")}>
          <Code>docs/extensions.md</Code>
        </Row>
      )}
    </Group>
  );
}
