/**
 * Auth state compatibility on a copy of a real `~/.zenith/code` (plan §9.4a/c): the sessions in
 * its `auth_sessions`, signed with its `server-signing-key.bin` the way the TS server signs them,
 * must authenticate on the Rust server, and a cookie the Rust server issues there must verify
 * with the TS code.
 *
 *   node scripts/compat/auth-live-copy.ts [--source ~/.zenith/code]
 *
 * The source is only read (plain file copies: database, WAL, environment id, signing key) into
 * a private temp dir that is deleted at the end. Nothing secret is printed: only counts.
 */
import * as NodeAssert from "node:assert/strict";
import * as NodeFS from "node:fs";
import * as NodeOS from "node:os";
import * as NodePath from "node:path";
import * as NodeSqlite from "node:sqlite";

import { CODE_ROOT, freePort, startServer } from "./lib/serverUnderTest.ts";
import { bootstrapBrowserSession, http } from "./lib/client.ts";

const utils = (await import(
  NodePath.join(CODE_ROOT, "apps/server/src/auth/utils.ts")
)) as typeof import("../../apps/server/src/auth/utils.ts");

const sourceArg = process.argv.indexOf("--source");
const source = NodePath.resolve(
  sourceArg === -1 ? NodePath.join(NodeOS.homedir(), ".zenith/code") : process.argv[sourceArg + 1]!,
);
const copy = NodeFS.mkdtempSync(NodePath.join(NodeOS.tmpdir(), "zenith-code-auth-copy-"));
NodeFS.chmodSync(copy, 0o700);
const log = (line: string) => process.stdout.write(`${line}\n`);

const FILES = [
  "userdata/state.sqlite",
  "userdata/state.sqlite-wal",
  "userdata/state.sqlite-shm",
  "userdata/environment-id",
  "userdata/secrets/server-signing-key.bin",
];

let failed = false;
try {
  for (const relative of FILES) {
    const from = NodePath.join(source, relative);
    if (!NodeFS.existsSync(from)) continue;
    const to = NodePath.join(copy, relative);
    NodeFS.mkdirSync(NodePath.dirname(to), { recursive: true, mode: 0o700 });
    NodeFS.copyFileSync(from, to);
  }
  const key = NodeFS.readFileSync(NodePath.join(copy, "userdata/secrets/server-signing-key.bin"));
  const db = new NodeSqlite.DatabaseSync(NodePath.join(copy, "userdata/state.sqlite"));
  const now = Date.now();
  const rows = db
    .prepare(
      `SELECT session_id, subject, scopes, method, issued_at, expires_at
       FROM auth_sessions WHERE revoked_at IS NULL AND expires_at > ?`,
    )
    .all(new Date(now).toISOString()) as Array<{
    session_id: string;
    subject: string;
    scopes: string;
    method: string;
    issued_at: string;
    expires_at: string;
  }>;
  const total = (db.prepare("SELECT count(*) AS n FROM auth_sessions").get() as { n: number }).n;
  db.close();
  log(`copy has ${total} auth_sessions rows, ${rows.length} live`);
  NodeAssert.ok(rows.length > 0, "no live session in the copy");

  // The TS signing path: base64url(JSON.stringify(claims)) "." signPayload(…, key).
  const mint = (row: (typeof rows)[number]) => {
    const claims = {
      v: 1,
      kind: "session",
      sid: row.session_id,
      sub: row.subject,
      scopes: JSON.parse(row.scopes) as Array<string>,
      method: row.method,
      iat: Date.parse(row.issued_at),
      exp: Date.parse(row.expires_at),
    };
    const payload = utils.base64UrlEncode(JSON.stringify(claims));
    return { token: `${payload}.${utils.signPayload(payload, key)}`, scopes: claims.scopes };
  };

  const port = await freePort();
  const server = await startServer({ backend: "rust", homeDir: copy, port, isolateHome: true });
  try {
    const state = await http<{ auth: { sessionCookieName: string } }>(
      `${server.httpUrl}/api/auth/session`,
    );
    const expectedName = utils.resolveSessionCookieName({
      mode: "web",
      port,
      host: "127.0.0.1",
      instanceKey: NodePath.join(copy, "userdata"),
      environmentId: NodeFS.readFileSync(
        NodePath.join(copy, "userdata/environment-id"),
        "utf8",
      ).trim(),
      development: false,
    });
    NodeAssert.equal(state.body.auth.sessionCookieName, expectedName, "cookie name");
    log("ok  the Rust cookie name is the TS one for this state dir and port");

    let accepted = 0;
    for (const row of rows) {
      const { token, scopes } = mint(row);
      const headers =
        row.method === "browser-session-cookie"
          ? { cookie: `${expectedName}=${token}` }
          : { authorization: `Bearer ${token}` };
      if (row.method === "dpop-access-token") continue; // needs a proof from the client's key
      const res = await http<{
        authenticated: boolean;
        scopes?: Array<string>;
        sessionMethod?: string;
      }>(`${server.httpUrl}/api/auth/session`, { headers });
      NodeAssert.equal(res.body.authenticated, true, `a live ${row.method} session was rejected`);
      NodeAssert.deepEqual(res.body.scopes, scopes);
      NodeAssert.equal(res.body.sessionMethod, row.method);
      accepted += 1;
    }
    log(`ok  ${accepted} live sessions signed with the copy's key authenticate on Rust`);

    // The other way: a cookie the Rust server issues verifies with the TS code.
    const issued = await server.cli(["auth", "pairing", "create", "--admin", "--json"]);
    NodeAssert.equal(issued.code, 0, issued.stderr);
    const credential = (JSON.parse(issued.stdout) as { credential: string }).credential;
    const boot = await bootstrapBrowserSession(server.httpUrl, credential);
    NodeAssert.equal(boot.status, 200, boot.text);
    const cookieToken = boot.cookie!.slice(boot.cookie!.indexOf("=") + 1);
    const [payload, signature] = cookieToken.split(".");
    NodeAssert.ok(
      utils.timingSafeEqualBase64Url(signature!, utils.signPayload(payload!, key)),
      "the Rust cookie's signature does not verify with the TS code",
    );
    const claims = JSON.parse(utils.base64UrlDecodeUtf8(payload!)) as Record<string, unknown>;
    NodeAssert.deepEqual(Object.keys(claims), [
      "v",
      "kind",
      "sid",
      "sub",
      "scopes",
      "method",
      "iat",
      "exp",
    ]);
    NodeAssert.equal(claims.method, "browser-session-cookie");
    log("ok  a Rust-issued cookie verifies with the TS signing code and the copy's key");
  } finally {
    await server.stop();
  }
  log("all live-copy checks passed");
} catch (error) {
  failed = true;
  process.stderr.write(`FAILED: ${error instanceof Error ? error.message : String(error)}\n`);
} finally {
  NodeFS.rmSync(copy, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
