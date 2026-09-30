"use client";

import Link from "next/link";
import { useMemo } from "react";
import { tr } from "@/lib/i18n";
import { NEEDS_YOU, STATUS_STYLE, sameDir, threadHref, useCode } from "@/components/code/store";

/**
 * A project's zenith code threads at a glance: how many need you, else how many are
 * working. Matched by folder, like the sidebar. Renders nothing when all is quiet.
 */
export function ProjectThreads({ dir }: { dir: string | null }) {
  const { snapshot } = useCode();
  const state = useMemo(() => {
    if (!dir || !snapshot) return null;
    const ids = new Set(snapshot.projects.filter((p) => sameDir(p.workspaceRoot, dir)).map((p) => `${p.environmentId}:${p.id}`));
    const mine = snapshot.threads.filter((t) => ids.has(`${t.environmentId}:${t.projectId}`) && t.section !== "settled");
    const needs = mine.filter((t) => t.status && NEEDS_YOU.has(t.status));
    const working = mine.filter((t) => t.status === "working" || t.status === "connecting");
    return { needs, working };
  }, [dir, snapshot]);
  if (!state) return null;
  const { needs, working } = state;
  const list = needs.length ? needs : working;
  if (!list.length) return null;
  const first = list[0];
  const dot = STATUS_STYLE[first.status!].dot;
  const n = list.length;
  return (
    <Link
      href={threadHref(first)}
      title={first.title}
      className="relative z-10 inline-flex h-6 items-center gap-1.5 whitespace-nowrap rounded-md border border-line bg-surface px-1.5 text-2xs text-ink-2 transition-colors hover:bg-hover hover:text-ink"
    >
      <span className="size-1.5 rounded-full" style={{ background: dot }} />
      {needs.length ? tr(`${n} à voir`, `${n} to review`) : tr(`${n} en cours`, `${n} working`)}
    </Link>
  );
}
