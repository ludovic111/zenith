import { Suspense } from "react";
import type { Metadata } from "next";
import { CalendarClock, CircleAlert, Mail } from "lucide-react";
import { PROJECTS } from "@/lib/projects";
import { identityOf, OWNER } from "@/lib/identity";
import { plural, tr } from "@/lib/i18n";
import { date } from "@/lib/format";
import { domainInfo } from "@/lib/sources/domains";
import { Empty, Panel, Skeleton } from "@/components/z/panel";
import { IdentityCard, ownerTodos, Service, Socials } from "@/components/identity/identity-card";
import { Copy } from "@/components/identity/copy";
import { Meteors } from "@/components/ui/meteors";

export const dynamic = "force-dynamic";
const allDomains = () => PROJECTS.flatMap((p) => identityOf(p.id).domains);

export function generateMetadata(): Metadata {
  return { title: tr("Annuaire", "Directory") };
}

export default function Annuaire() {
  const socials = PROJECTS.reduce((a, p) => a + identityOf(p.id).socials.length, 0);
  const domains = allDomains();
  const emails = new Set([...OWNER.emails, ...PROJECTS.flatMap((p) => identityOf(p.id).emails)].map((e) => e.address));
  const hasOwner = !!(OWNER.name || OWNER.emails.length || OWNER.socials.length || OWNER.accounts.length);
  const counts = [
    `${PROJECTS.length} ${plural(PROJECTS.length, ["projet", "projets"], ["project", "projects"])}`,
    `${domains.length} ${plural(domains.length, ["domaine", "domaines"], ["domain", "domains"])}`,
    `${emails.size} ${plural(emails.size, ["adresse e-mail", "adresses e-mail"], ["email address", "email addresses"])}`,
    `${socials} ${plural(socials, ["compte social", "comptes sociaux"], ["social account", "social accounts"])}`,
  ];
  return (
    <>
      <header className="relative mb-8 overflow-hidden rounded-[2rem] border border-line px-6 py-8 sm:px-10 sm:py-10">
        <div aria-hidden className="absolute inset-0" style={{ background: "radial-gradient(110% 140% at 0% 0%, #FFD16630 0%, transparent 55%), radial-gradient(90% 130% at 100% 100%, #B18CFF30 0%, transparent 60%)" }} />
        <div aria-hidden className="absolute inset-0 overflow-hidden opacity-60"><Meteors number={10} /></div>
        <div className="relative">
          <h1 className="font-display text-5xl font-black tracking-tight sm:text-7xl">{tr("Annuaire", "Directory")}</h1>
          <p className="mt-2 max-w-2xl font-serif text-xl italic text-ink-2 sm:text-2xl">
            {tr(
              "Noms, domaines, e-mails, réseaux, stores et comptes de chaque projet, au même endroit.",
              "Names, domains, emails, socials, stores and accounts of every project, in one place.",
            )}
          </p>
          <div className="mt-6 flex flex-wrap gap-2 text-sm text-ink-2">
            {counts.map((t) => (
              <span key={t} className="rounded-full border border-white/10 bg-black/30 px-3 py-1">{t}</span>
            ))}
          </div>
        </div>
      </header>

      {!hasOwner && !PROJECTS.length ? (
        <Empty>
          {tr("L'annuaire est vide. Renseigne ", "The directory is empty. Fill in ")}
          <code className="font-mono text-xs text-sun">owner</code>
          {tr(" et ", " and ")}
          <code className="font-mono text-xs text-sun">projects[].identity</code>
          {tr(" dans ", " in ")}
          <code className="font-mono text-xs">zenith.config.json</code>.
        </Empty>
      ) : (
        <>
          <div className="grid gap-5 xl:grid-cols-[1.3fr_1fr]">
            <Suspense fallback={<Skeleton className="h-64" />}>
              <Owner />
            </Suspense>
            <Suspense fallback={<Skeleton className="h-64" />}>
              <Deadlines />
            </Suspense>
          </div>

          <div className="mt-5 space-y-5">
            {PROJECTS.map((p) => (
              <Suspense key={p.id} fallback={<Skeleton className="h-96" />}>
                <IdentityCard project={p} />
              </Suspense>
            ))}
          </div>
        </>
      )}
    </>
  );
}

