import { tr } from "@/lib/i18n";
import { noStore } from "@/lib/code/http";
import { mayAct, underLimit } from "@/lib/agent/auth";
import { notify } from "@/lib/agent/knowledge";

/** `{ text, from? }`: an agent taps the person on the shoulder. Six times an hour at most. */
export async function POST(request: Request) {
  if (!mayAct(request)) return Response.json({ error: "Forbidden" }, { status: 403 });
  const body = (await request.json().catch(() => null)) as { text?: unknown; from?: unknown } | null;
  const text = typeof body?.text === "string" ? body.text.trim() : "";
  if (!text || text.length > 2000) return Response.json({ error: "Invalid text" }, { status: 400 });
  if (!underLimit("notify", 6)) return Response.json({ error: tr("Déjà six messages cette heure-ci : garde le reste pour le résumé.", "Six messages this hour already: keep the rest for the summary.") }, { status: 429 });
  return Response.json(await notify(text, typeof body?.from === "string" ? body.from : null), { headers: noStore });
}
