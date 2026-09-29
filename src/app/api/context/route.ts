import { brief } from "@/lib/context";

/** The full brief, in Markdown: paste it into any agent. */
export async function GET() {
  return new Response(await brief(), { headers: { "Content-Type": "text/markdown; charset=utf-8", "Cache-Control": "no-store" } });
}
