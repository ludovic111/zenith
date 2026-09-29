import "server-only";
import { randomBytes } from "node:crypto";
import { mkdir, rename, writeFile } from "node:fs/promises";
import path from "node:path";

/**
 * zenith's small state files (.data/*.json): written whole to a temporary file then
 * renamed, so a reader never sees half a file, and updated one at a time per file.
 */

export async function writeJson(file: string, data: unknown) {
  await mkdir(path.dirname(file), { recursive: true });
  const tmp = `${file}.${randomBytes(4).toString("hex")}.tmp`;
  await writeFile(tmp, JSON.stringify(data, null, 2));
  await rename(tmp, file);
}

const g = globalThis as { __zenithSerial?: Map<string, Promise<unknown>> };
const queues = (g.__zenithSerial ??= new Map());

/** Runs `fn` after every earlier call for the same key has finished. */
export function serial<T>(key: string, fn: () => Promise<T>): Promise<T> {
  const next = (queues.get(key) ?? Promise.resolve()).catch(() => {}).then(fn);
  queues.set(key, next);
  return next;
}
