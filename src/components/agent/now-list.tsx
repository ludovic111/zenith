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
import { ago } from "@/lib/format";
import { plural, tr } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import { NEEDS_YOU, STATUS_STYLE, threadHref, threadKey, useCode, type CodeThread } from "@/components/code/store";
import { askZenith, markNow } from "./client";

const KIND: Record<NowKind, { icon: typeof Mail; color: string }> = {
  down: { icon: CircleAlert, color: "#fb5a6b" },
  payment: { icon: CreditCard, color: "#fb5a6b" },
  birthday: { icon: Cake, color: "#FF6FB5" },
  sale: { icon: ShoppingBag, color: "#FFD166" },
  reply: { icon: Mail, color: "#8FA0FF" },
  civic: { icon: Landmark, color: "#FDBA74" },
  ci: { icon: Wrench, color: "#7dd3fc" },
  refresh: { icon: RefreshCw, color: "#34d399" },
};

const SHOWN = 5;

type Row =
  | { type: "item"; item: NowItem }
  | { type: "thread"; thread: CodeThread };

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
    <section className="relative overflow-hidden rounded-3xl border border-line bg-white/[0.035] shadow-[inset_0_1px_0_0_rgb(255_255_255/0.06)] backdrop-blur-md">
      <header className="flex items-end justify-between gap-4 px-5 pt-5">
        <div>
          <div className="mb-1 text-[11px] font-medium uppercase tracking-[0.2em] text-ink-3">{tr("Maintenant", "Now")}</div>
          <h2 className="font-display text-sm font-medium tracking-wide text-ink">
            {n === 0
              ? tr("Rien ne t'attend", "Nothing is waiting")
              : tr(`${n} chose${n > 1 ? "s" : ""} t'attend${n > 1 ? "ent" : ""}`, `${n} ${plural(n, ["thing", "things"], ["thing", "things"])} waiting for you`)}
          </h2>
        </div>
        <p className="hidden text-xs text-ink-3 sm:block">
          <Sparkles className="mr-1 inline size-3 text-sun" />
          {tr("Confie-les à zenith : un agent s'en charge et te montre tout avant d'envoyer.", "Hand them to zenith: an agent takes it on and shows you everything before sending.")}
        </p>
      </header>

      {n === 0 ? (
        <div className="px-5 pb-6 pt-4 font-serif text-xl italic text-ink-2">{tr("Ciel dégagé. Profite.", "Clear skies. Enjoy.")}</div>
      ) : (
        <ul className="px-3 pb-3 pt-3">
          <AnimatePresence initial={false}>
            {shown.map((row) =>
              row.type === "thread" ? (
                <ThreadRow key={`t:${threadKey(row.thread)}`} thread={row.thread} />
              ) : (
                <motion.li
                  key={row.item.id}
                  layout
                  initial={{ opacity: 0, y: 6 }}
                  animate={{ opacity: 1, y: 0 }}
                  exit={{ opacity: 0, height: 0, marginTop: 0, marginBottom: 0 }}
                  transition={{ duration: 0.25 }}
                >
                  <ItemRow
                    item={row.item}
                    color={(row.item.project && colors[row.item.project]) || KIND[row.item.kind].color}
                    thread={row.item.delegated ? threads.get(`${row.item.delegated.environmentId}:${row.item.delegated.threadId}`) ?? null : null}
                    busy={busy === row.item.id}
                    error={error?.id === row.item.id ? error.message : null}
                    targetName={row.item.target !== "life" ? projectNames[row.item.target] ?? row.item.target : null}
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
        <footer className="flex items-center justify-between gap-3 border-t border-line px-5 py-2.5 text-xs">
          {n > SHOWN ? (
            <button type="button" onClick={() => setAll((a) => !a)} className="text-ink-3 transition hover:text-ink">
              {all ? tr("Moins", "Show less") : tr(`${n - SHOWN} de plus`, `${n - SHOWN} more`)}
            </button>
          ) : (
            <span />
          )}
          <AnimatePresence>
            {undo && (
              <motion.span initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }} className="inline-flex items-center gap-2 text-ink-2">
                <Check className="size-3.5 text-good" /> {undo.label}
                <button type="button" onClick={() => restore(undo.id)} className="text-ink-3 underline-offset-4 hover:text-ink hover:underline">
                  {tr("Annuler", "Undo")}
                </button>
              </motion.span>
            )}
          </AnimatePresence>
        </footer>
      )}
    </section>
  );
}

