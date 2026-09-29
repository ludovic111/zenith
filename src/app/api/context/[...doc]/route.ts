import { doc } from "@/lib/context";
import { tr } from "@/lib/i18n";

/** One context document: brief, vie, argent, annuaire, veille, projets/<id>. */
export async function GET(_: Request, ctx: { params: Promise<{ doc: string[] }> }) {
  const { doc: parts } = await ctx.params;
  const body = await doc(parts.join("/").replace(/\.md$/, ""));
  if (!body) return new Response(tr("Document inconnu", "Unknown document"), { status: 404 });
  return new Response(body, { headers: { "Content-Type": "text/markdown; charset=utf-8", "Cache-Control": "no-store" } });
}
