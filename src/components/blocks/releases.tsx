import { Download } from "lucide-react";
import type { Project } from "@/lib/projects";
import type { Release } from "@/lib/sources/github";
import { date, nf } from "@/lib/format";
import { tr } from "@/lib/i18n";
import { agentUi } from "@/lib/agent/ui";
import { AskButton } from "@/components/agent/ask-button";
import { Chip } from "@/components/z/panel";

/** Latest releases as dense rows: name, date, downloads. `count` says what a download is. */
export function ReleaseRows({ project: p, list, count, max = 7 }: { project: Project; list: Release[]; count: (r: Release) => number; max?: number }) {
  return (
    <ul>
      {list.slice(0, max).map((r, i) => (
        <li key={r.tag_name}>
          <a href={r.html_url} target="_blank" rel="noopener noreferrer" className="-mx-1.5 flex h-9 items-center gap-2.5 rounded-md px-1.5 text-[13px] transition-colors hover:bg-hover">
            <span className={i === 0 ? "size-1.5 shrink-0 rounded-full" : "size-1.5 shrink-0 rounded-full bg-ink-3/40"} style={i === 0 ? { background: p.color } : undefined} />
            <span className="min-w-0 truncate font-medium text-ink">{r.name || r.tag_name}</span>
            {i === 0 && <Chip>{tr("dernière", "latest")}</Chip>}
            <span className="ml-auto shrink-0 text-xs text-ink-3">{date(r.published_at, { day: "numeric", month: "short" })}</span>
            <span className="inline-flex w-14 shrink-0 items-center justify-end gap-1 text-xs text-ink-2 tabular">
              <Download className="size-3 text-ink-3" />
              {nf(count(r))}
            </span>
          </a>
        </li>
      ))}
    </ul>
  );
}

/** "Write the release notes", aimed at the project, since its latest tag. Nothing when agents are off. */
export function ReleaseNotesButton({ project: p, latest }: { project: Project; latest?: string }) {
  const ui = agentUi();
  if (!ui.enabled || !ui.targets.some((t) => t.id === p.id)) return null;
  const since = latest ? `${latest}..HEAD` : "<dernier tag>..HEAD";
  return (
    <AskButton
      target={p.id}
      label={tr("Écris les notes de version", "Write the release notes")}
      prompt={tr(
        `Écris les notes de la prochaine version de ${p.name} : lis les commits ${latest ? `depuis ${latest} (git log ${since})` : "depuis le dernier tag"}, regroupe-les par thème (nouveautés, corrections, sous le capot) et rédige des notes claires pour les utilisateurs, en français et en anglais. Enregistre-les dans un brouillon ; ne crée ni tag ni release et ne pousse rien sans me demander.`,
        `Write ${p.name}'s next release notes: read the commits ${latest ? `since ${latest} (git log ${latest}..HEAD)` : "since the last tag"}, group them by theme (new, fixed, under the hood) and write clear notes for users, in English and French. Save them as a draft; don't create any tag or release and don't push anything without asking me.`,
      )}
    />
  );
}
