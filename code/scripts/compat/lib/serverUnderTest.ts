/**
 * ServerUnderTest: start a zenith code backend as a black box, the way the dashboard does
 * (`serve --host 127.0.0.1 --port <p> --base-dir <home>`), and hand back its URLs and the
 * one-time bootstrap credential printed in its banner.
 *
 *   BACKEND=ts    node <code>/apps/server/dist/bin.mjs   (if built in this checkout)
 *                 node <code>/apps/server/src/bin.ts     (otherwise: Node runs the TS source)
 *                 override: ZENITH_CODE_TS_ENTRY=/path/to/bin.mjs|bin.ts
 *   BACKEND=rust  <repo>/target/debug/zenith-code         override: ZENITH_CODE_BIN=/path/to/zenith-code
 *
 * Both backends must print the same banner (`Token: <credential>`), honour the same flags and
 * answer `/.well-known/t3/environment` once ready. That is all this file relies on.
 */
import * as NodeChildProcess from "node:child_process";
import * as NodeFS from "node:fs";
import * as NodeNet from "node:net";
import * as NodeOS from "node:os";
import * as NodePath from "node:path";

export type Backend = "ts" | "rust";

export const CODE_ROOT = NodePath.resolve(import.meta.dirname, "../../..");
export const REPO_ROOT = NodePath.resolve(CODE_ROOT, "..");

export interface StartOptions {
  readonly backend?: Backend;
  /** Base dir (`--base-dir`). A fresh temp dir when omitted (removed on stop). */
  readonly homeDir?: string;
  /** Extra environment for the server process. */
  readonly env?: Readonly<Record<string, string>>;
  /** Written to <home>/userdata/settings.json before start (sparse ServerSettings). */
  readonly settings?: unknown;
  /** Extra `serve` arguments, placed before `--base-dir`. */
  readonly args?: ReadonlyArray<string>;
  readonly port?: number;
  readonly startupTimeoutMs?: number;
  /**
   * Point HOME at an empty temp dir, so provider CLIs see no login and nothing of the user's
   * machine leaks into answers (provider auth status, editor and shell discovery). PATH is kept.
   */
  readonly isolateHome?: boolean;
  /** Working directory of the server process (its `cwd` in getConfig). Default: the home dir. */
  readonly cwd?: string;
  /** Mirror the server's stdout/stderr to this process' stderr. */
  readonly echo?: boolean;
}

export interface CliResult {
  readonly code: number | null;
  readonly stdout: string;
  readonly stderr: string;
}

export interface ServerHandle {
  readonly backend: Backend;
  readonly httpUrl: string;
  readonly wsUrl: string;
  readonly port: number;
  readonly homeDir: string;
  /** The fake HOME of the server process (with `isolateHome`). */
  readonly userHomeDir: string | undefined;
  /** The one-time administrative pairing credential from the banner (`Token: …`). */
  readonly bootstrapCredential: string;
  readonly pid: number | undefined;
  /** Everything the server printed so far (ANSI stripped). */
  output(): string;
  /** Run the backend's CLI against the same base dir (`<args> --base-dir <home>`). */
  cli(args: ReadonlyArray<string>, options?: { env?: Record<string, string> }): Promise<CliResult>;
  /** SIGTERM, then SIGKILL after 5 s. Resolves with how it exited and how long it took. */
  stop(): Promise<{ code: number | null; signal: NodeJS.Signals | null; ms: number }>;
}

export const backendFromEnv = (): Backend => {
  const value = (process.env.BACKEND ?? "ts").toLowerCase();
  if (value !== "ts" && value !== "rust")
    throw new Error(`BACKEND must be ts or rust, got ${value}`);
  return value;
};

export const backendCommand = (backend: Backend): { command: string; prefix: Array<string> } => {
  if (backend === "rust") {
    const bin = process.env.ZENITH_CODE_BIN ?? NodePath.join(REPO_ROOT, "target/debug/zenith-code");
    if (!NodeFS.existsSync(bin)) {
      throw new Error(
        `Rust backend not built: ${bin} is missing (cargo build -p zenith-code, or set ZENITH_CODE_BIN)`,
      );
    }
    return { command: bin, prefix: [] };
  }
  const built = NodePath.join(CODE_ROOT, "apps/server/dist/bin.mjs");
  const entry =
    process.env.ZENITH_CODE_TS_ENTRY ??
    (NodeFS.existsSync(built) ? built : NodePath.join(CODE_ROOT, "apps/server/src/bin.ts"));
  return { command: process.execPath, prefix: [entry] };
};

export const freePort = (): Promise<number> =>
  new Promise((resolve, reject) => {
    const server = NodeNet.createServer();
    server.unref();
    server.on("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address() as NodeNet.AddressInfo;
      server.close(() => resolve(port));
    });
  });

// eslint-disable-next-line no-control-regex
const stripAnsi = (text: string) => text.replace(/\u001b\[[0-9;]*[A-Za-z]/g, "");

/** The environment a backend child gets: ours minus T3CODE_* (so a dev shell cannot leak in). */
const childEnv = (extra: Readonly<Record<string, string>> | undefined) => {
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (value === undefined || key.startsWith("T3CODE_") || key === "BACKEND") continue;
    env[key] = value;
  }
  return { ...env, T3CODE_NO_BROWSER: "1", NODE_ENV: "production", ...extra };
};

