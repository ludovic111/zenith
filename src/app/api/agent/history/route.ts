import { noStore } from "@/lib/code/http";
import { mayAct } from "@/lib/agent/auth";
import { history } from "@/lib/agent/knowledge";

/** `?hours=24&agent=<id>`: the team's recent conversations, to learn from them. */
export async function GET(request: Request) {
  if (!mayAct(request) && request.headers.get("sec-fetch-site") !== "same-origin") return Response.json({ error: "Forbidden" }, { status: 403 });
  const q = new URL(request.url).searchParams;
  const hours = Math.min(Math.max(Number(q.get("hours")) || 24, 1), 24 * 30);
  try {
    return Response.json(await history({ hours, agent: q.get("agent") || undefined }), { headers: noStore });
  } catch (e) {
    return Response.json({ error: e instanceof Error ? e.message : String(e) }, { status: 502, headers: noStore });
  }
}
