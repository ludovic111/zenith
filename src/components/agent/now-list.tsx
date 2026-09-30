"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useMemo, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import {
  ArrowUpRight,
  BellRing,
  Cake,
  Check,
  CircleAlert,
  CircleCheck,
  Clock,
  CreditCard,
  Landmark,
  LoaderCircle,
  Mail,
  RefreshCw,
  ShoppingBag,
  Sparkles,
  SquareTerminal,
  Wrench,
} from "lucide-react";
import type { NowItem, NowKind } from "@/lib/agent/now";
import { ago, date } from "@/lib/format";
import { tr } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { NEEDS_YOU, STATUS_STYLE, threadHref, threadKey, useCode, type CodeThread } from "@/components/code/store";
import { askZenith, markNow } from "./client";

/** Kind icons stay quiet; only what is broken or unpaid is red. */
const KIND: Record<NowKind, { icon: typeof Mail; bad?: boolean }> = {
  down: { icon: CircleAlert, bad: true },
  payment: { icon: CreditCard, bad: true },
  birthday: { icon: Cake },
  sale: { icon: ShoppingBag },
  reply: { icon: Mail },
  civic: { icon: Landmark },
  ci: { icon: Wrench },
  refresh: { icon: RefreshCw },
};

const SHOWN = 6;

type Row = { type: "item"; item: NowItem } | { type: "thread"; thread: CodeThread };

/** Past: "3 h ago"; ahead (a renewal, a deadline): the date. */
const when = (at: string) => {
  const t = Date.parse(at);
  if (t <= Date.now()) return ago(at);
  return new Date(t).getFullYear() === new Date().getFullYear() ? date(at) : date(at, { day: "numeric", month: "short", year: "numeric" });
};

/**
 * Now: what is waiting for you, most pressing first. One tap hands it to zenith (an agent
 * starts on it and you land in its thread); check it off, or snooze it until tomorrow.
 * Agents waiting for your answer come first.
 */
