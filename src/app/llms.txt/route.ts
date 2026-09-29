import { DOCS, describeDoc } from "@/lib/context";
import { OWNER } from "@/lib/identity";
import { PROJECTS } from "@/lib/projects";
import { tr } from "@/lib/i18n";

/** Standard entry point for agents exploring a site. */
export function GET() {
  const who = OWNER.name ? tr(` de ${OWNER.name}`, ` of ${OWNER.name}`) : "";
  const projects = PROJECTS.length ? ` (${PROJECTS.map((p) => p.name).join(", ")})` : "";
  const body = [
    "# zenith",
    "",
    tr(
      `> Tableau de bord privé${who} : ses projets${projects}, sa vie, son argent et ses agents IA.`,
      `> Private dashboard${who}: their projects${projects}, life, money and AI agents.`,
    ),
    "",
    "## Documents",
    "",
    ...DOCS.map((d) => `- [${d}](/api/context/${d}): ${describeDoc(d)}`),
  ].join("\n");
  return new Response(body, { headers: { "Content-Type": "text/plain; charset=utf-8" } });
}
