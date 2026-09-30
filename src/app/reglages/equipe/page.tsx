import type { Metadata } from "next";
import { config } from "@/lib/config";
import { tr } from "@/lib/i18n";
import { PROJECTS } from "@/lib/projects";
import { detectAgents } from "@/lib/setup";
import { skills } from "@/lib/agent/skills";
import { agentAvatar, agentHome } from "@/lib/agent/team";
import { gatewayStatus } from "@/lib/agent/gateway";
import { PageHeader } from "@/components/z/panel";
import { Group, Mono, Row, tilde } from "@/components/settings/rows";
import { KeyForm } from "@/components/settings/key-form";
import { TeamSettings, type TeamDraft } from "@/components/setup/settings";
import type { BotDraft, RoutineDraft } from "@/components/setup/templates";

export const dynamic = "force-dynamic";
export function generateMetadata(): Metadata {
  return { title: tr("Équipe", "Team") };
}

export default async function TeamSettingsPage() {
  const a = config().agent;
  const initial: TeamDraft = {
    main: { id: "life", name: a.name, role: "", provider: a.provider, ...agentAvatar() },
    bots: a.bots.map((b): BotDraft => ({ id: b.id, name: b.name, title: b.title, role: b.role, provider: b.provider ?? a.provider, shape: b.shape, color: b.color, accessory: b.accessory, model: b.model, enabled: b.enabled === false ? false : undefined })),
    routines: a.routines.map((r): RoutineDraft => ({ ...r, days: r.days.length === 7 ? undefined : r.days, enabled: r.enabled === false ? false : undefined })),
    mcp: Object.entries(a.mcp).map(([name, s]) => ("url" in s ? { name, kind: "url" as const, value: s.url } : { name, kind: "command" as const, value: [s.command, ...s.args].join(" ") })),
  };
  // A clean draft: no undefined keys, so "unsaved changes" only shows real changes.
  const draft = JSON.parse(JSON.stringify(initial)) as TeamDraft;
  const gw = gatewayStatus();
  return (
    <div className="mx-auto max-w-3xl">
      <PageHeader title={tr("Équipe", "Team")} description={tr("Ton agent, son équipe, ce qu'ils font seuls et les outils qu'ils partagent.", "Your agent, its team, what they do on their own and the tools they share.")} />
      <TeamSettings initial={draft} skills={(await skills()).map((s) => s.id)} taken={PROJECTS.map((p) => p.id)} available={detectAgents()} />

      <Group title={tr("Depuis ton téléphone", "From your phone")} description={tr("Parle à ton agent sur Telegram.", "Talk to your agent on Telegram.")} className="mt-10">
        <Row
          stack
          label="Telegram"
          description={
            gw.running
              ? tr(`À l'écoute · ${gw.chats} chat${gw.chats > 1 ? "s" : ""} autorisé${gw.chats > 1 ? "s" : ""}.`, `Listening · ${gw.chats} allowed chat${gw.chats === 1 ? "" : "s"}.`)
              : tr("Crée un bot avec @BotFather, colle son jeton ici, puis envoie-lui /start : il te donne l'id du chat à autoriser (agent.gateway.telegram.chats).", "Create a bot with @BotFather, paste its token here, then send it /start: it tells you the chat id to allow (agent.gateway.telegram.chats).")
          }
        >
          <KeyForm name="TELEGRAM_BOT_TOKEN" collapsed={gw.token} placeholder={gw.token ? tr("Nouveau jeton", "New token") : "123456:ABC…"} />
        </Row>
      </Group>

      <Group title={tr("Leurs dossiers", "Their folders")} className="mt-8">
        <Row label={tr("Ton agent", "Your agent")} description={tr("SOUL.md (sa personnalité), USER.md (toi), MEMORY.md, skills/", "SOUL.md (its personality), USER.md (you), MEMORY.md, skills/")}>
          <Mono value={agentHome()}>{tilde(agentHome())}</Mono>
        </Row>
      </Group>
    </div>
  );
}