async function Owner() {
  const todo = await ownerTodos();
  const title = [OWNER.name, OWNER.place].filter(Boolean).join(" · ") || tr("Toi", "You");
  const none = <p className="text-sm text-ink-3">—</p>;
  return (
    <Panel kicker={tr("Toi", "You")} title={title} accent="#FFD166">
      <div className="grid gap-8 sm:grid-cols-3">
        <div>
          <div className="mb-3 text-[11px] uppercase tracking-[0.18em] text-ink-3">{tr("E-mails", "Email")}</div>
          {OWNER.emails.length ? (
            <ul className="space-y-2.5">
              {OWNER.emails.map((e) => (
                <li key={e.address} className="text-sm">
                  <div className="flex items-center gap-2">
                    <a href={`mailto:${e.address}`} aria-label={tr(`Écrire à ${e.address}`, `Write to ${e.address}`)}><Mail className="size-3.5 text-sun" /></a>
                    <Copy value={e.address} />
                  </div>
                  {e.role && <div className="ml-5.5 text-xs text-ink-3">{e.role}</div>}
                </li>
              ))}
            </ul>
          ) : none}
        </div>
        <div>
          <div className="mb-3 text-[11px] uppercase tracking-[0.18em] text-ink-3">{tr("Réseaux perso", "Personal socials")}</div>
          {OWNER.socials.length ? <Socials socials={OWNER.socials} /> : none}
        </div>
        <div>
          <div className="mb-3 text-[11px] uppercase tracking-[0.18em] text-ink-3">{tr("Comptes", "Accounts")}</div>
          {OWNER.accounts.length ? (
            <ul className="space-y-2.5">
              {OWNER.accounts.map((s) => <Service key={s.label} s={s} />)}
            </ul>
          ) : none}
        </div>
      </div>
      {todo.length > 0 && (
        <ul className="mt-6 space-y-1.5 rounded-2xl border border-warn/25 bg-warn/[0.04] p-4">
          {todo.map((t) => (
            <li key={t} className="flex gap-2 text-sm text-ink-2">
              <CircleAlert className="mt-0.5 size-3.5 shrink-0 text-warn" />
              {t}
            </li>
          ))}
        </ul>
      )}
    </Panel>
  );
}

/** Everything that expires: domain renewals and HTTPS certificates. */
async function Deadlines() {
  const all = await Promise.all(
    PROJECTS.flatMap((p) => identityOf(p.id).domains.map(async (d) => ({ p, info: await domainInfo(d).catch(() => null) }))),
  );
  const items = all
    .flatMap(({ p, info }) =>
      info
        ? [
            info.expires && { p, what: tr(`Renouveler ${info.domain}`, `Renew ${info.domain}`), detail: info.registrar ?? "", at: info.expires },
            info.tlsExpires && { p, what: tr(`Certificat de ${info.domain}`, `Certificate of ${info.domain}`), detail: tr("renouvelé automatiquement", "renewed automatically"), at: info.tlsExpires },
          ]
        : [],
    )
    .filter((x): x is { p: (typeof PROJECTS)[number]; what: string; detail: string; at: string } => !!x)
    .sort((a, b) => a.at.localeCompare(b.at));
  return (
    <Panel kicker={tr("Échéances", "Deadlines")} title={tr("Ce qui expire", "What expires")} accent="#FFD166">
      <ol className="space-y-3">
        {items.map((i) => {
          const d = Math.round((new Date(i.at).getTime() - Date.now()) / 864e5);
          return (
            <li key={i.what} className="flex items-center gap-3 text-sm">
              <CalendarClock className={d < 30 ? "size-4 shrink-0 text-warn" : "size-4 shrink-0 text-ink-3"} />
              <span className="size-2 shrink-0 rounded-full" style={{ background: i.p.glow }} />
              <div className="min-w-0 flex-1">
                <div className="truncate text-ink">{i.what}</div>
                <div className="truncate text-xs text-ink-3">{i.detail}</div>
              </div>
              <div className="shrink-0 text-right">
                <div className="text-xs text-ink-2">{date(i.at, { day: "numeric", month: "short", year: "numeric" })}</div>
                <div className={`font-mono text-[11px] ${d < 30 ? "text-warn" : "text-ink-3"}`}>{tr(`dans ${d} j`, `in ${d} d`)}</div>
              </div>
            </li>
          );
        })}
        {!items.length && (
          <li className="text-sm text-ink-3">
            {allDomains().length ? tr("Rien à renouveler.", "Nothing to renew.") : tr("Aucun domaine dans zenith.config.json (projects[].identity.domains).", "No domain in zenith.config.json (projects[].identity.domains).")}
          </li>
        )}
      </ol>
    </Panel>
  );
}
