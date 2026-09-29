import { noStore } from "@/lib/code/http";
import { mayAct } from "@/lib/agent/auth";
import { runNow } from "@/lib/agent/routines";

/** `{ id }`: runs a routine now, whatever the time. */
export async function POST(request: Request) {
  if (!mayAct(request)) return Response.json({ error: "Forbidden" }, { status: 403 });
  const body = (await request.json().catch(() => null)) as { id?: unknown } | null;
  if (typeof body?.id !== "string") return Response.json({ error: "Invalid request" }, { status: 400 });
  try {
    return Response.json(await runNow(body.id), { headers: noStore });
  } catch (e) {
    return Response.json({ error: e instanceof Error ? e.message : String(e) }, { status: 502, headers: noStore });
  }
}
