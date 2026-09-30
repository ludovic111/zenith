import { ArrowUpRight, CircleAlert, Globe, Mail, Star } from "lucide-react";
import type { Project } from "@/lib/projects";
import { identityOf, type Field, type Link, type Social } from "@/lib/identity";
import { config } from "@/lib/config";
import { source } from "@/lib/source";
import { tr, plural } from "@/lib/i18n";
import { date } from "@/lib/format";
import { appStore, domainInfo, type DomainInfo } from "@/lib/sources/domains";
import { sponsors } from "@/lib/sources/github";
import { urgent } from "@/lib/subscriptions";
import { agentUi } from "@/lib/agent/ui";
import { cn } from "@/lib/utils";
import { AskButton } from "@/components/agent/ask-button";
import { Panel } from "@/components/z/panel";
import { Status } from "@/components/z/status";
import { BrandIcon } from "./brand-icon";
import { Copy } from "./copy";

const days = (iso: string | null) => (iso ? Math.round((new Date(iso).getTime() - Date.now()) / 864e5) : null);

/** Tone of a deadline: red under two weeks, orange under two months. */
export const deadlineTone = (d: number | null) => (d == null ? "text-ink-2" : d < 14 ? "text-bad" : d < 60 ? "text-warn" : "text-ink-2");

/** Tasks that follow from the live state: renewals coming up, failing payments. */
export async function liveTodos(p: Project, domains: DomainInfo[]) {
  const out: string[] = [];
  for (const d of domains) {
    const reg = days(d.expires);
    const cert = days(d.tlsExpires);
    if (reg != null && reg < 60)
      out.push(tr(`${d.domain} expire dans ${reg} j : renouveler chez ${d.registrar ?? "le registraire"}`, `${d.domain} expires in ${reg} d: renew at ${d.registrar ?? "the registrar"}`));
    if (cert != null && cert < 14) out.push(tr(`Certificat HTTPS de ${d.domain} : ${cert} j restants`, `HTTPS certificate of ${d.domain}: ${cert} d left`));
  }
  for (const s of urgent().filter((u) => u.project === p.id)) out.push(`${s.name}${tr(" : ", ": ")}${s.evidence}`);
  return out;
}

/** Tasks about you rather than a project: a GitHub Sponsors login set in the config but not enabled. */
export async function ownerTodos() {
  const login = config().owner.sponsors;
  if (!login) return [];
  const sp = await source(sponsors);
  return sp.ok && !sp.data.listed ? [tr(`Activer GitHub Sponsors sur ${login} pour recevoir les dons`, `Enable GitHub Sponsors on ${login} to receive donations`)] : [];
}

const host = (url?: string) => {
  try {
    return url ? new URL(url).host : null;
  } catch {
    return null;
  }
};

/** A prompt that hands one to-do to an agent: prepare everything, ask before acting. */
const todoPrompt = (task: string, about?: string) =>
  tr(
    `${about ? `${about} — ` : ""}à faire : « ${task} ». Prépare tout ce qu'il faut (étapes exactes, liens, brouillons de messages) et fais ce qui est sans risque. Demande-moi avant d'envoyer, de payer, de publier ou de supprimer quoi que ce soit. Quand c'est réglé, dis-le-moi en une phrase.`,
    `${about ? `${about} — ` : ""}to do: "${task}". Prepare everything needed (exact steps, links, draft messages) and do what is safe. Ask me before sending, paying, publishing or deleting anything. When it's done, tell me in one sentence.`,
  );

/** To-dos, each with a button that hands it to an agent when the agent is on. */
export function TodoList({ items, about, className }: { items: string[]; about?: string; className?: string }) {
  const ui = agentUi();
  if (!items.length) return null;
  return (
    <ul className={cn("divide-y divide-line", className)}>
      {items.map((t) => (
        <li key={t} className="flex items-center gap-3 py-2">
          <CircleAlert className="size-3.5 shrink-0 text-warn" />
          <span className="min-w-0 flex-1 text-[13px] text-ink-2">{t}</span>
          {ui.enabled && <AskButton className="shrink-0" target="life" label={tr("Prépare ça", "Prepare it")} prompt={todoPrompt(t, about)} />}
        </li>
      ))}
    </ul>
  );
}

