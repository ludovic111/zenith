import { ArrowUpRight, CircleDot, Eye, GitBranch, GitPullRequest, Star } from "lucide-react";
import type { Project } from "@/lib/projects";
import { localRepo } from "@/lib/sources/git";
import { openIssues, openPulls, repo, runs, views } from "@/lib/sources/github";
import { source } from "@/lib/source";
import { ago, weekly } from "@/lib/format";
import { plural, tr } from "@/lib/i18n";
import { Chip, Panel } from "@/components/z/panel";
import { Gate } from "@/components/z/gate";
import { Status, runHealth } from "@/components/z/status";
import { Bars } from "@/components/charts/bars";
import { cn } from "@/lib/utils";

function Sub({ children, count }: { children: React.ReactNode; count?: number }) {
  return (
    <h3 className="mb-1.5 flex items-baseline gap-1.5 text-xs font-medium text-ink-3">
      {children}
      {count != null && <span className="font-normal tabular">{count}</span>}
    </h3>
  );
}

const row = "flex h-8 items-center gap-2.5 rounded-md text-[13px]";

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
  const open = (prs?.ok ? prs.data.length : 0) + (issues?.ok ? issues.data.length : 0);

  return (
    <Panel
      title={tr("Dépôt", "Repository")}
      action={
        <>
          {gh?.ok && (
            <>
              <span className="inline-flex items-center gap-1 tabular" title={tr("Étoiles", "Stars")}>
                <Star className="size-3.5" />
                {gh.data.stargazers_count}
              </span>
              {traffic?.ok && (
                <span className="inline-flex items-center gap-1 tabular" title={tr("Vues du dépôt sur 14 jours", "Repository views over 14 days")}>
                  <Eye className="size-3.5" />
                  {traffic.data.count}
                </span>
              )}
              <span>{gh.data.private ? tr("privé", "private") : tr("public", "public")}</span>
            </>
          )}
          {r ? (
            <a href={`https://github.com/${r}`} target="_blank" rel="noopener noreferrer" className="inline-flex items-center gap-1 font-mono hover:text-ink">
              {r}
              <ArrowUpRight className="size-3" />
            </a>
          ) : (
            <span className="font-mono">{p.dir}</span>
          )}
        </>
      }
    >
      <div className={cn("grid gap-x-8 gap-y-6", local && ci && "lg:grid-cols-[1.3fr_1fr]")}>
        {local && (
          <Gate src={local}>
            {(l) => {
              const last30 = l.commits.filter((c) => c.at > Date.now() - 30 * 864e5).length;
              return (
                <div className="min-w-0">
                  <div className="mb-4 flex flex-wrap items-center gap-1.5">
                    <Chip className="font-mono">
                      <GitBranch className="size-3" /> {l.branch}
                    </Chip>
                    <Chip>{`${last30} ${plural(last30, ["commit", "commits"], ["commit", "commits"])} ${tr("en 30 j", "in 30 d")}`}</Chip>
                    {l.dirty > 0 && (
                      <Chip className="border-warn/30 bg-warn/10 text-warn">
                        {`${l.dirty} ${plural(l.dirty, ["fichier non commité", "fichiers non commités"], ["uncommitted file", "uncommitted files"])}`}
                      </Chip>
                    )}
                    {l.ahead > 0 && (
                      <Chip className="border-warn/30 bg-warn/10 text-warn">
                        {`${l.ahead} ${plural(l.ahead, ["commit non poussé", "commits non poussés"], ["unpushed commit", "unpushed commits"])}`}
                      </Chip>
                    )}
                  </div>
                  <Bars data={weekly(l.commits.map((c) => c.at), 16)} color={p.color} height={96} />
                  <div className="mt-4">
                    <Sub>{tr("Derniers commits", "Latest commits")}</Sub>
                    <ul>
                      {l.commits.slice(0, 6).map((c) => (
                        <li key={c.hash} className={row}>
                          <code className="shrink-0 font-mono text-2xs text-ink-3">{c.hash.slice(0, 7)}</code>
                          <span className="min-w-0 flex-1 truncate text-ink-2">{c.subject}</span>
                          <span className="shrink-0 text-xs text-ink-3">{ago(c.at)}</span>
                        </li>
                      ))}
                    </ul>
                  </div>
                </div>
              );
            }}
          </Gate>
        )}

        {ci && (
          <div className="min-w-0 space-y-5">
            <div>
              <Sub>{tr("Intégration continue", "Continuous integration")}</Sub>
              <Gate src={ci} compact>
                {(list) =>
                  list.length ? (
                    <ul>
                      {list.slice(0, 5).map((x) => (
                        <li key={x.id}>
                          <a href={x.html_url} target="_blank" rel="noopener noreferrer" className={cn(row, "-mx-1.5 px-1.5 hover:bg-hover")}>
                            <Status health={runHealth(x)} label="" />
                            <span className="min-w-0 flex-1 truncate text-ink-2">
                              <span className="text-ink">{x.name}</span> · {x.display_title}
                            </span>
                            <span className="shrink-0 text-xs text-ink-3">{ago(x.created_at)}</span>
                          </a>
                        </li>
                      ))}
                    </ul>
                  ) : (
                    <p className="text-[13px] text-ink-3">{tr("Aucun workflow.", "No workflows.")}</p>
                  )
                }
              </Gate>
            </div>
            <div>
              <Sub count={open}>{tr("Ouvert", "Open")}</Sub>
              <ul>
                {prs?.ok &&
                  prs.data.map((x) => (
                    <li key={`pr${x.number}`}>
                      <a href={x.html_url} target="_blank" rel="noopener noreferrer" className={cn(row, "-mx-1.5 px-1.5 hover:bg-hover")}>
                        <GitPullRequest className="size-3.5 shrink-0 text-good" />
                        <span className="min-w-0 flex-1 truncate text-ink-2">{x.title}</span>
                        <span className="shrink-0 text-xs text-ink-3 tabular">#{x.number}</span>
                      </a>
                    </li>
                  ))}
                {issues?.ok &&
                  issues.data.slice(0, 5).map((x) => (
                    <li key={`is${x.number}`}>
                      <a href={x.html_url} target="_blank" rel="noopener noreferrer" className={cn(row, "-mx-1.5 px-1.5 hover:bg-hover")}>
                        <CircleDot className="size-3.5 shrink-0 text-warn" />
                        <span className="min-w-0 flex-1 truncate text-ink-2">{x.title}</span>
                        <span className="shrink-0 text-xs text-ink-3 tabular">#{x.number}</span>
                      </a>
                    </li>
                  ))}
                {prs?.ok && issues?.ok && !open && <li className="text-[13px] text-ink-3">{tr("Aucune PR ni issue ouverte.", "No open PRs or issues.")}</li>}
              </ul>
            </div>
          </div>
        )}
      </div>
    </Panel>
  );
}
