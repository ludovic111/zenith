import "server-only";
import path from "node:path";
import { config, type LoadedConfig } from "./config";
import { liveArray } from "./live";

/** Folder holding all the repositories (zenith included). */
export const projectsRoot = () => config().projectsRoot;

export type ProjectId = string;

export type Probe = { label: string; url: string };

export type Project = LoadedConfig["projects"][number];

/** Your projects, in the order of zenith.config.json: the order also gives each one its color. */
export const PROJECTS: Project[] = liveArray(() => config().projects);

export const project = (id: ProjectId) => PROJECTS.find((p) => p.id === id)!;

export const findProject = (id: ProjectId) => PROJECTS.find((p) => p.id === id) ?? null;

/** Absolute local folder of a project, or null when none is configured. */
export const projectDir = (p: Pick<Project, "dir">) => (p.dir ? path.resolve(projectsRoot(), p.dir.replace(/^~(?=$|\/)/, process.env.HOME ?? "~")) : null);