/**
 * Everything tied to a project that no API can guess (names, ids, handles, services), next to
 * what can be checked live (domains, certificates, mail, the App Store). `full` shows the
 * project's name as the title (the directory); off, it sits inside the project's own page.
 */
export async function IdentityCard({ project: p, full = true }: { project: Project; full?: boolean }) {
  const id = identityOf(p.id);
  const [domains, store] = await Promise.all([
    Promise.all(id.domains.map((d) => domainInfo(d).catch(() => null))).then((l) => l.filter((d): d is DomainInfo => !!d)),
    p.appStore ? source(() => appStore(p.appStore!.id, p.appStore!.countries?.[0])) : Promise.resolve(null),
  ]);
  const todo = [...id.todo, ...(await liveTodos(p, domains))];
  const siteHost = host(p.site);

  return (
    <Panel
      title={
        full ? (
          <span className="inline-flex items-center gap-2">
            <span className="size-2 rounded-full" style={{ background: p.color }} />
            {p.name}
          </span>
        ) : (
          // Inside a project page, under its own "Identity" heading: no second title.
          tr("Noms, domaines, comptes", "Names, domains, accounts")
        )
      }
      action={
        <>
          {todo.length > 0 && (
            <span className="inline-flex items-center gap-1 text-warn">
              <CircleAlert className="size-3.5" />
              <span className="tabular">{todo.length}</span>
            </span>
          )}
          {p.site && (
            <a href={p.site} target="_blank" rel="noopener noreferrer" className="inline-flex items-center gap-0.5 hover:text-ink">
              {siteHost}
              <ArrowUpRight className="size-3" />
            </a>
          )}
        </>
      }
    >
      <div className="grid gap-x-10 gap-y-6 md:grid-cols-2">
        <Section title={tr("Noms & identifiants", "Names & ids")} n={id.names.length + id.ids.length}>
          {id.names.length + id.ids.length ? <Fields fields={[...id.names, ...id.ids]} /> : <None>{tr("Rien de noté.", "Nothing noted.")}</None>}
        </Section>

        <Section title={tr("Domaines", "Domains")} n={id.domains.length}>
          {domains.map((d) => (
            <Domain key={d.domain} d={d} />
          ))}
          {!id.domains.length &&
            (siteHost ? (
              <dl className="grid grid-cols-[7rem_1fr] gap-x-3 gap-y-1.5 text-[13px]">
                <dt className="text-xs leading-5 text-ink-3">{tr("Adresse", "Address")}</dt>
                <dd className="min-w-0">
                  <Copy value={siteHost} mono />
                  <div className="text-xs text-ink-3">{p.railway ? tr("Adresse Railway, pas de domaine propre", "Railway address, no custom domain") : tr("Pas de domaine propre", "No custom domain")}</div>
                </dd>
              </dl>
            ) : (
              <None>{tr("Aucun domaine.", "No domain.")}</None>
            ))}
        </Section>

        <Section title={tr("E-mails", "Email")} n={id.emails.length}>
          {id.emails.length ? (
            <dl className="grid grid-cols-[7rem_1fr] gap-x-3 gap-y-1.5 text-[13px]">
              {id.emails.map((e) => (
                <div key={e.address} className="contents">
                  <dt className="truncate text-xs leading-5 text-ink-3" title={e.role}>
                    {e.role || tr("Adresse", "Address")}
                  </dt>
                  <dd className="flex min-w-0 items-center gap-2">
                    <Copy value={e.address} />
                    <a href={`mailto:${e.address}`} aria-label={tr(`Écrire à ${e.address}`, `Write to ${e.address}`)} className="shrink-0 text-ink-3 hover:text-ink">
                      <Mail className="size-3.5" />
                    </a>
                  </dd>
                </div>
              ))}
            </dl>
          ) : (
            <None>{tr("Aucune adresse propre au projet.", "No project-specific address.")}</None>
          )}
        </Section>

        <Section title={tr("Réseaux sociaux", "Social accounts")} n={id.socials.length}>
          {id.socials.length ? <Socials socials={id.socials} /> : <None>{tr("Pas de compte dédié.", "No dedicated account.")}</None>}
        </Section>

        <Section title={tr("Stores & téléchargement", "Stores & downloads")} n={id.stores.length}>
          {id.stores.length ? (
            <ul className="space-y-1.5">
              {id.stores.map((s) => (
                <Service key={s.label} s={s} />
              ))}
            </ul>
          ) : (
            <None>{p.appStore ? tr("App Store ci-dessous.", "App Store below.") : tr("Application web, rien à installer.", "Web app, nothing to install.")}</None>
          )}
          {store && store.ok && store.data && (
            <div className="mt-2.5 flex flex-wrap items-center gap-x-3 gap-y-1 rounded-md bg-muted px-2.5 py-1.5 text-xs text-ink-2">
              <span>
                App Store <span className="font-medium text-ink tabular">v{store.data.version}</span>
              </span>
              <span className="text-ink-3">{tr("depuis le", "since")} {date(store.data.updated, { day: "numeric", month: "short" })}</span>
              <span className="inline-flex items-center gap-1">
                <Star className="size-3 text-ink-3" />
                {store.data.rating != null ? `${store.data.rating.toFixed(1)} (${store.data.ratings} ${plural(store.data.ratings, ["avis", "avis"], ["rating", "ratings"])})` : tr("pas encore de note", "no rating yet")}
              </span>
            </div>
          )}
        </Section>

        <Section title={tr("Services & comptes", "Services & accounts")} n={id.services.length}>
          {id.services.length ? (
            <ul className="space-y-1.5">
              {id.services.map((s) => (
                <Service key={s.label} s={s} />
              ))}
            </ul>
          ) : (
            <None>{tr("Aucun service noté.", "No service noted.")}</None>
          )}
        </Section>
      </div>

      {todo.length > 0 && (
        <div className="mt-6 border-t border-line pt-3">
          <div className="text-xs font-medium text-ink-3">
            {tr("À faire", "To do")} <span className="tabular">{todo.length}</span>
          </div>
          <TodoList items={todo} about={p.name} className="mt-1" />
        </div>
      )}
    </Panel>
  );
}

