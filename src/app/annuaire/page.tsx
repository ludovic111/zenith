import { Suspense } from "react";
import type { Metadata } from "next";
import { Mail } from "lucide-react";
import { PROJECTS } from "@/lib/projects";
import { identityOf, OWNER } from "@/lib/identity";
import { plural, tr } from "@/lib/i18n";
import { date } from "@/lib/format";
import { domainInfo } from "@/lib/sources/domains";
import { agentUi } from "@/lib/agent/ui";
import { cn } from "@/lib/utils";
import { AskButton } from "@/components/agent/ask-button";
import { Empty, PageHeader, Panel, Skeleton } from "@/components/z/panel";
import { deadlineTone, IdentityCard, ownerTodos, Service, Socials, TodoList } from "@/components/identity/identity-card";
import { Copy } from "@/components/identity/copy";

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
      <PageHeader
        title={tr("Annuaire", "Directory")}
        description={counts.join(" · ")}
        action={
          PROJECTS.length > 1 ? (
            <nav className="hidden flex-wrap items-center gap-x-3 gap-y-1 text-xs text-ink-3 md:flex">
              {PROJECTS.map((p) => (
                <a key={p.id} href={`#id-${p.id}`} className="inline-flex items-center gap-1.5 hover:text-ink">
                  <span className="size-1.5 rounded-full" style={{ background: p.color }} />
                  {p.name}
                </a>
              ))}
            </nav>
          ) : undefined
        }
      />

      {!hasOwner && !PROJECTS.length ? (
        <Empty>
          {tr("L'annuaire est vide. Renseigne ", "The directory is empty. Fill in ")}
          <code className="font-mono text-xs text-ink-2">owner</code>
          {tr(" et ", " and ")}
          <code className="font-mono text-xs text-ink-2">projects[].identity</code>
          {tr(" dans ", " in ")}
          <code className="font-mono text-xs">zenith.config.json</code>.
        </Empty>
      ) : (
        <>
          <div className="grid items-start gap-4 lg:grid-cols-[1.25fr_1fr]">
            <Suspense fallback={<Skeleton className="h-56" />}>
              <Owner />
            </Suspense>
            <Suspense fallback={<Skeleton className="h-56" />}>
              <Deadlines />
            </Suspense>
          </div>

          <div className="mt-8 space-y-4">
            {PROJECTS.map((p) => (
              <div key={p.id} id={`id-${p.id}`} className="scroll-mt-16">
                <Suspense fallback={<Skeleton className="h-96" />}>
                  <IdentityCard project={p} />
                </Suspense>
              </div>
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
  const none = <p className="text-[13px] text-ink-3">—</p>;
  const head = (t: string, n: number) => (
    <div className="mb-2 flex items-baseline gap-1.5 border-b border-line pb-1.5 text-xs font-medium text-ink-3">
      {t}
      {n > 0 && <span className="font-normal tabular">{n}</span>}
    </div>
  );
  return (
    <Panel kicker={tr("Toi", "You")} title={title}>
      <div className="grid gap-x-8 gap-y-6 sm:grid-cols-2">
        <div className="min-w-0">
          {head(tr("E-mails", "Email"), OWNER.emails.length)}
          {OWNER.emails.length ? (
            <ul className="space-y-1.5">
              {OWNER.emails.map((e) => (
                <li key={e.address} className="text-[13px]">
                  <div className="flex items-center gap-2">
                    <Copy value={e.address} />
                    <a href={`mailto:${e.address}`} aria-label={tr(`Écrire à ${e.address}`, `Write to ${e.address}`)} className="shrink-0 text-ink-3 hover:text-ink">
                      <Mail className="size-3.5" />
                    </a>
                  </div>
                  {e.role && <div className="text-xs text-ink-3">{e.role}</div>}
                </li>
              ))}
            </ul>
          ) : (
            none
          )}
        </div>
        <div className="min-w-0">
          {head(tr("Réseaux", "Socials"), OWNER.socials.length)}
          {OWNER.socials.length ? <Socials socials={OWNER.socials} /> : none}
        </div>
        {OWNER.accounts.length > 0 && (
          <div className="min-w-0 sm:col-span-2">
            {head(tr("Comptes", "Accounts"), OWNER.accounts.length)}
            <ul className="grid gap-x-8 gap-y-1.5 sm:grid-cols-2">
              {OWNER.accounts.map((s) => (
                <Service key={s.label} s={s} />
              ))}
            </ul>
          </div>
        )}
      </div>
      {todo.length > 0 && (
        <div className="mt-5 border-t border-line pt-2">
          <TodoList items={todo} />
        </div>
      )}
    </Panel>
  );
}

/** Everything that expires: domain renewals and HTTPS certificates, the soonest first. */
async function Deadlines() {
  const ui = agentUi();
  const all = await Promise.all(PROJECTS.flatMap((p) => identityOf(p.id).domains.map(async (d) => ({ p, info: await domainInfo(d).catch(() => null) }))));
  type Item = { p: (typeof PROJECTS)[number]; what: string; detail: string; at: string; domain: string; renew: boolean };
  const items = all
    .flatMap(({ p, info }) =>
      info
        ? [
            info.expires && { p, what: info.domain, detail: tr(`Renouvellement${info.registrar ? ` · ${info.registrar}` : ""}`, `Renewal${info.registrar ? ` · ${info.registrar}` : ""}`), at: info.expires, domain: info.domain, renew: true },
            info.tlsExpires && { p, what: info.domain, detail: tr(`Certificat HTTPS${info.tlsIssuer ? ` · ${info.tlsIssuer}` : ""}`, `HTTPS certificate${info.tlsIssuer ? ` · ${info.tlsIssuer}` : ""}`), at: info.tlsExpires, domain: info.domain, renew: false },
          ]
        : [],
    )
    .filter((x): x is Item => !!x)
    .sort((a, b) => a.at.localeCompare(b.at));
  const soon = items.filter((i) => Math.round((new Date(i.at).getTime() - Date.now()) / 864e5) < 60).length;
  return (
    <Panel
      title={tr("Échéances", "Deadlines")}
      action={soon > 0 ? <span className="text-warn tabular">{tr(`${soon} dans moins de 60 j`, `${soon} within 60 d`)}</span> : items.length ? <span className="tabular">{items.length}</span> : undefined}
      bodyClassName={items.length ? "px-0 pb-1 pt-1" : undefined}
    >
      {items.length ? (
        <ol>
          {items.map((i) => {
            const d = Math.round((new Date(i.at).getTime() - Date.now()) / 864e5);
            return (
              <li key={i.what + i.detail} className="flex items-center gap-3 border-t border-line px-4 py-2 first:border-0">
                <span className="size-2 shrink-0 rounded-full" style={{ background: i.p.color }} title={i.p.name} />
                <div className="min-w-0 flex-1">
                  <div className="truncate text-[13px] text-ink">{i.what}</div>
                  <div className="truncate text-xs text-ink-3">{i.detail}</div>
                </div>
                {ui.enabled && i.renew && d < 60 && (
                  <AskButton
                    target="life"
                    label={tr("Préparer", "Prepare")}
                    prompt={tr(
                      `Le domaine ${i.domain} (${i.p.name}) expire le ${date(i.at, { day: "numeric", month: "long", year: "numeric" })}, dans ${d} jours. Vérifie chez ${i.detail.split(" · ")[1] ?? "le registraire"} si le renouvellement automatique est actif et si le moyen de paiement est valide (cherche aussi dans mes e-mails les avis de renouvellement ou d'échec). Prépare les étapes exactes pour le renouveler. Ne paie rien sans me demander.`,
                      `The domain ${i.domain} (${i.p.name}) expires on ${date(i.at, { day: "numeric", month: "long", year: "numeric" })}, in ${d} days. Check at ${i.detail.split(" · ")[1] ?? "the registrar"} whether auto-renew is on and the payment method is valid (also look in my emails for renewal or failure notices). Prepare the exact steps to renew it. Don't pay anything without asking me.`,
                    )}
                  />
                )}
                <div className="w-24 shrink-0 text-right">
                  <div className="text-xs text-ink-2">{date(i.at, { day: "numeric", month: "short", year: "numeric" })}</div>
                  <div className={cn("text-2xs tabular", d < 60 ? deadlineTone(d) : "text-ink-3")}>{tr(`dans ${d} j`, `in ${d} d`)}</div>
                </div>
              </li>
            );
          })}
        </ol>
      ) : (
        <p className="text-[13px] text-ink-3">
          {allDomains().length ? tr("Rien à renouveler.", "Nothing to renew.") : tr("Aucun domaine dans zenith.config.json (projects[].identity.domains).", "No domain in zenith.config.json (projects[].identity.domains).")}
        </p>
      )}
    </Panel>
  );
}

