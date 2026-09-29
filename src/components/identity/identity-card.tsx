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
import { Panel } from "@/components/z/panel";
import { Status } from "@/components/z/status";
import { BrandIcon } from "./brand-icon";
import { Copy } from "./copy";

const days = (iso: string | null) => (iso ? Math.round((new Date(iso).getTime() - Date.now()) / 864e5) : null);

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
      kicker={tr("Carte d'identité", "Identity card")}
      title={full ? <span className="inline-flex items-center gap-2"><span>{p.emoji}</span>{p.name}</span> : tr("Tout ce qui est lié au projet", "Everything tied to the project")}
      accent={p.glow}
    >
      <div className="grid gap-x-10 gap-y-8 md:grid-cols-2 xl:grid-cols-3">
        <Section title={tr("Noms & identifiants", "Names & ids")}>
          {id.names.length + id.ids.length ? <Fields fields={[...id.names, ...id.ids]} /> : <p className="text-sm text-ink-3">{tr("Rien de noté.", "Nothing noted.")}</p>}
        </Section>

        <Section title={tr("Domaines & adresses", "Domains & addresses")}>
          {domains.map((d) => <Domain key={d.domain} d={d} />)}
          {!id.domains.length &&
            (siteHost ? (
              <div className="text-sm">
                <div className="flex items-center gap-2 text-ink-2">
                  <Globe className="size-3.5 text-ink-3" />
                  <Copy value={siteHost} mono />
                </div>
                <div className="mt-1 text-xs text-ink-3">
                  {p.railway ? tr("Adresse Railway, pas de domaine propre", "Railway address, no custom domain") : tr("Pas de domaine propre", "No custom domain")}
                </div>
              </div>
            ) : (
              <p className="text-sm text-ink-3">{tr("Aucun domaine.", "No domain.")}</p>
            ))}
        </Section>

        <Section title={tr("E-mails", "Email")}>
          {id.emails.length ? (
            <ul className="space-y-2.5">
              {id.emails.map((e) => (
                <li key={e.address} className="text-sm">
                  <div className="flex items-center gap-2">
                    <a href={`mailto:${e.address}`} aria-label={tr(`Écrire à ${e.address}`, `Write to ${e.address}`)}><Mail className="size-3.5" style={{ color: p.glow }} /></a>
                    <Copy value={e.address} />
                  </div>
                  <div className="ml-5.5 text-xs text-ink-3">{e.role}</div>
                </li>
              ))}
            </ul>
          ) : (
            <p className="text-sm text-ink-3">{tr("Aucune adresse propre au projet.", "No project-specific address.")}</p>
          )}
          {domains.map((d) => (
            <p key={d.domain} className="mt-2 text-xs text-ink-3">
              {d.domain}{tr(" : ", ": ")}{d.mail ? d.mail.provider : tr("ne reçoit pas d'e-mails (aucun MX)", "receives no email (no MX)")}
            </p>
          ))}
        </Section>

        <Section title={tr("Réseaux sociaux", "Social accounts")}>
          {id.socials.length ? <Socials socials={id.socials} /> : <p className="text-sm text-ink-3">{tr("Pas de compte dédié.", "No dedicated account.")}</p>}
        </Section>

        <Section title={tr("Stores & téléchargement", "Stores & downloads")}>
          {id.stores.length ? (
            <ul className="space-y-2.5">
              {id.stores.map((s) => <Service key={s.label} s={s} />)}
            </ul>
          ) : (
            <p className="text-sm text-ink-3">{p.appStore ? tr("Voir l'App Store ci-dessous.", "See the App Store below.") : tr("Application web, rien à installer.", "Web app, nothing to install.")}</p>
          )}
          {store && store.ok && store.data && (
            <div className="mt-3 flex flex-wrap items-center gap-x-4 gap-y-1 rounded-xl border border-line bg-black/20 px-3 py-2 text-xs text-ink-2">
              <span>{tr("En ligne : ", "Live: ")}<span className="font-mono text-ink">v{store.data.version}</span></span>
              <span>{tr("depuis le", "since")} {date(store.data.updated, { day: "numeric", month: "long" })}</span>
              <span className="inline-flex items-center gap-1">
                <Star className="size-3 text-sun" />
                {store.data.rating != null
                  ? `${store.data.rating.toFixed(1)} (${store.data.ratings} ${plural(store.data.ratings, ["avis", "avis"], ["rating", "ratings"])})`
                  : tr("pas encore de note", "no rating yet")}
              </span>
            </div>
          )}
        </Section>

        <Section title={tr("Services & comptes", "Services & accounts")}>
          {id.services.length ? (
            <ul className="space-y-2.5">
              {id.services.map((s) => <Service key={s.label} s={s} />)}
            </ul>
          ) : (
            <p className="text-sm text-ink-3">{tr("Aucun service noté.", "No service noted.")}</p>
          )}
        </Section>
      </div>

      {todo.length > 0 && (
        <div className="mt-8 rounded-2xl border border-warn/25 bg-warn/[0.04] p-4">
          <div className="mb-2 text-[11px] uppercase tracking-[0.18em] text-warn">{tr("À faire", "To do")}</div>
          <ul className="space-y-1.5">
            {todo.map((t) => (
              <li key={t} className="flex gap-2 text-sm text-ink-2">
                <CircleAlert className="mt-0.5 size-3.5 shrink-0 text-warn" />
                {t}
              </li>
            ))}
          </ul>
        </div>
      )}
    </Panel>
  );
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="min-w-0">
      <div className="mb-3 text-[11px] uppercase tracking-[0.18em] text-ink-3">{title}</div>
      {children}
    </div>
  );
}

