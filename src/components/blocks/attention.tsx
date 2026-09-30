import { ArrowUpRight, CircleAlert, CircleCheck, CircleDot, CircleX } from "lucide-react";
import type { Project } from "@/lib/projects";
import { source } from "@/lib/source";
import { ago } from "@/lib/format";
import { plural, tr } from "@/lib/i18n";
import { identityOf } from "@/lib/identity";
import { urgent } from "@/lib/subscriptions";
import { agentUi } from "@/lib/agent/ui";
import { LIFE } from "@/lib/agent/target";
import { uptime, isUp } from "@/lib/sources/uptime";
import { deployments } from "@/lib/sources/railway";
import { localRepo } from "@/lib/sources/git";
import { openIssues, openPulls, repo, runs } from "@/lib/sources/github";
import { domainInfo } from "@/lib/sources/domains";
import { runHealth } from "@/components/z/status";
import { AskButton } from "@/components/agent/ask-button";
import { cn } from "@/lib/utils";

/**
 * "What needs me on this project?": everything wrong or waiting, from the sources the project
 * already has (probes, CI, deploys, local repo, payments, identity to-dos), each with the
 * request that hands it to an agent. Project pages add their own with `extra`.
 */

export type Attention = {
  key: string;
  /** bad: broken now; warn: will bite; info: waiting on you. */
  level: "bad" | "warn" | "info";
  title: string;
  detail?: string;
  /** Where to handle it yourself. */
  href?: string | null;
  ask?: { label: string; prompt: string; target?: string; edit?: boolean };
};

const RANK = { bad: 0, warn: 1, info: 2 } as const;
const DAY = 864e5;
const days = (iso: string | null | undefined) => (iso ? Math.round((new Date(iso).getTime() - Date.now()) / DAY) : null);

