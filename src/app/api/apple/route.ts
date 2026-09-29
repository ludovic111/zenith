import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";

/**
 * Relevé envoyé par zenith.app (Calendrier, Rappels et Mail d'Apple, lus avec ton autorisation).
 * Un navigateur envoie toujours un en-tête Origin sur une requête POST : l'app native jamais.
 * On refuse donc tout ce qui vient d'une page web.
 */
export async function POST(request: Request) {
  if (request.headers.get("origin") || request.headers.get("sec-fetch-site")) return Response.json({ error: "Réservé à zenith.app" }, { status: 403 });
  if (request.headers.get("x-zenith-app") !== "1") return Response.json({ error: "Réservé à zenith.app" }, { status: 403 });
  const raw = await request.text();
  if (raw.length > 2_000_000) return Response.json({ error: "Trop gros" }, { status: 413 });
  let data: unknown;
  try {
    data = JSON.parse(raw);
  } catch {
    return Response.json({ error: "JSON invalide" }, { status: 400 });
  }
  const dir = path.join(process.cwd(), ".data");
  await mkdir(dir, { recursive: true });
  await writeFile(path.join(dir, "apple.json"), JSON.stringify({ ...(data as object), receivedAt: new Date().toISOString() }));
  (globalThis as { __zenithCache?: Map<string, unknown> }).__zenithCache?.delete("apple");
  return Response.json({ ok: true });
}
