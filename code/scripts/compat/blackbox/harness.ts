/**
 * node:test glue for the black-box suite.
 *
 *   describe("…", () => {
 *     const ctx = useServer();            // one backend per describe block (BACKEND=ts|rust)
 *     test("…", async () => { … ctx.httpUrl … });
 *   });
 *
 * Unless COMPAT_PROXY=0, every request goes through the validating proxy, and the block fails in
 * its `after` hook if the proxy saw a schema/protocol error: the suite doubles as a conformance
 * run. Set COMPAT_ECHO=1 to see the server's output.
 */
import * as NodeTest from "node:test";
import * as NodeAssert from "node:assert/strict";
import {
  bootstrapBrowserSession,
  exchangeAccessToken,
  issuePairingCredential,
} from "../lib/client.ts";
import { formatIssue, startProxy, type RunningProxy } from "../lib/proxy.ts";
import { SYNTHETIC_SETTINGS } from "../lib/fixtures.ts";
import { startServer, type ServerHandle, type StartOptions } from "../lib/serverUnderTest.ts";

export interface ServerContext {
  /** Base URL the tests talk to (the proxy, or the backend itself with COMPAT_PROXY=0). */
  readonly httpUrl: string;
  readonly wsUrl: string;
  readonly server: ServerHandle;
  readonly proxy: RunningProxy | undefined;
  /** A browser-session cookie with administrative scopes (consumes the bootstrap credential once). */
  adminCookie(): Promise<string>;
  /** A fresh one-time pairing credential (administrative scopes unless `scopes` is given). */
  newCredential(input?: { label?: string; scopes?: ReadonlyArray<string> }): Promise<string>;
  /** A fresh browser-session cookie from a fresh pairing credential. */
  newSessionCookie(input?: { label?: string; scopes?: ReadonlyArray<string> }): Promise<string>;
  /** A fresh bearer access token from a fresh pairing credential. */
  newBearerToken(scope?: string): Promise<string>;
}

const ADMIN_SCOPES = [
  "orchestration:read",
  "orchestration:operate",
  "terminal:operate",
  "review:write",
  "relay:read",
  "access:read",
  "access:write",
  "relay:write",
];

export const useServer = (options: StartOptions = {}): ServerContext => {
  let server: ServerHandle | undefined;
  let proxy: RunningProxy | undefined;
  let adminCookie: Promise<string> | undefined;
  const useProxy = process.env.COMPAT_PROXY !== "0";

  NodeTest.before(async () => {
    // Hermetic by default: empty HOME, no provider probed (see fixtures.ts SYNTHETIC_SETTINGS).
    server = await startServer({
      echo: process.env.COMPAT_ECHO === "1",
      isolateHome: true,
      settings: SYNTHETIC_SETTINGS,
      ...options,
    });
    if (useProxy) proxy = await startProxy({ target: server.httpUrl, strict: false });
  });

  NodeTest.after(async () => {
    await proxy?.close();
    const result = await server?.stop();
    if (process.env.COMPAT_ECHO === "1")
      process.stderr.write(`server stopped: ${JSON.stringify(result)}\n`);
    if (process.env.COMPAT_WARNINGS === "1") {
      for (const issue of proxy?.issues ?? []) {
        if (issue.severity === "warning") process.stderr.write(`${formatIssue(issue)}\n`);
      }
    }
    const errors = proxy?.issues.filter((issue) => issue.severity === "error") ?? [];
    NodeAssert.equal(
      errors.length,
      0,
      `validating proxy saw errors:\n${errors.map(formatIssue).join("\n")}`,
    );
  });

  const ctx: ServerContext = {
    get httpUrl() {
      return proxy?.url ?? server!.httpUrl;
    },
    get wsUrl() {
      return proxy?.wsUrl ?? server!.wsUrl;
    },
    get server() {
      return server!;
    },
    get proxy() {
      return proxy;
    },
    adminCookie: () => {
      adminCookie ??= bootstrapBrowserSession(ctx.httpUrl, server!.bootstrapCredential).then(
        (r) => {
          NodeAssert.equal(r.status, 200, `bootstrap failed: ${r.text}`);
          return r.cookie!;
        },
      );
      return adminCookie;
    },
    newCredential: async (input = {}) =>
      (
        await issuePairingCredential(ctx.httpUrl, await ctx.adminCookie(), {
          ...input,
          scopes: input.scopes ?? ADMIN_SCOPES,
        })
      ).credential,
    newSessionCookie: async (input) => {
      const result = await bootstrapBrowserSession(ctx.httpUrl, await ctx.newCredential(input));
      NodeAssert.equal(result.status, 200, result.text);
      return result.cookie!;
    },
    newBearerToken: async (scope) => {
      const result = await exchangeAccessToken(
        ctx.httpUrl,
        await ctx.newCredential(),
        scope ? { scope } : {},
      );
      NodeAssert.equal(result.status, 200, result.text);
      return result.body.access_token!;
    },
  };
  return ctx;
};

export const splitHeaderTokens = (value: string | null | undefined) =>
  (value ?? "")
    .split(",")
    .map((token) => token.trim())
    .filter((token) => token.length > 0)
    .toSorted();

/** `assertBrowserApiCorsResponseHeaders` from server.test.ts. */
export const assertCorsResponseHeaders = (
  headers: Readonly<Record<string, string | undefined>>,
  options?: { origin?: string; credentials?: boolean },
) => {
  NodeAssert.equal(headers["access-control-allow-origin"], options?.origin ?? "*");
  NodeAssert.equal(
    headers["access-control-allow-credentials"],
    options?.credentials ? "true" : undefined,
  );
};

/** `assertBrowserApiCorsPreflightHeaders` from server.test.ts. */
export const assertCorsPreflightHeaders = (
  headers: Readonly<Record<string, string | undefined>>,
  options?: { origin?: string; credentials?: boolean },
) => {
  assertCorsResponseHeaders(headers, options);
  NodeAssert.deepEqual(splitHeaderTokens(headers["access-control-allow-methods"]), [
    "GET",
    "OPTIONS",
    "POST",
  ]);
  NodeAssert.deepEqual(splitHeaderTokens(headers["access-control-allow-headers"]), [
    "authorization",
    "b3",
    "content-type",
    "dpop",
    "traceparent",
  ]);
};

export const crossOriginClientOrigin = "http://remote-client.test:3773";
