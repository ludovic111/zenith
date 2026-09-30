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
import { PageHeader } from "@/components/z/panel";
import { Status } from "@/components/z/status";
import { CodeActions, CodeState } from "@/components/settings/code-actions";
import { agentAvatar, agentName, bots } from "@/lib/agent/team";
import { AgentAvatar } from "@/components/agent/agent-avatar";
import { YouSettings } from "@/components/setup/settings";
import { UpdatePanel } from "@/components/setup/updates";
import { selfRestarts, updateState } from "@/lib/updater";
import type { YouDraft } from "@/components/setup/you";
import { Code, CodeBlock, Group, Mono, Row, tilde, Toggle } from "@/components/settings/rows";

export const dynamic = "force-dynamic";

export function generateMetadata(): Metadata {
  return { title: tr("Réglages", "Settings") };
}

export default async function General() {
  const c = config();
  const codeHere = existsSync(path.join(process.cwd(), "src", "lib", "code"));
  const [mac, update] = await Promise.all([source(apple), updateState()]);
  const team = bots();
  const agent = config().agent;
  const you: YouDraft = JSON.parse(JSON.stringify({ name: c.owner.name, locale: c.locale, currency: c.currency, timezone: c.timezone, location: c.location }));
  const code = codeStatus();
  const node = process.execPath;
  const mcp = `${process.cwd()}/scripts/mcp/zenith-mcp.mjs`;
  const yesNo: [string, string] = [tr("Activé", "On"), tr("Désactivé", "Off")];
  const snapshot = mac.ok ? mac.data : null;

  return (
    <div className="mx-auto max-w-3xl">
      <PageHeader title={tr("Général", "General")} description={tr("Toi, ton équipe, tes projets, et ce qui fait tourner zenith.", "You, your team, your projects, and what keeps zenith running.")} />

      <section>
        <div className="mb-2 px-1">
          <h2 className="text-[13px] font-semibold text-ink">{tr("Toi", "You")}</h2>
          <p className="mt-0.5 text-xs text-ink-3">{tr("Ce que zenith sait de toi, pour te saluer, compter et prévoir.", "What zenith knows about you, to greet you, count and plan.")}</p>
        </div>
        <YouSettings initial={you} />
      </section>

      <Group className="mt-8">
        {[
          { href: "/reglages/equipe", name: tr("Équipe", "Team"), hint: agent.enabled ? tr(`${agentName()} et ${team.length} ${plural(team.length, ["agent", "agents"], ["agent", "agents"])} · routines · outils partagés`, `${agentName()} and ${team.length} ${team.length === 1 ? "agent" : "agents"} · routines · shared tools`) : tr("Désactivée", "Off"), face: true },
          { href: "/reglages/projets", name: tr("Projets", "Projects"), hint: PROJECTS.length ? PROJECTS.map((p) => p.name).join(", ") : tr("Aucun pour l'instant", "None yet") },
          { href: "/reglages/sources", name: tr("Sources de données", "Data sources"), hint: tr("Clés d'API, comptes et ce que chaque source alimente", "API keys, accounts and what each source feeds") },
        ].map((l) => (
          <Link key={l.href} href={l.href} className="flex min-h-12 items-center justify-between gap-4 px-4 py-3 transition-colors hover:bg-hover">
            <div className="min-w-0">
              <div className="text-[13px] text-ink">{l.name}</div>
              <div className="mt-0.5 truncate text-xs text-ink-3">{l.hint}</div>
            </div>
            <span className="flex shrink-0 items-center gap-1">
              {l.face && [agentAvatar(), ...team.map((b) => b.avatar)].slice(0, 6).map((av, i) => <AgentAvatar key={i} avatar={av} id={`gen-${i}`} size={18} />)}
              <ChevronRight className="ml-1 size-4 text-ink-3" />
            </span>
          </Link>
        ))}
      </Group>

      <section id="maj" className="mt-8 scroll-mt-16">
        <div className="mb-2 px-1">
          <h2 className="text-[13px] font-semibold text-ink">{tr("Mises à jour", "Updates")}</h2>
          <p className="mt-0.5 text-xs text-ink-3">{tr("zenith se met à jour depuis GitHub.", "zenith updates itself from GitHub.")}</p>
        </div>
        <UpdatePanel initial={update} auto={c.updates.auto} selfRestarts={selfRestarts()} />
      </section>

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

      <ConfigGroup />

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
    <Group title={tr("Configuration", "Configuration")} description={tr("Les réglages l'écrivent pour toi. Modifiée à la main, elle est relue au redémarrage.", "Settings write it for you. Edited by hand, it is read again at restart.")}>
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
      <Row label={tr("Accueil", "Welcome")} description={tr("Reprendre la mise en route depuis le début", "Go through the setup again")}>
        <Link href="/bienvenue" className="text-[13px] text-ink-2 underline-offset-4 hover:text-ink hover:underline">
          {tr("Recommencer", "Start again")}
        </Link>
      </Row>
      {found && !error && (
        <Row label={tr("Extensions privées", "Private extensions")} description={tr("Pages et sources propres à tes projets, dans perso/", "Pages and sources specific to your projects, in perso/")}>
          <Code>docs/extensions.md</Code>
        </Row>
      )}
    </Group>
  );
}
