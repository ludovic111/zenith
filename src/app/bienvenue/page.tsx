import type { Metadata } from "next";
import { headers } from "next/headers";
import { config } from "@/lib/config";
import { tr } from "@/lib/i18n";
import { rawConfig } from "@/lib/config-write";
import { codeStatus } from "@/lib/code/manager";
import { currencyFor, detectAgents, scanProjects } from "@/lib/setup";
import { tilde } from "@/components/settings/rows";
import { agentAvatar } from "@/lib/agent/team";
import { Welcome } from "@/components/setup/welcome";
import type { BotDraft, RoutineDraft } from "@/components/setup/templates";

export const dynamic = "force-dynamic";
export function generateMetadata(): Metadata {
  return { title: tr("Bienvenue", "Welcome") };
}

/** The first run: who you are, your projects, your team, their routines. */
export default async function WelcomePage() {
  const c = config();
  const raw = await rawConfig();
  // No locale chosen yet: the browser's.
  const browser = (await headers()).get("accept-language")?.split(",")[0]?.trim() || "en-US";
  const locale = typeof raw.locale === "string" ? c.locale : /^fr|^en/i.test(browser) ? (browser.includes("-") ? browser : browser.startsWith("fr") ? "fr-FR" : "en-US") : "en-US";
  const found = await scanProjects(c.projectsRoot).catch(() => []);
  const a = c.agent;
  const props = JSON.parse(
    JSON.stringify({
      you: { name: c.owner.name, locale, currency: typeof raw.currency === "string" ? c.currency : currencyFor(locale), timezone: c.timezone, location: c.location },
      root: tilde(c.projectsRoot),
      found,
      existing: c.projects.map((p) => ({ id: p.id, name: p.name, dir: p.dir, repo: p.repo })),
      main: { id: "life", name: a.name, role: "", provider: a.provider, ...agentAvatar() } satisfies BotDraft,
      bots: a.bots.map((b): BotDraft => ({ id: b.id, name: b.name, title: b.title, role: b.role, provider: b.provider ?? a.provider, shape: b.shape, color: b.color, accessory: b.accessory })),
      routines: a.routines.map((r): RoutineDraft => ({ ...r })),
      available: detectAgents(),
      codeReady: codeStatus().running,
    }),
  );
  return <Welcome {...props} />;
}
