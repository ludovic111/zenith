import "server-only";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { config } from "../config";
import { tr } from "../i18n";
import { cached, getJson, MissingConfig } from "../source";

const run = promisify(execFile);

/** GITHUB_TOKEN, else the token of the `gh` CLI already signed in on this Mac. */
const token = () =>
  cached("gh:token", 3600, async () => {
    if (process.env.GITHUB_TOKEN) return process.env.GITHUB_TOKEN;
    try {
      const { stdout } = await run("gh", ["auth", "token"], { timeout: 5000 });
      if (stdout.trim()) return stdout.trim();
    } catch {}
    throw new MissingConfig(["GITHUB_TOKEN"]);
  });

async function gh<T>(path: string, accept = "application/vnd.github+json"): Promise<T> {
  return getJson<T>(`https://api.github.com${path}`, {
    headers: {
      Authorization: `Bearer ${await token()}`,
      Accept: accept,
      "X-GitHub-Api-Version": "2022-11-28",
    },
  });
}

export type Repo = {
  full_name: string;
  html_url: string;
  private: boolean;
  stargazers_count: number;
  forks_count: number;
  open_issues_count: number;
  watchers_count: number;
  pushed_at: string;
  language: string | null;
  size: number;
  default_branch: string;
};

export type Release = {
  tag_name: string;
  name: string;
  html_url: string;
  published_at: string;
  prerelease: boolean;
  assets: { name: string; download_count: number; size: number }[];
};

export type Run = {
  id: number;
  name: string;
  status: string;
  conclusion: string | null;
  html_url: string;
  created_at: string;
  head_branch: string;
  display_title: string;
};

export type Traffic = { count: number; uniques: number; views?: { timestamp: string; count: number; uniques: number }[] };

export const repo = (r: string) => cached(`gh:repo:${r}`, 300, () => gh<Repo>(`/repos/${r}`));

export const releases = (r: string) =>
  cached(`gh:rel:${r}`, 600, () => gh<Release[]>(`/repos/${r}/releases?per_page=30`));

export const runs = (r: string) =>
  cached(`gh:runs:${r}`, 180, async () => (await gh<{ workflow_runs: Run[] }>(`/repos/${r}/actions/runs?per_page=8`)).workflow_runs);

export const openPulls = (r: string) =>
  cached(`gh:prs:${r}`, 300, () => gh<{ number: number; title: string; html_url: string; created_at: string }[]>(`/repos/${r}/pulls?state=open&per_page=10`));

export const openIssues = (r: string) =>
  cached(`gh:issues:${r}`, 300, async () =>
    (await gh<{ number: number; title: string; html_url: string; created_at: string; pull_request?: unknown; user: { login: string } }[]>(
      `/repos/${r}/issues?state=open&per_page=20`,
    )).filter((i) => !i.pull_request),
  );

/** Repository views over 14 days (requires owning it). */
export const views = (r: string) => cached(`gh:views:${r}`, 1800, () => gh<Traffic>(`/repos/${r}/traffic/views`));

async function graphql<T>(query: string): Promise<T> {
  const res = await getJson<{ data: T; errors?: { message: string }[] }>("https://api.github.com/graphql", {
    method: "POST",
    headers: { Authorization: `Bearer ${await token()}`, "Content-Type": "application/json" },
    body: JSON.stringify({ query }),
  });
  if (!res.data) throw new Error(res.errors?.map((e) => e.message).join(", ") ?? tr("Réponse GraphQL vide", "Empty GraphQL response"));
  return res.data;
}

export type Sponsors = { listed: boolean; count: number; monthlyUSD: number };

/** GitHub Sponsors of the signed-in account: listing enabled, sponsor count, monthly estimate. */
export const sponsors = () =>
  cached("gh:sponsors", 900, async () => {
    const data = await graphql<{ viewer: { hasSponsorsListing: boolean; monthlyEstimatedSponsorsIncomeInCents: number; sponsors: { totalCount: number } } }>(
      "{ viewer { hasSponsorsListing monthlyEstimatedSponsorsIncomeInCents sponsors { totalCount } } }",
    );
    const v = data.viewer;
    return { listed: v.hasSponsorsListing, count: v.sponsors.totalCount, monthlyUSD: v.monthlyEstimatedSponsorsIncomeInCents / 100 } satisfies Sponsors;
  });

export type Profile = {
  login: string;
  followers: number;
  stars: number;
  repos: number;
  contributions: number;
  days: { date: string; count: number }[];
  streak: number;
};

