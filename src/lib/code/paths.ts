import "server-only";
import { existsSync, readFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { config } from "../config";

/** zenith code: the T3 Code fork in code/, built into a single Node entry point. */
export const CODE_ROOT = path.join(process.cwd(), "code");
export const CODE_BIN = path.join(CODE_ROOT, "apps", "server", "dist", "bin.mjs");
export const CODE_LOG = path.join(process.cwd(), ".data", "code.log");

const expandHome = (p: string) => p.replace(/^~(?=$|[/\\])/, os.homedir());

/** State directory: `code.home` in zenith.config.json, else ~/.zenith/code (never ~/.t3). */
export function codeHome(): string {
  const home = config().code.home?.trim();
  return home ? path.resolve(expandHome(home)) : path.join(os.homedir(), ".zenith", "code");
}

export const codePort = () => config().code.port;

/** Always 127.0.0.1: mixing it with `localhost` would split cookies across sites. */
export const codeOrigin = () => `http://127.0.0.1:${codePort()}`;

export const isCodeBuilt = () => existsSync(CODE_BIN);

/** "0.0.43 · t3code@451afcb", or null before the first build. */
export function codeVersion(): string | null {
  try {
    const pkg = JSON.parse(readFileSync(path.join(CODE_ROOT, "apps", "server", "package.json"), "utf8")) as { version?: string };
    const upstream = readFileSync(path.join(CODE_ROOT, "UPSTREAM"), "utf8").trim().slice(0, 7);
    return [pkg.version, upstream && `t3code@${upstream}`].filter(Boolean).join(" · ") || null;
  } catch {
    return null;
  }
}