function ItemRow({
  item,
  color,
  thread,
  busy,
  error,
  targetName,
  onDelegate,
  onDone,
  onSnooze,
}: {
  item: NowItem;
  color: string;
  thread: CodeThread | null;
  busy: boolean;
  error: string | null;
  targetName: string | null;
  onDelegate: () => void;
  onDone: () => void;
  onSnooze: () => void;
}) {
  const Icon = KIND[item.kind].icon;
  const s = thread?.status ? STATUS_STYLE[thread.status] : null;
  return (
    <div className="group flex flex-wrap items-center gap-x-3 gap-y-2 rounded-2xl px-2 py-2.5 transition hover:bg-white/[0.03] sm:flex-nowrap">
      <span className="grid size-9 shrink-0 place-items-center rounded-xl" style={{ background: `${color}1f`, color }}>
        <Icon className="size-4" />
      </span>
      <div className="min-w-0 flex-1 basis-[calc(100%-3rem)] sm:basis-auto">
        <div className="truncate text-sm text-ink">{item.title}</div>
        <div className="truncate text-xs text-ink-3">
          {item.detail}
          {item.at && item.kind !== "birthday" && <> · {ago(item.at)}</>}
          {targetName && <> · → {targetName}</>}
        </div>
        {error && <div className="mt-1 text-xs text-bad">{error}</div>}
      </div>
      <div className="flex w-full shrink-0 items-center justify-end gap-1 sm:w-auto">
        <div className="flex items-center gap-0.5 opacity-100 transition sm:opacity-0 sm:group-hover:opacity-100 sm:focus-within:opacity-100">
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
            className="ml-1 inline-flex items-center gap-2 rounded-full border border-white/10 bg-white/[0.04] px-3 py-1.5 text-xs text-ink-2 transition hover:border-white/20 hover:text-ink"
            title={s ? tr(...s.label()) : undefined}
          >
            <span className={cn("size-1.5 rounded-full", s?.pulse && "animate-pulse")} style={{ background: s?.dot ?? "var(--good)" }} />
            {s && NEEDS_YOU.has(thread!.status!) ? tr("À toi de voir", "Your turn") : tr("zenith s'en occupe", "zenith is on it")}
          </Link>
        ) : (
          <button
            type="button"
            onClick={onDelegate}
            disabled={busy}
            className="ml-1 inline-flex items-center gap-1.5 rounded-full border border-sun/30 bg-sun/[0.08] px-3 py-1.5 text-xs font-medium text-ink transition hover:border-sun/60 hover:bg-sun/[0.14] disabled:opacity-70"
            title={item.prompt}
          >
            {busy ? <LoaderCircle className="size-3.5 animate-spin text-sun" /> : <Sparkles className="size-3.5 text-sun" />}
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
    <motion.li layout initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }}>
      <Link href={threadHref(thread)} className="group flex items-center gap-3 rounded-2xl px-2 py-2.5 transition hover:bg-white/[0.03]">
        <span className="relative grid size-9 shrink-0 place-items-center rounded-xl bg-white/[0.05] text-ink-2">
          {thread.status === "approval" ? <BellRing className="size-4 text-[#fcd34d]" /> : <SquareTerminal className="size-4" />}
        </span>
        <div className="min-w-0 flex-1">
          <div className="truncate text-sm text-ink">{thread.title}</div>
          <div className="truncate text-xs text-ink-3">
            {s ? tr(...s.label()) : ""} · {ago(thread.activityAt)}
          </div>
        </div>
        <span className="ml-1 inline-flex items-center gap-2 rounded-full border border-white/10 bg-white/[0.04] px-3 py-1.5 text-xs text-ink transition group-hover:border-white/20">
          <span className="size-1.5 rounded-full" style={{ background: s?.dot ?? "var(--ink-3)" }} />
          {tr("Répondre", "Answer")}
        </span>
      </Link>
    </motion.li>
  );
}

function IconAction({ label, onClick, children }: { label: string; onClick: () => void; children: React.ReactNode }) {
  return (
    <button type="button" aria-label={label} title={label} onClick={onClick} className="grid size-8 place-items-center rounded-full text-ink-3 transition hover:bg-white/[0.07] hover:text-ink">
      {children}
    </button>
  );
}

function IconLink({ href, label, children }: { href: string; label: string; children: React.ReactNode }) {
  return (
    <a href={href} target="_blank" rel="noopener noreferrer" aria-label={label} title={label} className="grid size-8 place-items-center rounded-full text-ink-3 transition hover:bg-white/[0.07] hover:text-ink">
      {children}
    </a>
  );
}
