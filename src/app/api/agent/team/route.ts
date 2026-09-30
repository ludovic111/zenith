import { noStore } from "@/lib/code/http";
import { mayAct } from "@/lib/agent/auth";
import { busy, members } from "@/lib/agent/talk";

/** The team: who is who, what each is for, which subscription, and who is working now. */
export async function GET(request: Request) {
  if (!mayAct(request) && request.headers.get("sec-fetch-site") !== "same-origin") return Response.json({ error: "Forbidden" }, { status: 403 });
  const working = await busy();
  return Response.json(
    members().map(({ id, name, title, role, provider }) => ({ id, name, title, role, provider, busy: working.has(id) })),
    { headers: noStore },
  );
}
