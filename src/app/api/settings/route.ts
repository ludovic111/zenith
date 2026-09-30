import { readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { WRITABLE_KEYS } from "@/lib/extensions";
import { tr } from "@/lib/i18n";

const FILE = path.join(process.cwd(), ".env.local");

/** Saves a key to .env.local (ignored by git) and applies it without a restart. Only allowlisted names. */
export async function POST(request: Request) {
  const origin = request.headers.get("origin");
  if (request.headers.get("sec-fetch-site") !== "same-origin" || !origin || new URL(origin).host !== request.headers.get("host"))
    return Response.json({ error: tr("Interdit", "Forbidden") }, { status: 403 });

  const { name, value } = (await request.json().catch(() => ({}))) as { name?: string; value?: string };
  const clean = (value ?? "").trim();
  if (!name || !WRITABLE_KEYS().has(name)) return Response.json({ error: tr("Variable non autorisée", "Variable not allowed") }, { status: 400 });
  if (!clean || /[\r\n\0]/.test(clean) || clean.length > 4096) return Response.json({ error: tr("Valeur invalide", "Invalid value") }, { status: 400 });

  const current = await readFile(FILE, "utf8").catch(() => "");
  const lines = current.split("\n").filter((l) => l && !l.startsWith(`${name}=`));
  lines.push(`${name}=${clean}`);
  await writeFile(FILE, lines.join("\n") + "\n", { mode: 0o600 });

  process.env[name] = clean;
  if (name === "TELEGRAM_BOT_TOKEN") (await import("@/lib/agent/gateway")).startGateway();
  // Drop every cached source and any session an extension kept (globalThis.__<name>Cookie, __<name>Session…).
  const g = globalThis as Record<string, unknown>;
  (g.__zenithCache as Map<string, unknown> | undefined)?.clear();
  for (const k of Object.keys(g)) if (/^__\w+(Cookie|Session|Token)$/.test(k)) delete g[k];
  return Response.json({ ok: true });
}
