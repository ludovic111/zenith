import "server-only";
import { randomBytes, timingSafeEqual } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { isSameOrigin } from "../code/http";

/**
 * Who may make zenith act: its own pages (same origin), and local programs that can read
 * .data/agent-token (the MCP server, your scripts). A web page elsewhere can do neither,
 * so it can never start an agent on this Mac.
 */

export const TOKEN_FILE = path.join(process.cwd(), ".data", "agent-token");

const g = globalThis as { __zenithAgentToken?: string };

export function agentToken(): string {
  if (g.__zenithAgentToken) return g.__zenithAgentToken;
  let token = "";
  try {
    token = readFileSync(TOKEN_FILE, "utf8").trim();
  } catch {}
  if (token.length < 32) {
    token = randomBytes(32).toString("hex");
    mkdirSync(path.dirname(TOKEN_FILE), { recursive: true });
    writeFileSync(TOKEN_FILE, token, { mode: 0o600 });
  }
  return (g.__zenithAgentToken = token);
}

export function viaToken(request: Request): boolean {
  const given = request.headers.get("x-zenith-token") ?? "";
  const want = agentToken();
  return given.length === want.length && timingSafeEqual(Buffer.from(given), Buffer.from(want));
}

/** Same-origin JSON from zenith's pages, or the local token. */
export function mayAct(request: Request): boolean {
  if (viaToken(request)) return true;
  return isSameOrigin(request) && (request.headers.get("content-type") ?? "").startsWith("application/json");
}

// Agents asking agents: a ceiling so a loop can't run away.
const recent: number[] = [];
export function underLimit(max = 12, windowMs = 3600e3): boolean {
  const t = Date.now();
  while (recent.length && recent[0] < t - windowMs) recent.shift();
  if (recent.length >= max) return false;
  recent.push(t);
  return true;
}
