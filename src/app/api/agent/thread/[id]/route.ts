import { noStore } from "@/lib/code/http";
import { threadDetail } from "@/lib/code/api";
import { mayAct } from "@/lib/agent/auth";

/** A delegated agent's state and last messages, so the agent that asked can follow up. */
export async function GET(request: Request, { params }: { params: Promise<{ id: string }> }) {
  if (!mayAct(request) && request.headers.get("sec-fetch-site") !== "same-origin") return Response.json({ error: "Forbidden" }, { status: 403 });
  const { id } = await params;
  if (!/^[\w-]{8,64}$/.test(id)) return Response.json({ error: "Invalid id" }, { status: 400 });
  try {
    const t = await threadDetail(id);
    const messages = t.messages.filter((m) => m.role !== "system").slice(-6);
    return Response.json({ id: t.id, title: t.title, turn: t.latestTurn?.state ?? null, session: t.session?.status ?? null, messages }, { headers: noStore });
  } catch (e) {
    return Response.json({ error: e instanceof Error ? e.message : String(e) }, { status: 502, headers: noStore });
  }
}
