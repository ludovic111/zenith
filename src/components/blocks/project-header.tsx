import { Suspense } from "react";
import Link from "next/link";
import { ArrowUpRight, SquareTerminal } from "lucide-react";
import { config } from "@/lib/config";
import { projectDir, type Project } from "@/lib/projects";
import { tr } from "@/lib/i18n";
import { projectVersion } from "@/lib/sources/git";
import { uptime } from "@/lib/sources/uptime";
import { agentUi } from "@/lib/agent/ui";
import { Chip, Skeleton } from "@/components/z/panel";
import { Status } from "@/components/z/status";
import { ProjectAttention, type Attention } from "./attention";
import { ProjectAsk, type QuickAsk } from "./project-ask";

const quiet =
  "inline-flex h-7 items-center gap-1.5 rounded-md border border-line bg-surface px-2 text-xs font-medium text-ink-2 transition-colors hover:bg-hover hover:text-ink";

/** Requests any project can take, offered from the header's ask menu. */
function quickAsks(p: Project): QuickAsk[] {
  return [
    {
      label: tr(`Résume la semaine de ${p.name}`, `Summarise ${p.name}'s week`),
      prompt: tr(
        `Résume ma semaine sur ${p.name} : commits et PR des 7 derniers jours (git log --since="7 days ago"), ce qui a été livré, ce qui est en cours, ce qui casse. Termine par les 3 prochaines choses à faire. Ne modifie rien.`,
        `Summarise my week on ${p.name}: commits and PRs of the last 7 days (git log --since="7 days ago"), what shipped, what's in progress, what's broken. End with the 3 next things to do. Don't change anything.`,
      ),
    },
    {
      label: tr("Trouve et corrige le bug le plus gênant", "Find and fix the most annoying bug"),
      prompt: tr(
        `Trouve le bug le plus gênant de ${p.name} pour ses utilisateurs (issues, TODO, logs, tests qui échouent), explique-le, puis corrige-le sur une nouvelle branche avec un test. Ne pousse pas sur la branche principale et ne déploie rien.`,
        `Find the most annoying bug in ${p.name} for its users (issues, TODOs, logs, failing tests), explain it, then fix it on a new branch with a test. Don't push to the main branch and don't deploy anything.`,
      ),
    },
    {
      label: tr("Écris les notes de la prochaine version", "Write the next release notes"),
      prompt: tr(
        `Écris les notes de la prochaine version de ${p.name} : liste les commits depuis le dernier tag (git describe --tags --abbrev=0, puis git log <tag>..HEAD), regroupe-les par thème et rédige des notes claires pour les utilisateurs. Enregistre-les en brouillon ; ne crée ni tag ni release.`,
        `Write ${p.name}'s next release notes: list the commits since the last tag (git describe --tags --abbrev=0, then git log <tag>..HEAD), group them by theme and write clear notes for users. Save them as a draft; don't create any tag or release.`,
      ),
    },
    {
      label: tr("Ajoute des tests là où ça casse", "Add tests where it breaks"),
      prompt: tr(
        `Repère dans ${p.name} le code le plus fragile et le moins testé (fichiers souvent modifiés, bugs récents) et ajoute des tests utiles sur une nouvelle branche. Lance la suite de tests ; ne pousse rien sans me demander.`,
        `Find the most fragile, least tested code in ${p.name} (files changed often, recent bugs) and add useful tests on a new branch. Run the test suite; don't push anything without asking me.`,
      ),
    },
  ];
}

/**
 * A project page's header: color dot, name, version and status; tagline and links; the agent
 * and zenith code entries. Under it, what needs attention (`attention` adds project-specific
 * items), then `children` (the project's key numbers).
 */
export async function ProjectHeader({
  project: p,
  actions,
  attention,
  quick = [],
  children,
}: {
  project: Project;
  actions?: React.ReactNode;
  attention?: () => Promise<Attention[]>;
  quick?: QuickAsk[];
  children?: React.ReactNode;
}) {
  const [version, probes] = await Promise.all([projectVersion(p.id), uptime()]);
  const mine = probes.filter((u) => u.project === p.id);
  const anyDown = mine.some((u) => u.up === false);
  const allUp = mine.length > 0 && mine.every((u) => u.up);
  const ms = mine[0]?.last?.ms;
  const ui = agentUi();
  const canAsk = ui.enabled && ui.targets.some((t) => t.id === p.id);
  const code = config().code.enabled && !!projectDir(p);
  const links = [...p.links];
  if (p.repo && !links.some((l) => l.url.includes(`github.com/${p.repo}`))) links.push({ label: "GitHub", url: `https://github.com/${p.repo}` });

  return (
    <>
      <header className="mb-4 flex flex-wrap items-start justify-between gap-x-6 gap-y-3">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-x-2.5 gap-y-1">
            <span className="size-2.5 shrink-0 rounded-full" style={{ background: p.color }} />
            <h1 className="text-xl font-semibold tracking-tight text-ink">{p.name}</h1>
            {version && <Chip className="font-mono tabular">v{version}</Chip>}
            {mine.length > 0 && (
              <Status
                health={anyDown ? "down" : allUp ? "up" : "unknown"}
                label={
                  anyDown
                    ? tr("Incident en cours", "Incident")
                    : allUp
                      ? `${tr("En ligne", "Online")}${ms != null ? ` · ${ms} ms` : ""}`
                      : tr("Mesure en cours", "Measuring…")
                }
              />
            )}
          </div>
          {p.tagline && <p className="mt-1 text-sm text-ink-3">{p.tagline}</p>}
          {links.length > 0 && (
            <nav className="-ml-1.5 mt-2 flex flex-wrap items-center gap-0.5">
              {links.map((l) => (
                <a
                  key={l.url}
                  href={l.url}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="inline-flex h-6 items-center gap-1 rounded-md px-1.5 text-xs text-ink-3 transition-colors hover:bg-hover hover:text-ink"
                >
                  {l.label}
                  <ArrowUpRight className="size-3" />
                </a>
              ))}
            </nav>
          )}
        </div>
        {(actions || code || canAsk) && (
          <div className="flex shrink-0 flex-wrap items-center gap-2">
            {actions}
            {code && (
              <Link href={`/code?project=${encodeURIComponent(p.id)}`} className={quiet}>
                <SquareTerminal className="size-3.5" />
                {tr("Ouvrir dans zenith code", "Open in zenith code")}
              </Link>
            )}
            {canAsk && (
              <ProjectAsk
                id={p.id}
                label={tr(`Demander à zenith`, `Ask zenith`)}
                moreLabel={tr("Demandes toutes prêtes", "Ready-made requests")}
                quick={[...quick, ...quickAsks(p)]}
              />
            )}
          </div>
        )}
      </header>
      <Suspense fallback={<Skeleton className="h-10" />}>
        <ProjectAttention project={p} extra={attention} />
      </Suspense>
      {children && <div className="mt-4">{children}</div>}
    </>
  );
}