/** GitHub profile: followers, stars across your repositories, contributions over the year and current streak. */
export const profile = () =>
  cached("gh:profile", 1800, async (): Promise<Profile> => {
    const { viewer: v } = await graphql<{
      viewer: {
        login: string;
        followers: { totalCount: number };
        repositories: { totalCount: number; nodes: { stargazerCount: number }[] };
        contributionsCollection: { contributionCalendar: { totalContributions: number; weeks: { contributionDays: { date: string; contributionCount: number }[] }[] } };
      };
    }>(
      "{ viewer { login followers { totalCount } repositories(ownerAffiliations: OWNER, first: 100) { totalCount nodes { stargazerCount } } contributionsCollection { contributionCalendar { totalContributions weeks { contributionDays { date contributionCount } } } } } }",
    );
    const days = v.contributionsCollection.contributionCalendar.weeks.flatMap((w) => w.contributionDays.map((d) => ({ date: d.date, count: d.contributionCount })));
    // Streak: consecutive days with at least one contribution; today may still be empty.
    let streak = 0;
    for (let i = days.length - 1; i >= 0; i--) {
      if (days[i].count > 0) streak++;
      else if (i < days.length - 1) break;
    }
    return {
      login: v.login,
      followers: v.followers.totalCount,
      stars: v.repositories.nodes.reduce((a, r) => a + r.stargazerCount, 0),
      repos: v.repositories.totalCount,
      contributions: v.contributionsCollection.contributionCalendar.totalContributions,
      days,
      streak,
    };
  });

export type Notification = { id: string; repo: string; title: string; type: string; reason: string; at: string; url: string };

/** Unread GitHub notifications (review requests, mentions, CI…). */
export const notifications = () =>
  cached("gh:notifications", 180, async (): Promise<Notification[]> => {
    const list = await gh<{ id: string; reason: string; updated_at: string; repository: { full_name: string; html_url: string }; subject: { title: string; type: string; url: string | null } }[]>(
      "/notifications?per_page=30",
    );
    return list.map((n) => ({
      id: n.id,
      repo: n.repository.full_name,
      title: n.subject.title,
      type: n.subject.type,
      reason: n.reason,
      at: n.updated_at,
      url: n.subject.url ? n.subject.url.replace("https://api.github.com/repos/", "https://github.com/").replace("/pulls/", "/pull/") : n.repository.html_url,
    }));
  });

/** A CI failing every night gives one notification per night: group them, most recent first. */
export function groupNotifications(list: Notification[]) {
  const groups = new Map<string, Notification & { count: number }>();
  for (const n of list) {
    const key = n.repo + n.title.replace(/, Attempt #\d+/, "");
    const g = groups.get(key);
    if (g) g.count++;
    else groups.set(key, { ...n, count: 1 });
  }
  return [...groups.values()];
}

export type Star = { repo: string; user: string; at: string; url: string };

/** Who starred your repositories over the last 30 days. */
export const recentStars = (repos: string[]) =>
  cached(`gh:stars:${repos.join(",")}`, 1800, async (): Promise<Star[]> => {
    const since = Date.now() - 30 * 864e5;
    const lists = await Promise.all(
      repos.map(async (r) => {
        const info = await repo(r).catch(() => null);
        if (!info?.stargazers_count) return [];
        // The list is chronological: the last page holds the most recent ones.
        const page = Math.max(1, Math.ceil(info.stargazers_count / 100));
        const stars = await gh<{ starred_at: string; user: { login: string; html_url: string } }[]>(`/repos/${r}/stargazers?per_page=100&page=${page}`, "application/vnd.github.star+json").catch(() => []);
        return stars.filter((s) => new Date(s.starred_at).getTime() > since).map((s) => ({ repo: r, user: s.user.login, at: s.starred_at, url: s.user.html_url }));
      }),
    );
    return lists.flat().sort((a, b) => b.at.localeCompare(a.at));
  });

export type GhMention = { term: string; repo: string; title: string; url: string; at: string; kind: "issue" | "pr" };

/**
 * Your GitHub login: the first GitHub account in `owner.socials`, else `owner.sponsors`,
 * else the account the token belongs to.
 */
export async function githubLogin(): Promise<string> {
  const o = config().owner;
  const social = o.socials.find((s) => s.network === "github");
  const fromSocial = social ? (social.handle.replace(/^@/, "") || social.url.match(/github\.com\/([^/?#]+)/i)?.[1]) : null;
  return fromSocial || o.sponsors || (await profile()).login;
}

/** Issues and PRs by other people that cite your projects, across GitHub. `owner` defaults to your login. */
export const githubMentions = (terms: string[], owner?: string) =>
  cached(`gh:mentions:${terms.join(",")}`, 3600, async (): Promise<GhMention[]> => {
    if (!terms.length) return [];
    owner ??= await githubLogin();
    const lists = await Promise.all(
      terms.map(async (term) => {
        const res = await gh<{ items: { title: string; html_url: string; created_at: string; repository_url: string; pull_request?: unknown }[] }>(
          `/search/issues?q=${encodeURIComponent(`${term} -user:${owner}`)}&sort=created&order=desc&per_page=10`,
        ).catch(() => ({ items: [] }));
        return res.items.map((i) => ({
          term,
          repo: i.repository_url.replace("https://api.github.com/repos/", ""),
          title: i.title,
          url: i.html_url,
          at: i.created_at,
          kind: i.pull_request ? ("pr" as const) : ("issue" as const),
        }));
      }),
    );
    return lists.flat().sort((a, b) => b.at.localeCompare(a.at));
  });