function Fields({ fields }: { fields: Field[] }) {
  return (
    <dl className="space-y-2">
      {fields.map((f) => (
        <div key={f.label + f.value} className="grid grid-cols-[minmax(0,9rem)_1fr] items-baseline gap-3 text-sm">
          <dt className="truncate text-xs text-ink-3">{f.label}</dt>
          <dd className="min-w-0"><Copy value={f.value} mono={f.mono} /></dd>
        </div>
      ))}
    </dl>
  );
}

export function Socials({ socials }: { socials: Social[] }) {
  return (
    <ul className="space-y-2.5">
      {socials.map((s) => (
        <li key={s.url} className="text-sm">
          <a href={s.url} target="_blank" rel="noopener noreferrer" className="group inline-flex items-center gap-2 text-ink-2 hover:text-ink">
            <BrandIcon brand={s.network} size={15} />
            <span>{s.handle}</span>
            <ArrowUpRight className="size-3 opacity-0 transition group-hover:opacity-70" />
          </a>
          {s.note && <div className="ml-6 text-xs text-ink-3">{s.note}</div>}
        </li>
      ))}
    </ul>
  );
}

export function Service({ s }: { s: Link }) {
  const body = (
    <>
      <BrandIcon brand={s.brand} size={15} className="shrink-0" />
      <span className="min-w-0">
        <span className="block truncate text-ink-2 group-hover:text-ink">{s.label}</span>
        {s.value && <span className="block truncate font-mono text-[11px] text-ink-3">{s.value}</span>}
      </span>
      {s.url && <ArrowUpRight className="ml-auto size-3 shrink-0 opacity-0 transition group-hover:opacity-70" />}
    </>
  );
  return (
    <li className="text-sm">
      {s.url ? (
        <a href={s.url} target="_blank" rel="noopener noreferrer" className="group flex items-center gap-2.5">{body}</a>
      ) : (
        <div className="flex items-center gap-2.5">{body}</div>
      )}
    </li>
  );
}

function Domain({ d }: { d: DomainInfo }) {
  const reg = days(d.expires);
  const cert = days(d.tlsExpires);
  const up = d.status != null && d.status < 400;
  return (
    <div className="mb-4 text-sm last:mb-0">
      <div className="flex items-center justify-between gap-3">
        <a href={`https://${d.domain}`} target="_blank" rel="noopener noreferrer" className="inline-flex items-center gap-2 font-medium text-ink hover:underline">
          <Globe className="size-3.5 text-ink-3" />
          {d.domain}
        </a>
        <Status health={d.status == null ? "unknown" : up ? "up" : "down"} label={d.status == null ? "?" : String(d.status)} />
      </div>
      <dl className="mt-2 grid grid-cols-[6.5rem_1fr] gap-x-3 gap-y-1 text-xs">
        <dt className="text-ink-3">{tr("Registraire", "Registrar")}</dt>
        <dd className="text-ink-2">{d.registrar ?? "—"}</dd>
        <dt className="text-ink-3">{tr("Renouvellement", "Renewal")}</dt>
        <dd className={reg != null && reg < 60 ? "text-warn" : "text-ink-2"}>
          {d.expires ? `${date(d.expires, { day: "numeric", month: "long", year: "numeric" })} · ${reg} ${tr("j", "d")}` : "—"}
        </dd>
        <dt className="text-ink-3">{tr("Certificat", "Certificate")}</dt>
        <dd className={cert != null && cert < 14 ? "text-warn" : "text-ink-2"}>
          {d.tlsExpires ? `${d.tlsIssuer ? `${d.tlsIssuer} · ` : ""}${cert} ${tr("j", "d")}` : "—"}
        </dd>
        <dt className="text-ink-3">{tr("Acheté le", "Registered")}</dt>
        <dd className="text-ink-2">{d.created ? date(d.created, { day: "numeric", month: "long", year: "numeric" }) : "—"}</dd>
      </dl>
    </div>
  );
}
