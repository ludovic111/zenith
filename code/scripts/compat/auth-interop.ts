/**
 * Auth interop between the TS and the Rust backends on one base dir (plan §9.4c): whatever one
 * side issues, the other accepts.
 *
 *   node scripts/compat/auth-interop.ts [--home <base dir>]
 *
 * With `--home`, it runs on that directory (pass a COPY of a real `~/.zenith/code`, never the
 * live one); otherwise on a fresh temp dir. On one fixed port, in turn:
 *
 * 1. both CLIs mint a bearer session and an administrative pairing credential;
 * 2. Rust serves: both bearers authenticate, the TS credential becomes a Rust browser cookie,
 *    the TS bearer gets a Rust websocket ticket;
 * 3. TS serves: same cookie name as Rust, the Rust bearer and the Rust cookie authenticate,
 *    the Rust ticket opens a socket, the Rust credential becomes a TS cookie, and TS mints a
 *    ticket;
 * 4. Rust serves again: the TS cookie authenticates and the TS ticket opens a socket.
 *
 * Exits 1 on the first mismatch. Needs the Rust binary (`cargo build -p zenith-code`).
 */
import * as NodeAssert from "node:assert/strict";
import * as NodeFS from "node:fs";
import * as NodeOS from "node:os";
import * as NodePath from "node:path";

import { bootstrapBrowserSession, http, RawRpcClient } from "./lib/client.ts";
import { SYNTHETIC_SETTINGS } from "./lib/fixtures.ts";
import { freePort, startServer, type Backend, type ServerHandle } from "./lib/serverUnderTest.ts";

const homeArg = process.argv.indexOf("--home");
const ownsHome = homeArg === -1;
const homeDir = ownsHome
  ? NodeFS.mkdtempSync(NodePath.join(NodeOS.tmpdir(), "zenith-code-interop-"))
  : NodePath.resolve(process.argv[homeArg + 1]!);
if (!ownsHome && NodePath.resolve(homeDir) === NodePath.join(NodeOS.homedir(), ".zenith/code")) {
  throw new Error("refusing to run on the live ~/.zenith/code: pass a copy");
}
const port = await freePort();
const log = (line: string) => process.stdout.write(`${line}\n`);

const start = (backend: Backend) =>
  startServer({
    backend,
    homeDir,
    port,
    isolateHome: true,
    ...(ownsHome ? { settings: SYNTHETIC_SETTINGS } : {}),
  });

const cliJson = async (server: ServerHandle, args: Array<string>) => {
  const result = await server.cli(args);
  NodeAssert.equal(result.code, 0, `${args.join(" ")}: ${result.stderr}`);
  return JSON.parse(result.stdout.slice(result.stdout.indexOf("{"))) as Record<string, unknown>;
};

const sessionState = (server: ServerHandle, headers: Record<string, string>) =>
  http<{ authenticated: boolean; auth: { sessionCookieName: string }; scopes?: Array<string> }>(
    `${server.httpUrl}/api/auth/session`,
    { headers },
  );

const expectAuthenticated = async (
  server: ServerHandle,
  what: string,
  headers: Record<string, string>,
) => {
  const state = await sessionState(server, headers);
  NodeAssert.equal(state.body.authenticated, true, `${server.backend} rejects ${what}`);
  log(`ok  ${server.backend} accepts ${what}`);
};

const expectSocket = async (server: ServerHandle, what: string, ticket: string) => {
  const rpc = await RawRpcClient.connect(`${server.wsUrl}?wsTicket=${encodeURIComponent(ticket)}`);
  const access = rpc.stream("subscribeAuthAccess", {});
  const [snapshot] = (await access.waitForValues(1)) as Array<{ type: string }>;
  NodeAssert.equal(snapshot?.type, "snapshot");
  await rpc.close();
  log(`ok  ${server.backend} opens a socket with ${what}`);
};

const ticketFor = async (server: ServerHandle, bearer: string) => {
  const res = await http<{ ticket: string }>(`${server.httpUrl}/api/auth/websocket-ticket`, {
    method: "POST",
    headers: { authorization: `Bearer ${bearer}` },
  });
  NodeAssert.equal(res.status, 200, res.text);
  return res.body.ticket;
};

const cookieFrom = async (server: ServerHandle, credential: string) => {
  const boot = await bootstrapBrowserSession(server.httpUrl, credential);
  NodeAssert.equal(boot.status, 200, `${server.backend} browser-session: ${boot.text}`);
  return boot.cookie!;
};

let failed = false;
try {
  log(`base dir ${homeDir}, port ${port}`);
  // 1. Mint with both CLIs (the server is only started to reach `cli`).
  let rust = await start("rust");
  const rustBearer = (await cliJson(rust, ["auth", "session", "issue", "--json"])).token as string;
  const rustCredential = (await cliJson(rust, ["auth", "pairing", "create", "--admin", "--json"]))
    .credential as string;
  await rust.stop();
  let ts = await start("ts");
  const tsBearer = (await cliJson(ts, ["auth", "session", "issue", "--json"])).token as string;
  const tsCredential = (await cliJson(ts, ["auth", "pairing", "create", "--admin", "--json"]))
    .credential as string;
  const tsCookieName = (await sessionState(ts, {})).body.auth.sessionCookieName;
  await ts.stop();
  log("ok  both CLIs minted a bearer session and a pairing credential");

  // 2. Rust serves.
  rust = await start("rust");
  const rustCookieName = (await sessionState(rust, {})).body.auth.sessionCookieName;
  NodeAssert.equal(rustCookieName, tsCookieName, "cookie names differ");
  log(`ok  same cookie name on both: ${rustCookieName}`);
  await expectAuthenticated(rust, "a TS CLI bearer token", { authorization: `Bearer ${tsBearer}` });
  await expectAuthenticated(rust, "a Rust CLI bearer token", {
    authorization: `Bearer ${rustBearer}`,
  });
  const rustCookie = await cookieFrom(rust, tsCredential);
  log("ok  rust consumes a TS CLI pairing credential");
  const rustTicket = await ticketFor(rust, tsBearer);
  await rust.stop();

  // 3. TS serves.
  ts = await start("ts");
  await expectAuthenticated(ts, "a Rust CLI bearer token", {
    authorization: `Bearer ${rustBearer}`,
  });
  await expectAuthenticated(ts, "a Rust-issued session cookie", { cookie: rustCookie });
  await expectSocket(ts, "a Rust-issued websocket ticket", rustTicket);
  const tsCookie = await cookieFrom(ts, rustCredential);
  log("ok  ts consumes a Rust CLI pairing credential");
  const tsTicket = await ticketFor(ts, rustBearer);
  await ts.stop();

  // 4. Rust serves again.
  rust = await start("rust");
  await expectAuthenticated(rust, "a TS-issued session cookie", { cookie: tsCookie });
  await expectSocket(rust, "a TS-issued websocket ticket", tsTicket);
  await rust.stop();
  log("all interop checks passed");
} catch (error) {
  failed = true;
  process.stderr.write(`FAILED: ${error instanceof Error ? error.message : String(error)}\n`);
} finally {
  if (ownsHome) NodeFS.rmSync(homeDir, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
