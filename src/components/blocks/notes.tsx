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
    <Panel title={tr("Notes", "Notes")} action={`Obsidian · ${mine.length}`} bodyClassName="px-2 pb-2 pt-1">
      <ul className="grid gap-x-2 md:grid-cols-2">
        {mine.map((n) => (
          <li key={n.path}>
            <a href={n.url} className="flex gap-2.5 rounded-md px-2 py-1.5 transition-colors hover:bg-hover">
              <NotebookPen className="mt-0.5 size-3.5 shrink-0 text-ink-3" />
              <div className="min-w-0 flex-1">
                <div className="flex items-baseline gap-2">
                  <span className="min-w-0 flex-1 truncate text-[13px] text-ink">{n.title}</span>
                  <span className="shrink-0 text-xs text-ink-3">{ago(n.modified)}</span>
                </div>
                <div className="truncate text-xs text-ink-3">{n.excerpt || tr("note vide", "empty note")}</div>
              </div>
            </a>
          </li>
        ))}
      </ul>
    </Panel>
  );
}