function Section({ title, n, children }: { title: string; n?: number; children: React.ReactNode }) {
  return (
    <div className="min-w-0">
      <div className="mb-2 flex items-baseline gap-1.5 border-b border-line pb-1.5 text-xs font-medium text-ink-3">
        {title}
        {!!n && <span className="font-normal tabular">{n}</span>}
      </div>
      {children}
    </div>
  );
}

const None = ({ children }: { children: React.ReactNode }) => <p className="text-[13px] text-ink-3">{children}</p>;

function Fields({ fields }: { fields: Field[] }) {
  return (
    <dl className="grid grid-cols-[7rem_1fr] gap-x-3 gap-y-1.5 text-[13px]">
      {fields.map((f) => (
        <div key={f.label + f.value} className="contents">
          <dt className="truncate text-xs leading-5 text-ink-3" title={f.label}>
            {f.label}
          </dt>
          <dd className="min-w-0">
            <Copy value={f.value} mono={f.mono} />
          </dd>
        </div>
      ))}
    </dl>
  );
}

export function Socials({ socials }: { socials: Social[] }) {
  return (
    <ul className="space-y-1.5">
      {socials.map((s) => (
        <li key={s.url} className="text-[13px]">
          <div className="flex items-center gap-2">
            <BrandIcon brand={s.network} size={14} className="shrink-0 text-ink-3" />
            <Copy value={s.handle} />
            <a href={s.url} target="_blank" rel="noopener noreferrer" aria-label={tr(`Ouvrir ${s.handle}`, `Open ${s.handle}`)} className="shrink-0 text-ink-3 hover:text-ink">
              <ArrowUpRight className="size-3.5" />
            </a>
          </div>
          {s.note && <div className="ml-[22px] text-xs text-ink-3">{s.note}</div>}
        </li>
      ))}
    </ul>
  );
}

