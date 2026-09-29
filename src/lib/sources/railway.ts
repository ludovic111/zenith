import "server-only";
import type { Project } from "../projects";
import { cached, getJson, need } from "../source";

export type Deployment = {
  id: string;
  status: string;
  createdAt: string;
  staticUrl: string | null;
  meta: { commitMessage?: string; commitHash?: string; branch?: string } | null;
};

export type HttpLog = {
  timestamp: string;
  method: string;
  path: string;
  httpStatus: number;
  totalDuration: number;
  srcIp: string;
};

async function gql<T>(query: string, variables: Record<string, unknown>): Promise<T> {
  const [tok] = need("RAILWAY_TOKEN");
  const res = await getJson<{ data?: T; errors?: { message: string }[] }>("https://backboard.railway.com/graphql/v2", {
    method: "POST",
    headers: { Authorization: `Bearer ${tok}`, "Content-Type": "application/json" },
    body: JSON.stringify({ query, variables }),
  });
  if (res.errors?.length) throw new Error(res.errors.map((e) => e.message).join(" · "));
  return res.data as T;
}

export const deployments = (p: Project) =>
  cached(`rw:dep:${p.id}`, 120, async () => {
    const r = p.railway!;
    const data = await gql<{ deployments: { edges: { node: Deployment }[] } }>(
      `query($input: DeploymentListInput!) { deployments(input: $input, first: 8) { edges { node { id status createdAt staticUrl meta } } } }`,
      { input: r },
    );
    return data.deployments.edges.map((e) => e.node);
  });

/** Recent HTTP traffic of the live deployment (Railway's log, not an analytics tool). */
export const traffic = (p: Project) =>
  cached(`rw:http:${p.id}`, 300, async () => {
    const live = (await deployments(p)).find((d) => d.status === "SUCCESS" || d.status === "SLEEPING");
    if (!live) return null;
    // Without an anchor the API only returns the last few seconds: walk back 1000 requests from now.
    const data = await gql<{ httpLogs: HttpLog[] }>(
      `query($id: String!, $anchor: String, $n: Int) { httpLogs(deploymentId: $id, anchorDate: $anchor, beforeLimit: $n) { timestamp method path httpStatus totalDuration srcIp } }`,
      { id: live.id, anchor: new Date().toISOString(), n: 1000 },
    );
    const logs = data.httpLogs ?? [];
    const since = logs.length ? Math.min(...logs.map((l) => new Date(l.timestamp).getTime())) : Date.now();
    const paths = new Map<string, number>();
    const codes = { ok: 0, redirect: 0, client: 0, server: 0 };
    for (const l of logs) {
      paths.set(l.path, (paths.get(l.path) ?? 0) + 1);
      if (l.httpStatus >= 500) codes.server++;
      else if (l.httpStatus >= 400) codes.client++;
      else if (l.httpStatus >= 300) codes.redirect++;
      else codes.ok++;
    }
    const durations = logs.map((l) => l.totalDuration).sort((a, b) => a - b);
    return {
      requests: logs.length,
      visitors: new Set(logs.map((l) => l.srcIp)).size,
      since,
      codes,
      p50: durations[Math.floor(durations.length / 2)] ?? null,
      paths: Object.fromEntries(paths),
      topPaths: [...paths.entries()].sort((a, b) => b[1] - a[1]).slice(0, 8).map(([path, count]) => ({ path, count })),
      times: logs.map((l) => new Date(l.timestamp).getTime()),
    };
  });

export type Billing = {
  currentUsage: number;
  creditBalance: number;
  period: { start: string; end: string } | null;
  byProject: Record<string, number>;
};

/** Railway bill for the current period (USD) and each project's estimated share. */
export const billing = () =>
  cached("rw:billing", 900, async (): Promise<Billing> => {
    const data = await gql<{
      me: { workspaces: { id: string; customer: { currentUsage: number; creditBalance: number; billingPeriod: { start: string; end: string } | null } | null }[] };
    }>(`{ me { workspaces { id customer { currentUsage creditBalance billingPeriod { start end } } } } }`, {});
    const ws = data.me.workspaces[0];
    const usage = await gql<{ estimatedUsage: { measurement: string; estimatedValue: number; projectId: string }[] }>(
      `query($w: String) { estimatedUsage(workspaceId: $w, measurements: [CPU_USAGE, MEMORY_USAGE_GB, NETWORK_TX_GB, DISK_USAGE_GB]) { measurement estimatedValue projectId } }`,
      { w: ws.id },
    ).catch(() => ({ estimatedUsage: [] }));
    // Railway's public prices: per minute for CPU/memory/disk, per outbound GB for network.
    const PRICE: Record<string, number> = { CPU_USAGE: 20 / 43200, MEMORY_USAGE_GB: 10 / 43200, DISK_USAGE_GB: 0.15 / 43200, NETWORK_TX_GB: 0.05 };
    const byProject: Record<string, number> = {};
    for (const u of usage.estimatedUsage) byProject[u.projectId] = (byProject[u.projectId] ?? 0) + u.estimatedValue * (PRICE[u.measurement] ?? 0);
    return {
      currentUsage: ws.customer?.currentUsage ?? 0,
      creditBalance: ws.customer?.creditBalance ?? 0,
      period: ws.customer?.billingPeriod ?? null,
      byProject,
    };
  });
