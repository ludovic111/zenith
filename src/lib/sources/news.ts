import "server-only";
import { config } from "../config";
import { tr } from "../i18n";
import { cached, getJson, MissingConfig } from "../source";

export type Headline = { title: string; url: string; at: string | null; source: string; score?: number; comments?: number; discussion?: string };

const decode = (s: string) =>
  s
    .replace(/<!\[CDATA\[([\s\S]*?)\]\]>/g, "$1")
    .replace(/<[^>]+>/g, "")
    .replace(/&#(\d+);/g, (_, n) => String.fromCharCode(Number(n)))
    .replace(/&#x([0-9a-f]+);/gi, (_, n) => String.fromCharCode(parseInt(n, 16)))
    .replace(/&quot;/g, '"')
    .replace(/&apos;|&#39;/g, "'")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&nbsp;/g, " ")
    .replace(/&amp;/g, "&")
    .trim();

/** Drops tracking parameters (utm_*, *_source, fbclid…) from an article link. */
function clean(link: string) {
  try {
    const url = new URL(link);
    for (const k of [...url.searchParams.keys()]) if (/^(utm_|fbclid$|gclid$|mc_|at_)|_source$/i.test(k)) url.searchParams.delete(k);
    return url.toString();
  } catch {
    return link;
  }
}

/** Reads an RSS 2.0 or Atom feed without dependencies: title, link and date of each item. */
async function rss(url: string, name: string, limit = 8): Promise<Headline[]> {
  const res = await fetch(url, { cache: "no-store", signal: AbortSignal.timeout(10000), headers: { "User-Agent": "zenith/1.0 (personal dashboard)" } });
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
  const xml = await res.text();
  const tag = (block: string, t: string) => block.match(new RegExp(`<${t}(?:\\s[^>]*)?>([\\s\\S]*?)</${t}>`))?.[1];
  const items = [...xml.matchAll(/<item[\s>][\s\S]*?<\/item>/g)];
  const atom = !items.length;
  const blocks = atom ? [...xml.matchAll(/<entry[\s>][\s\S]*?<\/entry>/g)] : items;
  return blocks.slice(0, limit).map(([block]) => {
    const date = atom ? (tag(block, "published") ?? tag(block, "updated")) : tag(block, "pubDate");
    // Atom: <link rel="alternate" href="…"/>, or the first link.
    const link = atom
      ? (block.match(/<link[^>]*rel="alternate"[^>]*href="([^"]+)"/)?.[1] ?? block.match(/<link[^>]*href="([^"]+)"/)?.[1] ?? "")
      : (tag(block, "link") ?? "");
    const t = date ? new Date(decode(date)) : null;
    return {
      title: decode(tag(block, "title") ?? ""),
      url: clean(decode(link)),
      at: t && Number.isFinite(t.getTime()) ? t.toISOString() : null,
      source: name,
    };
  });
}

/** Local news: the RSS feeds listed in `news` in zenith.config.json, deduplicated by title. */
export const localNews = async () => {
  const feeds = config().news;
  if (!feeds.length) throw new MissingConfig(["news"]);
  return cached(`news:${feeds.map((f) => f.url).join(",")}`, 900, async () => {
    const lists = await Promise.allSettled(feeds.map((f) => rss(f.url, f.name, f.limit)));
    // One broken feed is not fatal; all of them is.
    if (lists.every((l) => l.status === "rejected")) throw (lists[0] as PromiseRejectedResult).reason;
    const seen = new Set<string>();
    return lists.flatMap((l) => (l.status === "fulfilled" ? l.value : [])).filter((h) => h.title && !seen.has(h.title) && seen.add(h.title));
  });
};

/** Top 10 Hacker News stories (official Firebase API, no key). */
export const hackerNews = () =>
  cached("news:hn", 900, async (): Promise<Headline[]> => {
    const ids = (await getJson<number[]>("https://hacker-news.firebaseio.com/v0/topstories.json")).slice(0, 10);
    const items = await Promise.all(
      ids.map((id) =>
        getJson<{ id: number; title: string; url?: string; time: number; score: number; descendants?: number }>(`https://hacker-news.firebaseio.com/v0/item/${id}.json`).catch(() => null),
      ),
    );
    return items
      .filter((i): i is NonNullable<typeof i> => !!i?.title)
      .map((i) => ({
        title: i.title,
        url: i.url ?? `https://news.ycombinator.com/item?id=${i.id}`,
        discussion: `https://news.ycombinator.com/item?id=${i.id}`,
        at: new Date(i.time * 1000).toISOString(),
        source: "Hacker News",
        score: i.score,
        comments: i.descendants ?? 0,
      }));
  });

/** Words that mean you or your projects (`watch` in zenith.config.json), and what they stand for. */
export const WATCH: { term: string; label: string }[] = config().watch.map((w) => ({ term: w.term.toLowerCase(), label: w.label }));

/** `project` is kept as an alias of `label` for older callers. */
export type Mention = { term: string; label: string; project: string; where: string; title: string; excerpt: string; url: string; at: string };

/** Someone talks about your projects on Hacker News (Algolia search, no key), over 90 days. */
export const hnMentions = () =>
  cached("mentions:hn", 1800, async (): Promise<Mention[]> => {
    const since = Math.floor(Date.now() / 1000) - 90 * 86400;
    const lists = await Promise.all(
      WATCH.map(async (w) => {
        const d = await getJson<{
          hits: { objectID: string; title: string | null; story_title: string | null; comment_text: string | null; story_text: string | null; url: string | null; created_at: string; author: string }[];
        }>(`https://hn.algolia.com/api/v1/search_by_date?query=${encodeURIComponent(w.term)}&tags=(story,comment)&typoTolerance=false&numericFilters=created_at_i>${since}&hitsPerPage=10`).catch(() => ({ hits: [] }));
        return d.hits.flatMap((h): Mention[] => {
          const text = decode(h.comment_text ?? h.story_text ?? "");
          // Algolia tolerates typos: keep only real mentions.
          if (!`${h.title ?? ""} ${h.url ?? ""} ${text}`.toLowerCase().includes(w.term)) return [];
          return [{ ...w, project: w.label, where: "Hacker News", title: h.title ?? h.story_title ?? tr("Commentaire", "Comment"), excerpt: text.slice(0, 220), url: `https://news.ycombinator.com/item?id=${h.objectID}`, at: h.created_at }];
        });
      }),
    );
    return lists.flat().sort((a, b) => b.at.localeCompare(a.at));
  });
