import { tr } from "@/lib/i18n";
import { noStore } from "@/lib/code/http";
import { mayAct, underLimit } from "@/lib/agent/auth";
import { message } from "@/lib/agent/talk";

/**
 * One agent writes to a teammate: `{ from?, to, text, wait? }`. With `wait` (the
 * default), the answer comes back in the response, after up to ten minutes.
 */
export async function POST(request: Request) {
  if (!mayAct(request)) return Response.json({ error: "Forbidden" }, { status: 403 });
  if (!underLimit("team", 30)) return Response.json({ error: tr("L'équipe s'est déjà beaucoup parlé cette heure-ci.", "The team has talked a lot this hour already.") }, { status: 429 });
  const body = (await request.json().catch(() => null)) as { from?: unknown; to?: unknown; text?: unknown; wait?: unknown } | null;
  const str = (v: unknown) => (typeof v === "string" && v.trim() ? v.trim() : null);
  const to = str(body?.to);
  const text = str(body?.text);
  if (!to || !text) return Response.json({ error: tr("Il faut un destinataire et un message.", "A recipient and a message are needed.") }, { status: 400 });
  if (text.length > 20_000) return Response.json({ error: tr("Message trop long.", "Message too long.") }, { status: 413 });
  try {
    return Response.json(await message({ from: str(body?.from), to, text, wait: body?.wait !== false }), { headers: noStore });
  } catch (e) {
    return Response.json({ error: e instanceof Error ? e.message : String(e) }, { status: 502, headers: noStore });
  }
}
