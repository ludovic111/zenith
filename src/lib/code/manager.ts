import "server-only";
import { spawn, type ChildProcess } from "node:child_process";
import { createWriteStream, mkdirSync, readFileSync, renameSync, statSync, type WriteStream } from "node:fs";
import path from "node:path";
import { config } from "../config";
import { CODE_BIN, CODE_LOG, CODE_ROOT, codeHome, codeOrigin, codePort, codeVersion, isCodeBuilt } from "./paths";
import { registerProjects } from "./cli";

/**
 * Runs the zenith code server as a child of zenith: 127.0.0.1 only, its own state
 * directory, restarted with backoff when it crashes, stopped when zenith exits.
 * State lives on globalThis so dev hot reloads never spawn a second server.
 */

export type CodeStatus = {
  enabled: boolean;
  built: boolean;
  running: boolean;
  /** Starting, or waiting for a restart after a crash. */
  starting: boolean;
  /** True when a server from a previous zenith run was found and reused. */
  adopted: boolean;
  pid: number | null;
  port: number;
  origin: string;
  version: string | null;
  home: string;
  log: string;
  startedAt: number | null;
  restarts: number;
  lastError: string | null;
};

type State = {
  child: ChildProcess | null;
  adoptedPid: number | null;
  ready: boolean;
  starting: boolean;
  stopping: boolean;
  startedAt: number | null;
  restarts: number;
  backoffMs: number;
  retryTimer: NodeJS.Timeout | null;
  lastError: string | null;
  log: WriteStream | null;
  exitHookInstalled: boolean;
  watchdog: NodeJS.Timeout | null;
  onReady: Array<() => void>;
};

const g = globalThis as { __zenithCode?: State };
const state: State = (g.__zenithCode ??= {
  child: null,
  adoptedPid: null,
  ready: false,
  starting: false,
  stopping: false,
  startedAt: null,
  restarts: 0,
  backoffMs: 1_000,
  retryTimer: null,
  lastError: null,
  log: null,
  exitHookInstalled: false,
  watchdog: null,
  onReady: [],
});

const MAX_BACKOFF = 60_000;
const STABLE_AFTER = 60_000;
const READY_TIMEOUT = 60_000;
const LOG_MAX_BYTES = 5 * 1024 * 1024;

// ——— Logging ———

