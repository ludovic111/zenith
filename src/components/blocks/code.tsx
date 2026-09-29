import { GitBranch, GitPullRequest, CircleDot, Star, Eye } from "lucide-react";
import type { Project } from "@/lib/projects";
import { localRepo } from "@/lib/sources/git";
import { openIssues, openPulls, repo, runs, views } from "@/lib/sources/github";
import { source } from "@/lib/source";
import { ago, weekly } from "@/lib/format";
import { plural, tr } from "@/lib/i18n";
import { Panel, ExtLink } from "@/components/z/panel";
import { Gate } from "@/components/z/gate";
import { Status, runHealth } from "@/components/z/status";
import { Bars } from "@/components/charts/bars";

/**
 * The local repository (branch, commits, uncommitted work) and its GitHub side (CI, PRs, issues).
 * Each half shows only when `dir` or `repo` is set; the panel is hidden when neither is.
 */
export async function CodePanel({ project: p }: { project: Project }) {
  const r = p.repo;
  if (!r && !p.dir) return null;
  const [local, gh, ci, prs, issues, traffic] = await Promise.all([
    p.dir ? source(() => localRepo(p.id)) : null,
    r ? source(() => repo(r)) : null,
    r ? source(() => runs(r)) : null,
    r ? source(() => openPulls(r)) : null,
    r ? source(() => openIssues(r)) : null,
    r ? source(() => views(r)) : null,
  ]);

  return (
    <Panel
      kicker="Code"
      title={
        r ? (
          <ExtLink href={`https://github.com/${r}`} className="font-mono">
            {r}
          </ExtLink>
        ) : (
          <span className="font-mono">{p.dir}</span>
        )
      }
      accent={p.glow}
      action={
        gh?.ok && (
          <div className="flex gap-3 text-xs text-ink-3">
            <span className="inline-flex items-center gap-1"><Star className="size-3.5" />{gh.data.stargazers_count}</span>
            {traffic?.ok && (
              <span className="inline-flex items-center gap-1" title={tr("Vues du dépôt sur 14 jours", "Repository views over 14 days")}>
                <Eye className="size-3.5" />
                {traffic.data.count}
              </span>
            )}
            <span>{gh.data.private ? tr("privé", "private") : tr("public", "public")}</span>
          </div>
        )
      }
    >
      {local && (
        <Gate src={local}>
          {(l) => {
            const last30 = l.commits.filter((c) => c.at > Date.now() - 30 * 864e5).length;
            return (
              <>
                <div className="mb-5 flex flex-wrap gap-2 text-xs">
                  <span className="inline-flex items-center gap-1.5 rounded-full border border-line px-2.5 py-1 font-mono text-ink-2">
                    <GitBranch className="size-3.5" /> {l.branch}
                  </span>
                  <span className="rounded-full border border-line px-2.5 py-1 text-ink-2">{tr(`${last30} commits en 30 j`, `${last30} ${plural(last30, ["commit", "commits"], ["commit", "commits"])} in 30 d`)}</span>
                  {l.dirty > 0 && (
                    <span className="rounded-full border border-warn/40 bg-warn/10 px-2.5 py-1 text-warn">
                      {tr(`${l.dirty} fichier(s) non commité(s)`, `${l.dirty} uncommitted ${plural(l.dirty, ["file", "files"], ["file", "files"])}`)}
                    </span>
                  )}
                  {l.ahead > 0 && (
                    <span className="rounded-full border border-warn/40 bg-warn/10 px-2.5 py-1 text-warn">
                      {tr(`${l.ahead} commit(s) non poussé(s)`, `${l.ahead} unpushed ${plural(l.ahead, ["commit", "commits"], ["commit", "commits"])}`)}
                    </span>
                  )}
                </div>
                <Bars data={weekly(l.commits.map((c) => c.at), 16)} color={p.color} height={110} />
                <ul className="mt-5 space-y-2">
                  {l.commits.slice(0, 6).map((c) => (
                    <li key={c.hash} className="flex items-baseline gap-3 text-sm">
                      <code className="shrink-0 font-mono text-[11px] text-ink-3">{c.hash.slice(0, 7)}</code>
                      <span className="min-w-0 flex-1 truncate text-ink-2">{c.subject}</span>
                      <span className="shrink-0 text-xs text-ink-3">{ago(c.at)}</span>
                    </li>
                  ))}
                </ul>
              </>
            );
          }}
        </Gate>
      )}

      {ci && (
        <div className={`grid gap-5 sm:grid-cols-2 ${local ? "mt-6 border-t border-line pt-5" : ""}`}>
          <div>
            <div className="mb-2 text-[11px] uppercase tracking-[0.18em] text-ink-3">{tr("Intégration continue", "Continuous integration")}</div>
            <Gate src={ci} compact>
              {(list) =>
                list.length ? (
                  <ul className="space-y-1.5">
                    {list.slice(0, 4).map((x) => (
                      <li key={x.id} className="flex items-center gap-2 text-sm">
                        <Status health={runHealth(x)} label="" />
                        <a href={x.html_url} target="_blank" rel="noopener noreferrer" className="min-w-0 flex-1 truncate text-ink-2 hover:text-ink">
                          {x.display_title}
                        </a>
                        <span className="shrink-0 text-xs text-ink-3">{ago(x.created_at)}</span>
                      </li>
                    ))}
                  </ul>
                ) : (
                  <p className="text-sm text-ink-3">{tr("Aucun workflow.", "No workflows.")}</p>
                )
              }
            </Gate>
          </div>
          <div>
            <div className="mb-2 text-[11px] uppercase tracking-[0.18em] text-ink-3">{tr("Ouvert", "Open")}</div>
            <ul className="space-y-1.5 text-sm">
              {prs?.ok &&
                prs.data.map((x) => (
                  <li key={`pr${x.number}`} className="flex items-center gap-2">
                    <GitPullRequest className="size-3.5 shrink-0 text-good" />
                    <a href={x.html_url} target="_blank" rel="noopener noreferrer" className="truncate text-ink-2 hover:text-ink">{x.title}</a>
                  </li>
                ))}
              {issues?.ok &&
                issues.data.slice(0, 5).map((x) => (
                  <li key={`is${x.number}`} className="flex items-center gap-2">
                    <CircleDot className="size-3.5 shrink-0 text-warn" />
                    <a href={x.html_url} target="_blank" rel="noopener noreferrer" className="truncate text-ink-2 hover:text-ink">{x.title}</a>
                  </li>
                ))}
              {prs?.ok && issues?.ok && !prs.data.length && !issues.data.length && <li className="text-ink-3">{tr("Rien en attente. ✨", "Nothing waiting. ✨")}</li>}
            </ul>
          </div>
        </div>
      )}
    </Panel>
  );
}
