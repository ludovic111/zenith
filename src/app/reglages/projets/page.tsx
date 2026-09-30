import type { Metadata } from "next";
import { config } from "@/lib/config";
import { tr } from "@/lib/i18n";
import { rawConfig } from "@/lib/config-write";
import { PageHeader } from "@/components/z/panel";
import { tilde } from "@/components/settings/rows";
import { ProjectsSettings } from "@/components/setup/settings";
import type { ProjectDraft } from "@/components/setup/editors";

export const dynamic = "force-dynamic";
export function generateMetadata(): Metadata {
  return { title: tr("Projets", "Projects") };
}

export default async function ProjectsSettingsPage() {
  // As written in the file: identity, probes and integrations you set by hand are kept.
  const raw = await rawConfig();
  const list = (Array.isArray(raw.projects) ? raw.projects : []) as ProjectDraft[];
  return (
    <div className="mx-auto max-w-3xl">
      <PageHeader
        title={tr("Projets", "Projects")}
        description={tr("Ce que zenith suit pour toi : commits, CI, sites, sessions d'agents. Tes agents y travaillent.", "What zenith follows for you: commits, CI, sites, agent sessions. Your agents work in them.")}
      />
      <ProjectsSettings initial={list} root={tilde(config().projectsRoot)} />
    </div>
  );
}