const waitForHttp = async (url: string, deadline: number, child: NodeChildProcess.ChildProcess) => {
  while (Date.now() < deadline) {
    if (child.exitCode !== null || child.signalCode !== null) return false;
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(2000) });
      await response.arrayBuffer();
      if (response.ok) return true;
    } catch {
      // not listening yet
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  return false;
};

export const startServer = async (options: StartOptions = {}): Promise<ServerHandle> => {
  const backend = options.backend ?? backendFromEnv();
  const { command, prefix } = backendCommand(backend);
  const ownsHome = options.homeDir === undefined;
  const homeDir =
    options.homeDir ??
    NodeFS.mkdtempSync(NodePath.join(NodeOS.tmpdir(), `zenith-code-${backend}-`));
  if (options.settings !== undefined) {
    NodeFS.mkdirSync(NodePath.join(homeDir, "userdata"), { recursive: true });
    NodeFS.writeFileSync(
      NodePath.join(homeDir, "userdata/settings.json"),
      `${JSON.stringify(options.settings, null, 2)}\n`,
    );
  }
  const port = options.port ?? (await freePort());
  let fakeUserHome: string | undefined;
  if (options.isolateHome) {
    fakeUserHome = NodeFS.mkdtempSync(NodePath.join(NodeOS.tmpdir(), "zenith-code-user-home-"));
  }
  const env = childEnv({
    ...(fakeUserHome
      ? {
          HOME: fakeUserHome,
          XDG_CONFIG_HOME: NodePath.join(fakeUserHome, ".config"),
          ZDOTDIR: fakeUserHome,
        }
      : {}),
    ...options.env,
  });
  const args = [
    ...prefix,
    "serve",
    "--host",
    "127.0.0.1",
    "--port",
    String(port),
    ...(options.args ?? []),
    "--base-dir",
    homeDir,
  ];
  const child = NodeChildProcess.spawn(command, args, {
    env,
    cwd: options.cwd ?? homeDir,
    stdio: ["ignore", "pipe", "pipe"],
  });
  let output = "";
  const onData = (chunk: Buffer) => {
    const text = chunk.toString("utf8");
    output += stripAnsi(text);
    if (options.echo) process.stderr.write(text);
  };
  child.stdout!.on("data", onData);
  child.stderr!.on("data", onData);

  const httpUrl = `http://127.0.0.1:${port}`;
  const deadline = Date.now() + (options.startupTimeoutMs ?? 60_000);
  const credential = await new Promise<string>((resolve, reject) => {
    const timer = setInterval(() => {
      const match = /^Token: (\S+)\s*$/m.exec(output);
      if (match) {
        clearInterval(timer);
        resolve(match[1]!);
      } else if (child.exitCode !== null || child.signalCode !== null) {
        clearInterval(timer);
        reject(new Error(`${backend} server exited during startup:\n${output.slice(-4000)}`));
      } else if (Date.now() > deadline) {
        clearInterval(timer);
        child.kill("SIGKILL");
        reject(
          new Error(`${backend} server did not print its banner in time:\n${output.slice(-4000)}`),
        );
      }
    }, 50);
  });
  if (!(await waitForHttp(`${httpUrl}/.well-known/t3/environment`, deadline, child))) {
    child.kill("SIGKILL");
    throw new Error(`${backend} server never answered the descriptor:\n${output.slice(-4000)}`);
  }

  let stopped:
    | Promise<{ code: number | null; signal: NodeJS.Signals | null; ms: number }>
    | undefined;
  return {
    backend,
    httpUrl,
    wsUrl: `ws://127.0.0.1:${port}/ws`,
    port,
    homeDir,
    userHomeDir: fakeUserHome ? NodeFS.realpathSync(fakeUserHome) : undefined,
    bootstrapCredential: credential,
    pid: child.pid,
    output: () => output,
    cli: (cliArgs, cliOptions) =>
      new Promise<CliResult>((resolve) => {
        const cliChild = NodeChildProcess.spawn(
          command,
          [...prefix, ...cliArgs, "--base-dir", homeDir],
          {
            env: childEnv(cliOptions?.env ?? options.env),
            stdio: ["ignore", "pipe", "pipe"],
          },
        );
        let stdout = "";
        let stderr = "";
        cliChild.stdout!.on("data", (c: Buffer) => (stdout += c.toString("utf8")));
        cliChild.stderr!.on("data", (c: Buffer) => (stderr += c.toString("utf8")));
        cliChild.on("close", (code) =>
          resolve({ code, stdout: stripAnsi(stdout), stderr: stripAnsi(stderr) }),
        );
      }),
    stop: () => {
      stopped ??= new Promise((resolve) => {
        const started = Date.now();
        const finish = () => {
          clearTimeout(killer);
          if (ownsHome) NodeFS.rmSync(homeDir, { recursive: true, force: true });
          if (fakeUserHome) NodeFS.rmSync(fakeUserHome, { recursive: true, force: true });
          resolve({ code: child.exitCode, signal: child.signalCode, ms: Date.now() - started });
        };
        const killer = setTimeout(() => child.kill("SIGKILL"), 5_000);
        if (child.exitCode !== null || child.signalCode !== null) return finish();
        child.once("exit", finish);
        child.kill("SIGTERM");
      });
      return stopped;
    },
  };
};

export const ServerUnderTest = { start: startServer };
