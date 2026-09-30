import { BellRing, CalendarClock, CircleAlert, Landmark, Mail, Package, Square, Tag } from "lucide-react";
import { source } from "@/lib/source";
import { plural, tr } from "@/lib/i18n";
import { ago, date, money } from "@/lib/format";
import { life } from "@/lib/sources/life";
import { apple } from "@/lib/sources/apple";
import { todo } from "@/lib/sources/obsidian";
import { urgent, SUBSCRIPTIONS } from "@/lib/subscriptions";
import { refreshLifePrompt } from "@/lib/agent/tasks";
import { AskButton } from "@/components/agent/ask-button";
import { Empty, Panel } from "@/components/z/panel";
import { Counted, Row, RowGroup } from "./rows";
import { salesByItem } from "./events";
import { buyersPrompt, paperworkPrompt, paymentPrompt, replyPrompt } from "./prompts";
import { nowMs } from "./time";

const DAY = 864e5;

/** Everything that waits for you in your life, most pressing first, each with its one-click agent. */
export async function Waiting({ agent }: { agent: boolean }) {
  const [snap, ap, td] = await Promise.all([source(life), source(apple), source(todo)]);
  const l = snap.ok ? snap.data : null;
  const a = ap.ok ? ap.data : null;
  const now = nowMs();
  const soon = (iso: string | null | undefined, days: number) => !!iso && new Date(iso).getTime() > now && new Date(iso).getTime() - now < days * DAY;

  const payments = urgent();
  const replies = [...(l?.inbox.needsReply ?? [])].sort((x, y) => x.date.localeCompare(y.date));
  const sales = salesByItem(l);
  const parcels = l?.deliveries ?? [];
  const civic = [...(l?.civic ?? [])].sort((x, y) => (y.date ?? "").localeCompare(x.date ?? ""));
  const reminders = (a?.reminders.items ?? []).filter((r) => !r.due || new Date(r.due).getTime() < now + 7 * DAY).sort((x, y) => (x.due ?? "9999").localeCompare(y.due ?? "9999"));
  const tasks = td.ok && td.data ? td.data.items.filter((i) => !i.done) : [];
  const renewals = SUBSCRIPTIONS.filter((s) => s.status === "active" && soon(s.next_renewal, 10) && (s.amount ?? 0) > 0).sort((x, y) => x.next_renewal!.localeCompare(y.next_renewal!));
  const total = payments.length + replies.length + sales.length + parcels.length + civic.length + reminders.length + tasks.length + renewals.length;

  const mailMeta = [
    l ? `${l.inbox.unread} ${plural(l.inbox.unread, ["non lu", "non lus"], ["unread", "unread"])}` : null,
    l?.inbox.unreadImportant ? `${l.inbox.unreadImportant} ${plural(l.inbox.unreadImportant, ["important", "importants"], ["important", "important"])}` : null,
  ]
    .filter(Boolean)
    .join(" · ");

  return (
    <Panel title={<Counted count={total}>{tr("Ce qui t'attend", "What's waiting")}</Counted>} bodyClassName="p-0 pt-2">
      {!l && (
        <div className="px-4 pb-4">
          <Empty>
            <span>
              {tr("E-mails, colis, ventes et administratif arrivent avec le relevé de ta boîte mail.", "Email, parcels, sales and paperwork come with the capture of your inbox.")}
              {agent && (
                <span className="mt-3 block">
                  <AskButton prompt={refreshLifePrompt()} target="life" label={tr("Relever maintenant", "Capture now")} />
                </span>
              )}
            </span>
          </Empty>
        </div>
      )}
      {l && total === 0 && (
        <div className="px-4 pb-4">
          <Empty>{tr("Rien ne t'attend. Vraiment rien.", "Nothing is waiting. Really nothing.")}</Empty>
        </div>
      )}

      {payments.length > 0 && (
        <RowGroup title={tr("Paiements en échec", "Failed payments")} count={payments.length}>
          {payments.map((s) => (
            <Row
              key={s.name + s.evidence}
              icon={CircleAlert}
              tone="text-bad"
              title={s.name}
              href={s.manage_url}
              meta={s.evidence}
              action={agent && <AskButton prompt={paymentPrompt(s)} target="life" label={tr("Comment régler ?", "How to fix?")} />}
            />
          ))}
        </RowGroup>
      )}

      {replies.length > 0 && (
        <RowGroup title={tr("E-mails à répondre", "Emails to answer")} count={replies.length} action={mailMeta}>
          {replies.map((m) => (
            <Row
              key={m.link + m.subject}
              icon={Mail}
              title={m.from}
              href={m.link}
              meta={
                <>
                  <span className="text-ink-2">{m.subject}</span> · {m.why}
                </>
              }
              aside={ago(m.date)}
              action={agent && <AskButton prompt={replyPrompt(m)} target="life" label={tr("Prépare une réponse", "Draft a reply")} />}
            />
          ))}
        </RowGroup>
      )}

      {sales.length > 0 && (
        <RowGroup title={tr("Ventes", "Sales")} count={sales.length}>
          {sales.map((s) => (
            <Row
              key={s.item}
              icon={Tag}
              title={s.item}
              href={s.links[0]}
              meta={tr(
                `${s.platforms.join(", ")} · ${s.messages} message${s.messages > 1 ? "s" : ""} d'acheteurs`,
                `${s.platforms.join(", ")} · ${s.messages} buyer message${s.messages === 1 ? "" : "s"}`,
              )}
              aside={ago(s.last)}
              action={agent && s.messages > 0 && <AskButton prompt={buyersPrompt(s)} target="life" label={tr("Réponds aux acheteurs", "Answer the buyers")} />}
            />
          ))}
        </RowGroup>
      )}

      {civic.length > 0 && (
        <RowGroup title={tr("Administratif", "Paperwork")} count={civic.length}>
          {civic.map((c) => (
            <Row
              key={c.link + c.title}
              icon={Landmark}
              tone={soon(c.date, 7) ? "text-warn" : undefined}
              title={c.title}
              href={c.link}
              meta={c.note}
              aside={c.date ? date(c.date, { day: "numeric", month: "short" }) : null}
              action={agent && <AskButton prompt={paperworkPrompt(c)} target="life" label={tr("Occupe-toi de ça", "Handle this")} />}
            />
          ))}
        </RowGroup>
      )}

      {parcels.length > 0 && (
        <RowGroup title={tr("Colis en route", "Parcels on the way")} count={parcels.length}>
          {parcels.map((d) => (
            <Row
              key={d.link + d.merchant}
              icon={Package}
              title={d.merchant}
              href={d.link}
              meta={d.status}
              aside={d.eta ? date(d.eta, { weekday: "short", day: "numeric", month: "short" }) : null}
            />
          ))}
        </RowGroup>
      )}

      {reminders.length > 0 && (
        <RowGroup title={tr("Rappels", "Reminders")} count={reminders.length}>
          {reminders.slice(0, 8).map((r) => {
            const late = !!r.due && new Date(r.due).getTime() < now;
            return (
              <Row
                key={r.title + r.due}
                icon={BellRing}
                tone={late ? "text-bad" : undefined}
                title={r.title}
                meta={r.list}
                aside={r.due ? <span className={late ? "text-bad" : undefined}>{date(r.due, { day: "numeric", month: "short" })}</span> : null}
              />
            );
          })}
        </RowGroup>
      )}

      {tasks.length > 0 && td.ok && td.data && (
        <RowGroup
          title={tr("Tâches", "Tasks")}
          count={tasks.length}
          action={
            <a href={td.data.note.url} className="hover:text-ink">
              Obsidian
            </a>
          }
        >
          {tasks.slice(0, 5).map((t) => (
            <Row key={t.text} icon={Square} title={<span title={t.text}>{t.text}</span>} href={td.ok && td.data ? td.data.note.url : undefined} />
          ))}
          {tasks.length > 5 && (
            <li className="flex h-8 items-center px-4 pl-11 text-xs text-ink-3">
              <a href={td.data.note.url} className="hover:text-ink">
                {tr(`et ${tasks.length - 5} autre${tasks.length - 5 > 1 ? "s" : ""} dans Obsidian`, `and ${tasks.length - 5} more in Obsidian`)}
              </a>
            </li>
          )}
        </RowGroup>
      )}

      {renewals.length > 0 && (
        <RowGroup title={tr("Prélèvements à venir", "Upcoming charges")} count={renewals.length}>
          {renewals.map((s) => (
            <Row
              key={s.name}
              icon={CalendarClock}
              title={s.name}
              href="/abonnements"
              aside={
                <>
                  <span className="text-ink-2">{money(s.amount!, s.currency)}</span> · {date(s.next_renewal!, { day: "numeric", month: "short" })}
                </>
              }
            />
          ))}
        </RowGroup>
      )}

      {l && l.notes.length > 0 && (
        <div className="border-t border-line px-4 py-3">
          <div className="mb-1.5 text-xs font-medium text-ink-2">{tr("À retenir", "Worth knowing")}</div>
          <ul className="space-y-1 text-xs leading-relaxed text-ink-3">
            {l.notes.map((n) => (
              <li key={n} className="flex gap-2">
                <span className="mt-[7px] size-1 shrink-0 rounded-full bg-ink-3" />
                <span>{n}</span>
              </li>
            ))}
          </ul>
        </div>
      )}
    </Panel>
  );
}