export function Service({ s }: { s: Link }) {
  return (
    <li className="flex min-w-0 items-center gap-2 text-[13px]">
      <BrandIcon brand={s.brand} size={14} className="shrink-0 text-ink-3" />
      <span className="shrink-0 text-ink-2">{s.label}</span>
      {s.value && (
        <span className="min-w-0 flex-1">
          <Copy value={s.value} mono className="text-ink-3" />
        </span>
      )}
      {s.url && (
        <a href={s.url} target="_blank" rel="noopener noreferrer" aria-label={tr(`Ouvrir ${s.label}`, `Open ${s.label}`)} className={cn("shrink-0 text-ink-3 hover:text-ink", !s.value && "ml-auto")}>
          <ArrowUpRight className="size-3.5" />
        </a>
      )}
    </li>
  );
}

function Domain({ d }: { d: DomainInfo }) {
  const reg = days(d.expires);
  const cert = days(d.tlsExpires);
  const up = d.status != null && d.status < 400;
  const long = { day: "numeric", month: "short", year: "numeric" } as const;
  return (
    <div className="mb-4 last:mb-0">
      <div className="mb-1.5 flex items-center justify-between gap-3">
        <span className="flex min-w-0 items-center gap-2 text-[13px] font-medium text-ink">
          <Globe className="size-3.5 shrink-0 text-ink-3" />
          <Copy value={d.domain} className="text-ink" />
          <a href={`https://${d.domain}`} target="_blank" rel="noopener noreferrer" aria-label={tr(`Ouvrir ${d.domain}`, `Open ${d.domain}`)} className="shrink-0 text-ink-3 hover:text-ink">
            <ArrowUpRight className="size-3.5" />
          </a>
        </span>
        <Status health={d.status == null ? "unknown" : up ? "up" : "down"} label={d.status == null ? "—" : `HTTP ${d.status}`} />
      </div>
      <dl className="grid grid-cols-[7rem_1fr] gap-x-3 gap-y-1 text-xs">
        <dt className="text-ink-3">{tr("Registraire", "Registrar")}</dt>
        <dd className="text-ink-2">{d.registrar ?? "—"}</dd>
        <dt className="text-ink-3">{tr("Renouvellement", "Renewal")}</dt>
        <dd className={deadlineTone(reg)}>
          {d.expires ? (
            <>
              {date(d.expires, long)} <span className="tabular">· {tr(`${reg} j`, `${reg} d`)}</span>
            </>
          ) : (
            "—"
          )}
        </dd>
        <dt className="text-ink-3">{tr("Certificat", "Certificate")}</dt>
        <dd className={cert != null && cert < 14 ? "text-bad" : "text-ink-2"}>
          {d.tlsExpires ? (
            <>
              {d.tlsIssuer ? `${d.tlsIssuer} · ` : ""}
              <span className="tabular">{tr(`${cert} j`, `${cert} d`)}</span>
            </>
          ) : (
            "—"
          )}
        </dd>
        <dt className="text-ink-3">{tr("E-mail", "Email")}</dt>
        <dd className={d.mail ? "text-ink-2" : "text-ink-3"}>{d.mail ? d.mail.provider : tr("aucun MX, ne reçoit rien", "no MX, receives nothing")}</dd>
        <dt className="text-ink-3">{tr("Acheté le", "Registered")}</dt>
        <dd className="text-ink-2">{d.created ? date(d.created, long) : "—"}</dd>
      </dl>
    </div>
  );
}