/** The generic part: what any configured project can be checked for. */
export async function projectAttention(p: Project): Promise<Attention[]> {
  const r = p.repo;
  const id = identityOf(p.id);
  const [probes, deps, local, gh, ci, prs, issues, domains] = await Promise.all([
    uptime().catch(() => []),
    p.railway ? source(() => deployments(p)) : null,
    p.dir ? source(() => localRepo(p.id)) : null,
    r ? source(() => repo(r)) : null,
    r ? source(() => runs(r)) : null,
    r ? source(() => openPulls(r)) : null,
    r ? source(() => openIssues(r)) : null,
    Promise.all(id.domains.map((d) => domainInfo(d).catch(() => null))),
  ]);
  const out: Attention[] = [];

  for (const u of probes.filter((x) => x.project === p.id && x.up === false)) {
    const since = [...u.samples].reverse().find(isUp);
    out.push({
      key: `down:${u.url}`,
      level: "bad",
      title: tr(`${u.label} ne répond plus`, `${u.label} is down`),
      detail: [u.last?.status ? `HTTP ${u.last.status}` : tr("pas de réponse", "no response"), since ? tr(`dernière réponse ${ago(since.t)}`, `last answered ${ago(since.t)}`) : null].filter(Boolean).join(" · "),
      href: u.url,
      ask: {
        label: tr("Enquête sur la panne", "Investigate the outage"),
        prompt: tr(
          `${u.label} (${u.url}) ne répond plus${since ? ` depuis ${ago(since.t)}` : ""}. Diagnostique : état du service, logs, derniers déploiements et commits. Propose le correctif et applique-le sur une branche ; ne déploie rien en production sans me demander.`,
          `${u.label} (${u.url}) is down${since ? ` since ${ago(since.t)}` : ""}. Diagnose it: service status, logs, latest deploys and commits. Propose the fix and apply it on a branch; don't deploy anything to production without asking me.`,
        ),
      },
    });
  }

  for (const s of urgent().filter((x) => x.project === p.id)) {
    out.push({
      key: `pay:${s.name}`,
      level: "bad",
      title: tr(`Paiement en échec · ${s.name}`, `Failed payment · ${s.name}`),
      detail: s.evidence,
      href: s.manage_url ?? null,
      ask: {
        label: tr("Aide-moi à régler", "Help me fix it"),
        target: LIFE,
        prompt: tr(
          `Mon paiement « ${s.name} »${s.vendor ? ` (${s.vendor})` : ""} pour ${p.name} est en échec : ${s.evidence}. Trouve dans mes e-mails ce qui est demandé exactement (montant, échéance, lien), dis-moi quel moyen de paiement mettre à jour et où${s.manage_url ? ` (page de gestion : ${s.manage_url})` : ""}, en 3 étapes. Ne paie rien toi-même.`,
          `My payment for "${s.name}"${s.vendor ? ` (${s.vendor})` : ""} for ${p.name} is failing: ${s.evidence}. Find in my emails exactly what is asked (amount, deadline, link), tell me which payment method to update and where${s.manage_url ? ` (management page: ${s.manage_url})` : ""}, in 3 steps. Don't pay anything yourself.`,
        ),
      },
    });
  }

  // CI: the latest run of each workflow on the default branch.
  if (r && ci?.ok) {
    const main = gh?.ok ? gh.data.default_branch : null;
    const seen = new Set<string>();
    for (const run of ci.data) {
      if (main ? run.head_branch !== main : !/^(main|master)$/.test(run.head_branch)) continue;
      if (seen.has(run.name)) continue;
      seen.add(run.name);
      if (runHealth(run) !== "down" || Date.now() - new Date(run.created_at).getTime() > 14 * DAY) continue;
      out.push({
        key: `ci:${run.name}`,
        level: "bad",
        title: tr(`CI en échec · ${run.name}`, `CI failing · ${run.name}`),
        detail: `${run.display_title} · ${ago(run.created_at)}`,
        href: run.html_url,
        ask: {
          label: tr("Répare la CI", "Fix the CI"),
          prompt: tr(
            `Le workflow « ${run.name} » échoue sur ${run.head_branch} de ${r} (${run.html_url}). Trouve pourquoi (gh run view ${run.id} --log-failed), corrige la cause sur une nouvelle branche et ouvre une PR. Ne pousse pas sur ${run.head_branch}.`,
            `The "${run.name}" workflow fails on ${run.head_branch} of ${r} (${run.html_url}). Find out why (gh run view ${run.id} --log-failed), fix the cause on a new branch and open a PR. Don't push to ${run.head_branch}.`,
          ),
        },
      });
    }
  }

  const last = deps?.ok ? deps.data[0] : undefined;
  if (last && (last.status === "FAILED" || last.status === "CRASHED")) {
    const msg = last.meta?.commitMessage?.split("\n")[0];
    out.push({
      key: `deploy:${last.id}`,
      level: "bad",
      title: last.status === "CRASHED" ? tr("Le service Railway a planté", "The Railway service crashed") : tr("Dernier déploiement en échec", "Last deploy failed"),
      detail: [msg, ago(last.createdAt)].filter(Boolean).join(" · "),
      href: p.railway ? `https://railway.com/project/${p.railway.projectId}/service/${p.railway.serviceId}` : null,
      ask: {
        label: tr("Trouve la cause", "Find the cause"),
        prompt: tr(
          `Le dernier déploiement Railway de ${p.name} est « ${last.status} »${msg ? ` (commit : « ${msg} »)` : ""}. Lis les logs de build et de déploiement, trouve la cause et corrige-la sur une branche. Ne redéploie rien sans me demander.`,
          `${p.name}'s last Railway deploy is "${last.status}"${msg ? ` (commit: "${msg}")` : ""}. Read the build and deploy logs, find the cause and fix it on a branch. Don't redeploy anything without asking me.`,
        ),
      },
    });
  }

  if (local?.ok && (local.data.dirty > 0 || local.data.ahead > 0)) {
    const l = local.data;
    const parts = [
      l.dirty > 0 && `${l.dirty} ${plural(l.dirty, ["fichier non commité", "fichiers non commités"], ["uncommitted file", "uncommitted files"])}`,
      l.ahead > 0 && `${l.ahead} ${plural(l.ahead, ["commit non poussé", "commits non poussés"], ["unpushed commit", "unpushed commits"])}`,
    ].filter((x): x is string => !!x);
    out.push({
      key: "local",
      level: "warn",
      title: parts.join(tr(" et ", " and ")),
      detail: tr(`sur ${l.branch}${l.commits[0] ? ` · dernier commit ${ago(l.commits[0].at)}` : ""}`, `on ${l.branch}${l.commits[0] ? ` · last commit ${ago(l.commits[0].at)}` : ""}`),
      ask: {
        label: tr("Range le travail en cours", "Tidy up work in progress"),
        prompt: tr(
          `Dans ce dépôt, sur la branche ${l.branch} : ${parts.join(" et ")}. Regarde git status et git diff, résume ce que c'est en quelques lignes, puis propose un découpage en commits logiques avec leurs messages. Ne committe qu'après mon accord et ne pousse rien sans me demander. Attention : un autre agent travaille peut-être dans ce dossier.`,
          `In this repository, on branch ${l.branch}: ${parts.join(" and ")}. Look at git status and git diff, summarise what it is in a few lines, then propose a split into logical commits with their messages. Only commit once I agree and don't push anything without asking. Careful: another agent may be working in this folder.`,
        ),
      },
    });
  }

  if (r && prs?.ok && prs.data.length) {
    const n = prs.data.length;
    out.push({
      key: "prs",
      level: "info",
      title: `${n} ${plural(n, ["pull request ouverte", "pull requests ouvertes"], ["open pull request", "open pull requests"])}`,
      detail: prs.data.slice(0, 2).map((x) => `#${x.number} ${x.title}`).join(" · "),
      href: `https://github.com/${r}/pulls`,
      ask: {
        label: tr("Passe-les en revue", "Review them"),
        prompt: tr(
          `Passe en revue les pull requests ouvertes de ${r} (gh pr list, gh pr diff) : pour chacune, résume-la, signale les risques et dis si elle est prête à fusionner. Ne fusionne, ne ferme et ne commente rien sans me demander.`,
          `Review the open pull requests of ${r} (gh pr list, gh pr diff): for each, summarise it, flag risks and say whether it's ready to merge. Don't merge, close or comment on anything without asking me.`,
        ),
      },
    });
  }

  if (r && issues?.ok && issues.data.length) {
    const n = issues.data.length;
    out.push({
      key: "issues",
      level: "info",
      title: `${n}${n >= 20 ? "+" : ""} ${plural(n, ["issue ouverte", "issues ouvertes"], ["open issue", "open issues"])}`,
      detail: issues.data.slice(0, 2).map((x) => `#${x.number} ${x.title}`).join(" · "),
      href: `https://github.com/${r}/issues`,
      ask: {
        label: tr("Trie les issues", "Triage the issues"),
        prompt: tr(
          `Lis les issues ouvertes de ${r} (gh issue list), classe-les par urgence et propose un plan de correction pour les 3 plus importantes. Ne ferme, n'étiquette et ne commente rien sans me demander.`,
          `Read the open issues of ${r} (gh issue list), rank them by urgency and propose a fix plan for the 3 most important. Don't close, label or comment on anything without asking me.`,
        ),
      },
    });
  }

  for (const d of domains) {
    if (!d) continue;
    const reg = days(d.expires);
    const cert = days(d.tlsExpires);
    if (reg != null && reg < 45)
      out.push({
        key: `domain:${d.domain}`,
        level: reg < 14 ? "bad" : "warn",
        title: tr(`${d.domain} expire dans ${reg} j`, `${d.domain} expires in ${reg} d`),
        detail: tr(`à renouveler chez ${d.registrar ?? "le registraire"}`, `renew at ${d.registrar ?? "the registrar"}`),
      });
    if (cert != null && cert < 14)
      out.push({
        key: `tls:${d.domain}`,
        level: cert < 5 ? "bad" : "warn",
        title: tr(`Certificat HTTPS de ${d.domain} : ${cert} j restants`, `HTTPS certificate of ${d.domain}: ${cert} d left`),
      });
  }

  for (const t of id.todo)
    out.push({
      key: `todo:${t}`,
      level: "info",
      title: t,
      ask: {
        label: tr("Confier", "Delegate"),
        edit: true,
        prompt: tr(
          `Aide-moi avec cette tâche de ${p.name} : « ${t} ». Dis-moi exactement quoi faire, étape par étape, et prépare ce que tu peux. Ne publie, n'envoie et ne paie rien sans me demander.`,
          `Help me with this ${p.name} task: "${t}". Tell me exactly what to do, step by step, and prepare what you can. Don't publish, send or pay anything without asking me.`,
        ),
      },
    });

  return out;
}

