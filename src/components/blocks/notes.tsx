import { NotebookPen } from "lucide-react";
import type { Project } from "@/lib/projects";
import { source } from "@/lib/source";
import { ago } from "@/lib/format";
import { tr } from "@/lib/i18n";
import { notes } from "@/lib/sources/obsidian";
import { Panel } from "@/components/z/panel";

/** Notes of the Obsidian vault about this project (by folder or name). */
export async function NotesPanel({ project: p }: { project: Project }) {
  const all = await source(notes);
  if (!all.ok) return null;
  const mine = all.data.notes.filter((n) => n.project === p.id);
  if (!mine.length) return null;
  return (
    <Panel kicker="Obsidian" title={tr("Tes notes sur le projet", "Your notes on the project")} accent={p.glow}>
      <ul className="grid gap-x-8 gap-y-1 md:grid-cols-2">
        {mine.map((n) => (
          <li key={n.path}>
            <a href={n.url} className="group flex gap-3 rounded-xl px-2 py-2 text-sm hover:bg-white/[0.03]">
              <NotebookPen className="mt-0.5 size-4 shrink-0" style={{ color: p.glow }} />
              <div className="min-w-0 flex-1">
                <div className="truncate text-ink group-hover:underline">{n.title}</div>
                <div className="line-clamp-2 text-xs text-ink-3">{ago(n.modified)} · {n.excerpt || tr("note vide", "empty note")}</div>
              </div>
            </a>
          </li>
        ))}
      </ul>
    </Panel>
  );
}