export function NowList({ items: initial, colors, projectNames }: { items: NowItem[]; colors: Record<string, string>; projectNames: Record<string, string> }) {
  const router = useRouter();
  const { snapshot } = useCode();
  const [hidden, setHidden] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<{ id: string; message: string } | null>(null);
  const [undo, setUndo] = useState<{ id: string; label: string } | null>(null);
  const [all, setAll] = useState(false);

  const threads = useMemo(() => new Map((snapshot?.threads ?? []).map((t) => [threadKey(t), t])), [snapshot]);
  const waiting = useMemo(() => (snapshot?.threads ?? []).filter((t) => t.status && NEEDS_YOU.has(t.status) && t.section !== "settled"), [snapshot]);
  const items = initial.filter((i) => !hidden.has(i.id));
  const rows: Row[] = [...waiting.map((thread) => ({ type: "thread" as const, thread })), ...items.map((item) => ({ type: "item" as const, item }))];
  const shown = all ? rows : rows.slice(0, SHOWN);
  const n = rows.length;

  async function delegate(item: NowItem) {
    setBusy(item.id);
    setError(null);
    try {
      const r = await askZenith({ nowId: item.id, source: "now" });
      router.push(r.href);
    } catch (e) {
      setError({ id: item.id, message: e instanceof Error ? e.message : String(e) });
      setBusy(null);
    }
  }

  async function file(item: NowItem, action: "done" | "snooze") {
    setHidden((h) => new Set(h).add(item.id));
    setUndo({ id: item.id, label: action === "done" ? tr("Classé", "Done") : tr("À demain", "Until tomorrow") });
    await markNow(item.id, action).catch(() => {});
    window.setTimeout(() => setUndo((u) => (u?.id === item.id ? null : u)), 6000);
  }

  async function restore(id: string) {
    setUndo(null);
    await markNow(id, "restore").catch(() => {});
    setHidden((h) => {
      const next = new Set(h);
      next.delete(id);
      return next;
    });
  }

  return (
    <section className="overflow-hidden rounded-xl border border-line bg-surface">
      <header className="flex min-h-11 items-center justify-between gap-4 px-4">
        <div className="flex items-center gap-2">
          <h2 className="text-[13px] font-semibold text-ink">{tr("Maintenant", "Now")}</h2>
          {n > 0 && <span className="rounded-md bg-muted px-1.5 py-px text-2xs font-medium text-ink-2 tabular">{n}</span>}
        </div>
        {n > 0 && (
          <p className="hidden truncate text-xs text-ink-3 md:block">
            {tr("Confie-les à zenith : un agent s'en charge et te montre tout avant d'envoyer.", "Hand them to zenith: an agent takes it on and shows you everything before sending.")}
          </p>
        )}
      </header>

      {n === 0 ? (
        <div className="flex items-center gap-3 border-t border-line px-4 py-4">
          <CircleCheck className="size-4 shrink-0 text-good" />
          <div className="min-w-0">
            <div className="text-[13px] font-medium text-ink">{tr("Tout va bien", "All clear")}</div>
            <div className="text-xs text-ink-3">{tr("Rien ne t'attend : pas de paiement en échec, de service en panne ni de message en souffrance.", "Nothing is waiting: no failed payment, no service down, no message left unanswered.")}</div>
          </div>
        </div>
      ) : (
        <ul className="divide-y divide-line border-t border-line">
          <AnimatePresence initial={false}>
            {shown.map((row) =>
              row.type === "thread" ? (
                <ThreadRow key={`t:${threadKey(row.thread)}`} thread={row.thread} />
              ) : (
                <motion.li key={row.item.id} layout="position" exit={{ opacity: 0, height: 0 }} transition={{ duration: 0.18 }} className="overflow-hidden">
                  <ItemRow
                    item={row.item}
                    color={row.item.project ? (colors[row.item.project] ?? null) : null}
                    projectName={row.item.project ? (projectNames[row.item.project] ?? row.item.project) : null}
                    thread={row.item.delegated ? (threads.get(`${row.item.delegated.environmentId}:${row.item.delegated.threadId}`) ?? null) : null}
                    busy={busy === row.item.id}
                    error={error?.id === row.item.id ? error.message : null}
                    targetName={row.item.target !== "life" ? (projectNames[row.item.target] ?? row.item.target) : null}
                    onDelegate={() => delegate(row.item)}
                    onDone={() => file(row.item, "done")}
                    onSnooze={() => file(row.item, "snooze")}
                  />
                </motion.li>
              ),
            )}
          </AnimatePresence>
        </ul>
      )}

      {(n > SHOWN || undo) && (
        <footer className="flex h-9 items-center justify-between gap-3 border-t border-line px-4 text-xs">
          {n > SHOWN ? (
            <button type="button" onClick={() => setAll((a) => !a)} className="text-ink-3 transition-colors hover:text-ink">
              {all ? tr("Moins", "Show less") : tr(`${n - SHOWN} de plus`, `${n - SHOWN} more`)}
            </button>
          ) : (
            <span />
          )}
          {undo && (
            <span className="inline-flex items-center gap-2 text-ink-2">
              <Check className="size-3.5 text-good" /> {undo.label}
              <button type="button" onClick={() => restore(undo.id)} className="text-ink-3 underline-offset-4 hover:text-ink hover:underline">
                {tr("Annuler", "Undo")}
              </button>
            </span>
          )}
        </footer>
      )}
    </section>
  );
}