const ICON = {
  bad: <CircleX className="size-4 shrink-0 text-bad" />,
  warn: <CircleAlert className="size-4 shrink-0 text-warn" />,
  info: <CircleDot className="size-4 shrink-0 text-ink-3" />,
};

const SHOWN = 6;

/** The strip under a project's header: each thing that needs you, or one calm line. */
export async function ProjectAttention({ project: p, extra }: { project: Project; extra?: () => Promise<Attention[]> }) {
  const [base, more] = await Promise.all([projectAttention(p), extra ? extra().catch(() => []) : []]);
  const items = [...more, ...base].sort((a, b) => RANK[a.level] - RANK[b.level]);
  const ui = agentUi();
  const canAsk = ui.enabled && ui.targets.some((t) => t.id === p.id);

  if (!items.length)
    return (
      <div className="flex h-10 items-center gap-2 rounded-xl border border-line bg-surface px-4 text-[13px] text-ink-2">
        <CircleCheck className="size-4 text-good" />
        {tr("Tout est en ordre", "All good")}
        <span className="text-ink-3">· {tr("rien ne demande ton attention", "nothing needs you")}</span>
      </div>
    );

  const urgentCount = items.filter((i) => i.level !== "info").length;
  const row = (i: Attention) => (
    <li key={i.key} className="flex min-h-10 flex-wrap items-center gap-x-3 gap-y-1 border-t border-line px-4 py-2 first:border-t-0 sm:flex-nowrap">
      {ICON[i.level]}
      <div className="min-w-0 flex-1">
        <div className={cn("truncate text-[13px]", i.level === "info" ? "text-ink-2" : "text-ink")}>{i.title}</div>
        {i.detail && <div className="truncate text-xs text-ink-3">{i.detail}</div>}
      </div>
      <div className="ml-7 flex shrink-0 items-center gap-1.5 sm:ml-0">
        {i.href && (
          <a href={i.href} target="_blank" rel="noopener noreferrer" className="inline-flex h-7 items-center gap-1 rounded-md px-2 text-xs text-ink-3 transition-colors hover:bg-hover hover:text-ink">
            {tr("Ouvrir", "Open")}
            <ArrowUpRight className="size-3" />
          </a>
        )}
        {canAsk && i.ask && <AskButton label={i.ask.label} prompt={i.ask.prompt} target={i.ask.target ?? p.id} edit={i.ask.edit} />}
      </div>
    </li>
  );

  return (
    <section className="overflow-hidden rounded-xl border border-line bg-surface">
      <header className="flex h-10 items-center justify-between gap-3 border-b border-line px-4">
        <h2 className="text-[13px] font-semibold text-ink">
          {tr("À traiter", "Needs attention")} <span className="font-normal text-ink-3 tabular">{items.length}</span>
        </h2>
        <span className="text-xs text-ink-3">
          {urgentCount
            ? `${urgentCount} ${plural(urgentCount, ["problème", "problèmes"], ["issue", "issues"])} ${tr("à régler", "to fix")}`
            : tr("rien de cassé", "nothing broken")}
        </span>
      </header>
      <ul>{items.slice(0, SHOWN).map(row)}</ul>
      {items.length > SHOWN && (
        <details className="group border-t border-line">
          <summary className="flex h-9 cursor-pointer list-none items-center px-4 text-xs text-ink-3 hover:text-ink group-open:hidden">
            {tr(`${items.length - SHOWN} de plus`, `${items.length - SHOWN} more`)}
          </summary>
          <ul>{items.slice(SHOWN).map(row)}</ul>
        </details>
      )}
    </section>
  );
}
