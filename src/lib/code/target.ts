import "server-only";
import { existsSync } from "node:fs";
import { findProject, projectDir } from "../projects";
import { BRAND } from "./brand";

export type CodeTarget = { id: string; name: string; dir: string };

/** `?project=<id>` → the folder to focus in zenith code ("zenith" is the dashboard itself). */
export function resolveCodeTarget(id: string | null | undefined): CodeTarget | null {
  if (!id) return null;
  if (id === "zenith") return { id, name: BRAND, dir: process.cwd() };
  const project = findProject(id);
  const dir = project ? projectDir(project) : null;
  return project && dir && existsSync(dir) ? { id, name: project.name, dir } : null;
}