function ItemRow({
  item,
  color,
  projectName,
  thread,
  busy,
  error,
  targetName,
  onDelegate,
  onDone,
  onSnooze,
}: {
  item: NowItem;
  color: string | null;
  projectName?: string | null;
  thread: CodeThread | null;
  busy: boolean;
  error: string | null;
  targetName: string | null;
  onDelegate: () => void;
  onDone: () => void;
  onSnooze: () => void;
}) {
  const k = KIND[item.kind];
  const Icon = k.icon;
  const s = thread?.status ? STATUS_STYLE[thread.status] : null;
  return (
    <div className="group flex flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2.5 transition-colors hover:bg-hover sm:flex-nowrap">
      <Icon className={cn("size-4 shrink-0", k.bad ? "text-bad" : "text-ink-3")} />
      <div className="min-w-0 flex-1 basis-[calc(100%-2rem)] sm:basis-auto">
        <div className="flex items-center gap-2">
          <span className="truncate text-[13px] text-ink">{item.title}</span>
          {color && <span className="size-1.5 shrink-0 rounded-full" style={{ background: color }} title={projectName ?? undefined} />}
        </div>
        <div className="truncate text-xs text-ink-3">
          {item.detail}
          {targetName && <> · → {targetName}</>}
        </div>
        {error && <div className="mt-1 text-xs text-bad">{error}</div>}
      </div>
      <div className="flex w-full shrink-0 items-center justify-end gap-1 sm:w-auto">
        {item.at && item.kind !== "birthday" && (
          <span className="mr-1 hidden whitespace-nowrap text-xs text-ink-3 tabular sm:inline sm:group-hover:hidden sm:group-focus-within:hidden" suppressHydrationWarning>
            {when(item.at)}
          </span>
        )}
        <div className="flex items-center sm:hidden sm:group-hover:flex sm:group-focus-within:flex">
          {item.href && (
            <IconLink href={item.href} label={tr("Ouvrir", "Open")}>
              <ArrowUpRight className="size-3.5" />
            </IconLink>
          )}
          <IconAction label={tr("Plus tard (demain)", "Later (tomorrow)")} onClick={onSnooze}>
            <Clock className="size-3.5" />
          </IconAction>
          <IconAction label={tr("C'est fait", "Done")} onClick={onDone}>
            <Check className="size-3.5" />
          </IconAction>
        </div>
        {item.delegated ? (
          <Link
            href={threadHref({ environmentId: item.delegated.environmentId, id: item.delegated.threadId })}
            className="ml-1 inline-flex h-7 items-center gap-1.5 whitespace-nowrap rounded-md border border-line bg-surface px-2 text-xs text-ink-2 transition-colors hover:bg-hover hover:text-ink"
            title={s ? tr(...s.label()) : undefined}
          >
            <span className="size-1.5 rounded-full" style={{ background: s?.dot ?? "var(--good)" }} />
            {s && NEEDS_YOU.has(thread!.status!) ? tr("À toi de voir", "Your turn") : tr("zenith s'en occupe", "zenith is on it")}
          </Link>
        ) : (
          <button
            type="button"
            onClick={onDelegate}
            disabled={busy}
            className="ml-1 inline-flex h-7 items-center gap-1.5 whitespace-nowrap rounded-md border border-line bg-surface px-2 text-xs font-medium text-ink transition-colors hover:bg-hover disabled:opacity-60"
            title={item.prompt}
          >
            {busy ? <LoaderCircle className="size-3.5 animate-spin text-primary" /> : <Sparkles className="size-3.5 text-primary" />}
            {tr("Confier", "Hand off")}
          </button>
        )}
      </div>
    </div>
  );
}

function ThreadRow({ thread }: { thread: CodeThread }) {
  const s = thread.status ? STATUS_STYLE[thread.status] : null;
  return (
    <motion.li layout="position" exit={{ opacity: 0 }} transition={{ duration: 0.18 }}>
      <Link href={threadHref(thread)} className="group flex items-center gap-3 px-4 py-2.5 transition-colors hover:bg-hover">
        {thread.status === "approval" ? <BellRing className="size-4 shrink-0 text-warn" /> : <SquareTerminal className="size-4 shrink-0 text-ink-3" />}
        <div className="min-w-0 flex-1">
          <div className="truncate text-[13px] text-ink">{thread.title}</div>
          <div className="truncate text-xs text-ink-3">
            {s ? tr(...s.label()) : tr("Agent", "Agent")}
            {thread.branch ? ` · ${thread.branch}` : ""}
          </div>
        </div>
        <span className="mr-1 hidden text-xs text-ink-3 tabular sm:inline" suppressHydrationWarning>
          {ago(thread.activityAt)}
        </span>
        <span className="inline-flex h-7 items-center gap-1.5 whitespace-nowrap rounded-md border border-line bg-surface px-2 text-xs font-medium text-ink transition-colors group-hover:bg-hover">
          <span className="size-1.5 rounded-full" style={{ background: s?.dot ?? "var(--ink-3)" }} />
          {tr("Répondre", "Answer")}
        </span>
      </Link>
    </motion.li>
  );
}

function IconAction({ label, onClick, children }: { label: string; onClick: () => void; children: React.ReactNode }) {
  return (
    <button type="button" aria-label={label} title={label} onClick={onClick} className="grid size-7 place-items-center rounded-md text-ink-3 transition-colors hover:bg-hover hover:text-ink">
      {children}
    </button>
  );
}

function IconLink({ href, label, children }: { href: string; label: string; children: React.ReactNode }) {
  return (
    <a href={href} target="_blank" rel="noopener noreferrer" aria-label={label} title={label} className="grid size-7 place-items-center rounded-md text-ink-3 transition-colors hover:bg-hover hover:text-ink">
      {children}
    </a>
  );
}