/** Pairing tokens printed at startup are secrets: they never reach the log. */
const redact = (line: string) =>
  line.replace(/(#token=)[\w-]+/g, "$1…").replace(/^(\s*Token:\s*)\S+/gm, "$1…");

function logStream(): WriteStream {
  if (state.log) return state.log;
  mkdirSync(path.dirname(CODE_LOG), { recursive: true });
  try {
    if (statSync(CODE_LOG).size > LOG_MAX_BYTES) renameSync(CODE_LOG, `${CODE_LOG}.1`);
  } catch {}
  state.log = createWriteStream(CODE_LOG, { flags: "a", mode: 0o600 });
  return state.log;
}

function log(message: string) {
  logStream().write(`[${new Date().toISOString()}] [zenith] ${message}\n`);
}

function pipeOutput(stream: NodeJS.ReadableStream | null) {
  if (!stream) return;
  let buffer = "";
  stream.setEncoding("utf8");
  stream.on("data", (chunk: string) => {
    buffer += chunk;
    const lines = buffer.split("\n");
    buffer = lines.pop() ?? "";
    for (const line of lines) {
      if (/[▀▄█]/.test(line)) continue; // QR code
      logStream().write(redact(line) + "\n");
    }
  });
}

// ——— Process helpers ———

const alive = (pid: number) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};

/** Our fork answers /zenith/embed.json; upstream T3 Code (or anything else) does not. */
async function answers(timeoutMs = 1_500): Promise<boolean> {
  try {
    const res = await fetch(`${codeOrigin()}/zenith/embed.json`, { signal: AbortSignal.timeout(timeoutMs), cache: "no-store" });
    return res.ok;
  } catch {
    return false;
  }
}

/** A server left running by a previous zenith process, on our port and state dir. */
function previousServerPid(): number | null {
  try {
    const runtime = JSON.parse(readFileSync(path.join(codeHome(), "userdata", "server-runtime.json"), "utf8")) as { pid?: number; port?: number };
    return runtime.pid && runtime.port === codePort() && alive(runtime.pid) ? runtime.pid : null;
  } catch {
    return null;
  }
}

function markReady() {
  state.ready = true;
  state.starting = false;
  for (const fn of state.onReady.splice(0)) fn();
  registerProjects(log).catch((e) => log(`project registration failed: ${e instanceof Error ? e.message : e}`));
}

async function waitUntilAnswering(child: ChildProcess) {
  const deadline = Date.now() + READY_TIMEOUT;
  while (Date.now() < deadline && state.child === child && child.exitCode === null) {
    if (await answers()) return markReady();
    await new Promise((r) => setTimeout(r, 500));
  }
}

function installExitHook() {
  if (state.exitHookInstalled) return;
  state.exitHookInstalled = true;
  // `next start` and `next dev` exit through process.exit on SIGINT/SIGTERM, which fires this.
  process.once("exit", () => {
    state.stopping = true;
    state.child?.kill("SIGTERM");
  });
}

/** We are not the parent of an adopted server, so poll it to notice when it goes away. */
function watchAdopted() {
  if (state.watchdog) return;
  state.watchdog = setInterval(() => {
    if (!state.adoptedPid) return;
    if (alive(state.adoptedPid)) return;
    log(`adopted server (pid ${state.adoptedPid}) is gone`);
    state.adoptedPid = null;
    state.ready = false;
    if (!state.stopping && !state.retryTimer) scheduleRestart("adopted server exited");
  }, 10_000);
  state.watchdog.unref();
}

function scheduleRestart(reason: string) {
  state.lastError = reason;
  state.starting = true;
  const delay = state.backoffMs;
  state.backoffMs = Math.min(state.backoffMs * 2, MAX_BACKOFF);
  log(`restarting in ${Math.round(delay / 1000)}s (${reason})`);
  state.retryTimer = setTimeout(() => {
    state.retryTimer = null;
    state.restarts++;
    void spawnServer();
  }, delay);
}

async function spawnServer() {
  if (state.child || state.stopping) return;
  if (!isCodeBuilt()) {
    state.starting = false;
    state.lastError = "not built";
    return;
  }

  // Reuse a server orphaned by a previous zenith (crash, SIGKILL) instead of fighting for the port.
  const previous = previousServerPid();
  if (previous && (await answers())) {
    state.adoptedPid = previous;
    state.startedAt ??= Date.now();
    log(`reusing the running server (pid ${previous})`);
    markReady();
    watchAdopted();
    return;
  }

  const home = codeHome();
  mkdirSync(home, { recursive: true });
  const args = [CODE_BIN, "serve", "--host", "127.0.0.1", "--port", String(codePort()), "--base-dir", home];
  log(`starting: node ${args.join(" ")}`);
  const child = spawn(process.execPath, args, {
    cwd: CODE_ROOT,
    env: { ...process.env, T3CODE_NO_BROWSER: "1", NODE_ENV: "production" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  state.child = child;
  state.adoptedPid = null;
  state.ready = false;
  state.starting = true;
  state.startedAt = Date.now();
  pipeOutput(child.stdout);
  pipeOutput(child.stderr);

  child.once("error", (e) => {
    log(`spawn failed: ${e.message}`);
    state.lastError = e.message;
  });
  child.once("exit", (code, signal) => {
    const ranFor = Date.now() - (state.startedAt ?? Date.now());
    log(`exited (${signal ?? `code ${code}`}) after ${Math.round(ranFor / 1000)}s`);
    if (state.child === child) state.child = null;
    state.ready = false;
    if (state.stopping) {
      state.starting = false;
      return;
    }
    if (ranFor > STABLE_AFTER) state.backoffMs = 1_000;
    scheduleRestart(signal ? `killed by ${signal}` : `exited with code ${code}`);
  });

  void waitUntilAnswering(child);
}

// ——— Public API ———

/** Called once from instrumentation.ts. No-op when disabled, not built, or already running. */
export function startCodeServer() {
  if (process.env.NEXT_PHASE === "phase-production-build") return;
  if (!config().code.enabled || !isCodeBuilt()) return;
  installExitHook();
  if (state.child || state.adoptedPid || state.retryTimer) return;
  state.stopping = false;
  state.starting = true;
  void spawnServer();
}

async function stopCurrent() {
  state.stopping = true;
  if (state.retryTimer) clearTimeout(state.retryTimer);
  state.retryTimer = null;
  const child = state.child;
  if (child && child.exitCode === null) {
    const exited = new Promise<void>((resolve) => child.once("exit", () => resolve()));
    child.kill("SIGTERM");
    const timer = setTimeout(() => child.kill("SIGKILL"), 5_000);
    await exited;
    clearTimeout(timer);
  } else if (state.adoptedPid && alive(state.adoptedPid)) {
    const pid = state.adoptedPid;
    process.kill(pid, "SIGTERM");
    for (let i = 0; i < 50 && alive(pid); i++) await new Promise((r) => setTimeout(r, 100));
    if (alive(pid)) process.kill(pid, "SIGKILL");
  }
  state.child = null;
  state.adoptedPid = null;
  state.ready = false;
}

export async function stopCodeServer() {
  await stopCurrent();
  state.starting = false;
  log("stopped");
}

export async function restartCodeServer() {
  await stopCurrent();
  state.stopping = false;
  state.backoffMs = 1_000;
  state.lastError = null;
  state.restarts++;
  state.starting = true;
  if (config().code.enabled) await spawnServer();
  else state.starting = false;
}

/** Resolves once the server answers (immediately if it already does). */
export function whenCodeReady(timeoutMs = READY_TIMEOUT): Promise<boolean> {
  if (state.ready) return Promise.resolve(true);
  return new Promise((resolve) => {
    const timer = setTimeout(() => resolve(false), timeoutMs);
    state.onReady.push(() => {
      clearTimeout(timer);
      resolve(true);
    });
  });
}

export function codeStatus(): CodeStatus {
  const pid = state.child?.pid ?? state.adoptedPid ?? null;
  return {
    enabled: config().code.enabled,
    built: isCodeBuilt(),
    running: state.ready && pid !== null,
    starting: state.starting,
    adopted: state.adoptedPid !== null,
    pid,
    port: codePort(),
    origin: codeOrigin(),
    version: codeVersion(),
    home: codeHome(),
    log: CODE_LOG,
    startedAt: state.startedAt,
    restarts: state.restarts,
    lastError: state.lastError,
  };
}
