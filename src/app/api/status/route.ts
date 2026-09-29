import { findProject } from "@/lib/projects";
import { uptime } from "@/lib/sources/uptime";
import { isLive, sessions } from "@/lib/sources/agents";

/** Summary for the Mac app: services down (Dock badge, notifications) and live agents. */
export async function GET() {
  const [probes, agents] = await Promise.all([uptime(), sessions().catch(() => [])]);
  const down = probes
    .filter((u) => u.up === false)
    .map((u) => ({ project: findProject(u.project)?.name ?? u.project, label: u.label, url: u.url }));
  return Response.json({ down, liveAgents: agents.filter(isLive).length });
}
