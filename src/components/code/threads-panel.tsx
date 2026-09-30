"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { MessageSquarePlus } from "lucide-react";
import { Panel } from "@/components/z/panel";
import { tr } from "@/lib/i18n";
import { codeNavigate, sameDir, STATUS_STYLE, threadHref, threadKey, useCode } from "./store";

const SHOWN = 8;

/** A project's zenith code threads, live, on its dashboard page. Hidden until it has some. */
export function CodeThreadsPanel({ dir, className }: { dir: string; className?: string }) {
  const router = useRouter();
  const { snapshot, ready } = useCode();
  const project = snapshot?.projects.find((p) => sameDir(p.workspaceRoot, dir));
  if (!project) return null;
  const threads = snapshot!.threads.filter((t) => t.environmentId === project.environmentId && t.projectId === project.id && t.section !== "settled");
  if (!threads.length) return null;

  return (
    <div className={className}>
      <Panel
        title={
          <>
            {tr("Threads zenith code", "zenith code threads")} <span className="font-normal text-ink-3 tabular">{threads.length}</span>
          </>
        }
        bodyClassName="px-2 pb-2 pt-1"
        action={
          ready && (
            <button
              type="button"
              onClick={() => {
                router.push("/code");
                codeNavigate({ to: "new-thread", environmentId: project.environmentId, projectId: project.id });
              }}
              className="inline-flex h-7 items-center gap-1.5 rounded-md border border-line bg-surface px-2 text-xs font-medium text-ink-2 transition-colors hover:bg-hover hover:text-ink"
            >
              <MessageSquarePlus className="size-3.5" /> {tr("Nouveau thread", "New thread")}
            </button>
          )
        }
      >
        <ul className="grid gap-x-2 sm:grid-cols-2">
          {threads.slice(0, SHOWN).map((t) => {
            const s = t.status ? STATUS_STYLE[t.status] : null;
            return (
              <li key={threadKey(t)}>
                <Link href={threadHref(t)} className="flex h-8 items-center gap-2.5 rounded-md px-2 text-[13px] text-ink-2 transition-colors hover:bg-hover hover:text-ink">
                  <span className="size-1.5 shrink-0 rounded-full" style={{ background: s?.dot ?? "var(--ink-3)" }} />
                  <span className="min-w-0 flex-1 truncate">{t.title}</span>
                  {s ? (
                    <span className="shrink-0 text-xs text-ink-3">{tr(...s.label())}</span>
                  ) : (
                    t.branch && <span className="max-w-32 shrink-0 truncate font-mono text-2xs text-ink-3">{t.branch}</span>
                  )}
                </Link>
              </li>
            );
          })}
        </ul>
      </Panel>
    </div>
  );
}
