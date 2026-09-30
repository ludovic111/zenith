import { Star } from "lucide-react";
import type { Project } from "@/lib/projects";
import { source } from "@/lib/source";
import { ago } from "@/lib/format";
import { plural, tr } from "@/lib/i18n";
import { agentUi } from "@/lib/agent/ui";
import { appReviews, appStore } from "@/lib/sources/domains";
import { Empty, Panel } from "@/components/z/panel";
import { Gate } from "@/components/z/gate";
import { AskButton } from "@/components/agent/ask-button";

const STARS = [0, 1, 2, 3, 4];

/** App Store listing and what people write, in every followed country. Hidden without `appStore`. */
export async function AppStorePanel({ project: p, className }: { project: Project; className?: string }) {
  if (!p.appStore) return null;
  const { id, countries } = p.appStore;
  const [reviews, store] = await Promise.all([source(() => appReviews(id, countries)), source(() => appStore(id, countries?.[0]))]);
  const st = store.ok ? store.data : null;
  const ui = agentUi();
  const canAsk = ui.enabled && ui.targets.some((t) => t.id === p.id) && reviews.ok && reviews.data.length > 0;
  return (
    <Panel
      className={className}
      title={
        <>
          {tr("Avis App Store", "App Store reviews")} {reviews.ok && <span className="font-normal text-ink-3 tabular">{reviews.data.length}</span>}
        </>
      }
      action={
        <>
          {st && (
            <a href={st.url} target="_blank" rel="noopener noreferrer" className="tabular hover:text-ink">
              v{st.version} · {st.rating != null ? `${st.rating.toFixed(1)} / 5` : tr("pas encore de note", "no rating yet")} · {st.ratings} {plural(st.ratings, ["note", "notes"], ["rating", "ratings"])}
            </a>
          )}
          {canAsk && (
            <AskButton
              target={p.id}
              label={tr("Prépare les réponses", "Draft replies")}
              prompt={tr(
                `Lis les avis App Store récents de ${p.name} (app ${id}). Résume ce qui revient (bugs, demandes, compliments), propose une réponse courte et chaleureuse pour chaque avis et les 3 corrections qui feraient monter la note. Ne publie aucune réponse toi-même.`,
                `Read ${p.name}'s recent App Store reviews (app ${id}). Summarise the recurring themes (bugs, requests, praise), draft a short, warm reply to each review and the 3 fixes that would raise the rating. Don't publish any reply yourself.`,
              )}
            />
          )}
        </>
      }
      bodyClassName="px-0 pb-1 pt-1"
    >
      <Gate src={reviews}>
        {(list) =>
          list.length ? (
            <ul className="divide-y divide-line">
              {list.slice(0, 6).map((r) => (
                <li key={r.country + r.author + r.at} className="px-4 py-3">
                  <div className="flex items-center gap-2">
                    <span className="flex" aria-label={`${r.rating}/5`}>
                      {STARS.map((i) => (
                        <Star key={i} className={i < r.rating ? "size-3 fill-current text-warn" : "size-3 text-ink-3/50"} />
                      ))}
                    </span>
                    <span className="min-w-0 flex-1 truncate text-[13px] font-medium text-ink">{r.title}</span>
                    <span className="shrink-0 text-xs text-ink-3">{ago(r.at)}</span>
                  </div>
                  <p className="mt-1 line-clamp-2 text-[13px] text-ink-2">{r.body}</p>
                  <div className="mt-1 text-xs text-ink-3">
                    {r.author} · {r.country.toUpperCase()} · v{r.version}
                  </div>
                </li>
              ))}
            </ul>
          ) : (
            <div className="px-4 pb-3">
              <Empty>
                {tr("Aucun avis écrit pour l'instant", "No written reviews yet")}
                {countries?.length ? ` (${countries.map((c) => c.toUpperCase()).join(", ")})` : ""}.
              </Empty>
            </div>
          )
        }
      </Gate>
    </Panel>
  );
}
