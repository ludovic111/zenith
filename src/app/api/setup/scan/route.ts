import { tr } from "@/lib/i18n";
import { isSameOrigin, noStore } from "@/lib/code/http";
import { scanProjects, scanRoot } from "@/lib/setup";

/** `?root=~/Code`: the git repositories in that folder, for the welcome and the settings. */
export async function GET(request: Request) {
  if (!isSameOrigin(request) && request.headers.get("sec-fetch-site") !== "same-origin") return Response.json({ error: "Forbidden" }, { status: 403 });
  const root = scanRoot(new URL(request.url).searchParams.get("root"));
  if (!root) return Response.json({ error: tr("Choisis un dossier dans ton dossier personnel.", "Pick a folder inside your home folder.") }, { status: 400 });
  return Response.json({ root, projects: await scanProjects(root) }, { headers: noStore });
}
