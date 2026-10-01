/**
 * Auth, sessions, cookies, pairing and access management, black-box.
 * Ported from apps/server/src/server.test.ts ("server router seam"); the line number of the
 * original test is given for each. The originals run in desktop mode with a reusable desktop
 * bootstrap token; here the backend runs like zenith runs it (`serve` on loopback: policy
 * `loopback-browser`, one-time pairing credentials), so expectations that depend on the mode were
 * adapted and say so.
 */
import * as NodeTest from "node:test";
import * as NodeAssert from "node:assert/strict";
import {
  ADMIN_SCOPES,
  bootstrapBrowserSession,
  cookiePair,
  exchangeAccessToken,
  http,
} from "../lib/client.ts";
import {
  assertCorsPreflightHeaders,
  assertCorsResponseHeaders,
  crossOriginClientOrigin,
  useServer,
} from "./harness.ts";

const STANDARD_SCOPES = [
  "orchestration:read",
  "orchestration:operate",
  "terminal:operate",
  "review:write",
  "relay:read",
];

type AuthError = {
  _tag: string;
  code: string;
  reason?: string;
  requiredScope?: string;
  traceId: string;
};

NodeTest.describe("auth: sessions and cookies", () => {
  const ctx = useServer();

  // server.test.ts:2257 (adapted: web/loopback policy instead of desktop-managed-local)
  NodeTest.test("reports unauthenticated session state without requiring auth", async () => {
    const res = await http<{
      authenticated: boolean;
      auth: {
        policy: string;
        bootstrapMethods: Array<string>;
        sessionMethods: Array<string>;
        sessionCookieName: string;
      };
    }>(`${ctx.httpUrl}/api/auth/session`);
    NodeAssert.equal(res.status, 200);
    NodeAssert.equal(res.body.authenticated, false);
    NodeAssert.equal(res.body.auth.policy, "loopback-browser");
    NodeAssert.deepEqual(res.body.auth.bootstrapMethods, ["one-time-token"]);
    NodeAssert.deepEqual(res.body.auth.sessionMethods, [
      "browser-session-cookie",
      "bearer-access-token",
      "dpop-access-token",
    ]);
    // Plan §2.4: web mode on loopback → t3_session_<port>_<sha256(stateDir)[0..12]>.
    NodeAssert.match(
      res.body.auth.sessionCookieName,
      new RegExp(`^t3_session_${ctx.server.port}_[0-9a-f]{12}$`),
    );
  });

  // server.test.ts:2288
  NodeTest.test(
    "bootstraps a browser session and authenticates the session endpoint via cookie",
    async () => {
      const credential = await ctx.newCredential();
      const boot = await bootstrapBrowserSession(ctx.httpUrl, credential);
      NodeAssert.equal(boot.status, 200);
      NodeAssert.equal(boot.body.authenticated, true);
      NodeAssert.equal(boot.body.sessionMethod, "browser-session-cookie");
      NodeAssert.equal((boot.body as { sessionToken?: string }).sessionToken, undefined);
      NodeAssert.ok(boot.setCookie);

      const session = await http<{ authenticated: boolean; sessionMethod?: string }>(
        `${ctx.httpUrl}/api/auth/session`,
        { headers: { cookie: boot.cookie! } },
      );
      NodeAssert.equal(session.status, 200);
      NodeAssert.equal(session.body.authenticated, true);
      NodeAssert.equal(session.body.sessionMethod, "browser-session-cookie");
    },
  );

  // Plan §2.4: cookie attributes and no-store on credential responses.
  NodeTest.test(
    "sets the session cookie with HttpOnly, Path=/, SameSite=Lax and an expiry",
    async () => {
      const boot = await bootstrapBrowserSession(ctx.httpUrl, await ctx.newCredential());
      const state = await http<{ auth: { sessionCookieName: string } }>(
        `${ctx.httpUrl}/api/auth/session`,
      );
      const setCookie = boot.setCookie!;
      NodeAssert.ok(setCookie.startsWith(`${state.body.auth.sessionCookieName}=`), setCookie);
      const attributes = setCookie
        .split(";")
        .slice(1)
        .map((part) => part.trim().toLowerCase());
      NodeAssert.ok(attributes.includes("httponly"), setCookie);
      NodeAssert.ok(attributes.includes("path=/"), setCookie);
      NodeAssert.ok(attributes.includes("samesite=lax"), setCookie);
      NodeAssert.ok(
        attributes.some((a) => a.startsWith("expires=")),
        setCookie,
      );
      NodeAssert.ok(!attributes.includes("secure"), setCookie);
      NodeAssert.equal(boot.headers["cache-control"], "no-store");
    },
  );

  // Adapted from server.test.ts:5295 (desktop tokens are reusable; serve's are one-time).
  NodeTest.test("one-time pairing credentials cannot be reused", async () => {
    const credential = await ctx.newCredential();
    const first = await bootstrapBrowserSession(ctx.httpUrl, credential);
    const second = await bootstrapBrowserSession(ctx.httpUrl, credential);
    NodeAssert.equal(first.status, 200);
    NodeAssert.equal(second.status, 401);
    const body = second.body as unknown as AuthError;
    NodeAssert.equal(body._tag, "EnvironmentAuthInvalidError");
    NodeAssert.equal(body.code, "auth_invalid");
    NodeAssert.equal(typeof body.traceId, "string");
  });

  NodeTest.test("the banner's bootstrap credential is administrative and one-time", async () => {
    await ctx.adminCookie(); // consumes it
    const again = await bootstrapBrowserSession(ctx.httpUrl, ctx.server.bootstrapCredential);
    NodeAssert.equal(again.status, 401);
    const session = await http<{ scopes: Array<string> }>(`${ctx.httpUrl}/api/auth/session`, {
      headers: { cookie: await ctx.adminCookie() },
    });
    NodeAssert.deepEqual(session.body.scopes, ADMIN_SCOPES);
  });

  NodeTest.test("rejects an unknown credential with a typed 401", async () => {
    const res = await bootstrapBrowserSession(ctx.httpUrl, "ABCDEFGHJKLM");
    NodeAssert.equal(res.status, 401);
    const body = res.body as unknown as AuthError;
    NodeAssert.equal(body._tag, "EnvironmentAuthInvalidError");
    NodeAssert.equal(body.code, "auth_invalid");
    NodeAssert.equal(res.setCookies.length, 0);
  });

  // server.test.ts:2363
  NodeTest.test("exchanges a pairing credential for a scoped bearer access token", async () => {
    const token = await exchangeAccessToken(ctx.httpUrl, await ctx.newCredential());
    NodeAssert.equal(token.status, 200);
    NodeAssert.equal(token.body.issued_token_type, "urn:ietf:params:oauth:token-type:access_token");
    NodeAssert.equal(token.body.token_type, "Bearer");
    NodeAssert.equal(token.body.scope, ADMIN_SCOPES.join(" "));
    NodeAssert.equal(typeof token.body.access_token, "string");
    NodeAssert.equal(typeof token.body.expires_in, "number");

    const session = await http<{
      authenticated: boolean;
      sessionMethod?: string;
      scopes?: Array<string>;
    }>(`${ctx.httpUrl}/api/auth/session`, {
      headers: { authorization: `Bearer ${token.body.access_token}` },
    });
    NodeAssert.equal(session.status, 200);
    NodeAssert.equal(session.body.authenticated, true);
    NodeAssert.equal(session.body.sessionMethod, "bearer-access-token");
    NodeAssert.deepEqual(session.body.scopes, ADMIN_SCOPES);
  });

  NodeTest.test(
    "an invalid bearer token reads as unauthenticated (session endpoint is always 200)",
    async () => {
      const res = await http<{ authenticated: boolean }>(`${ctx.httpUrl}/api/auth/session`, {
        headers: { authorization: "Bearer not-a-token" },
      });
      NodeAssert.equal(res.status, 200);
      NodeAssert.equal(res.body.authenticated, false);
    },
  );

  // server.test.ts:4531
  NodeTest.test(
    "issues short-lived websocket tickets for authenticated bearer sessions",
    async () => {
      const bearer = await ctx.newBearerToken();
      const res = await http<{ ticket: string; expiresAt: string }>(
        `${ctx.httpUrl}/api/auth/websocket-ticket`,
        { method: "POST", headers: { authorization: `Bearer ${bearer}` } },
      );
      NodeAssert.equal(res.status, 200);
      NodeAssert.equal(typeof res.body.ticket, "string");
      NodeAssert.ok(res.body.ticket.length > 0);
      NodeAssert.equal(typeof res.body.expiresAt, "string");
      const ttl = Date.parse(res.body.expiresAt) - Date.now();
      NodeAssert.ok(ttl > 0 && ttl <= 5 * 60_000 + 5_000, `ticket ttl ${ttl}`);
    },
  );
});

NodeTest.describe("auth: CORS and error shapes", () => {
  const ctx = useServer();

  // server.test.ts:2239
  NodeTest.test("includes CORS headers on public environment descriptor responses", async () => {
    const res = await http(`${ctx.httpUrl}/.well-known/t3/environment`, {
      headers: { origin: crossOriginClientOrigin },
    });
    NodeAssert.equal(res.status, 200);
    assertCorsResponseHeaders(res.headers);
  });

  // server.test.ts:4603
  NodeTest.test("includes CORS headers on remote auth success responses", async () => {
    const origin = crossOriginClientOrigin;
    const token = await exchangeAccessToken(ctx.httpUrl, await ctx.newCredential(), {
      headers: { origin },
    });
    NodeAssert.equal(token.status, 200);
    assertCorsResponseHeaders(token.headers);
    NodeAssert.equal(token.body.token_type, "Bearer");

    const session = await http<{ authenticated: boolean; sessionMethod?: string }>(
      `${ctx.httpUrl}/api/auth/session`,
      { headers: { authorization: `Bearer ${token.body.access_token}`, origin } },
    );
    NodeAssert.equal(session.status, 200);
    assertCorsResponseHeaders(session.headers);
    NodeAssert.equal(session.body.sessionMethod, "bearer-access-token");

    const ticket = await http<{ ticket: string }>(`${ctx.httpUrl}/api/auth/websocket-ticket`, {
      method: "POST",
      headers: { authorization: `Bearer ${token.body.access_token}`, origin },
    });
    NodeAssert.equal(ticket.status, 200);
    assertCorsResponseHeaders(ticket.headers);
    NodeAssert.equal(typeof ticket.body.ticket, "string");
  });

  // server.test.ts:4655
  NodeTest.test(
    "responds to websocket-ticket preflight requests with authorization CORS headers",
    async () => {
      const res = await http(`${ctx.httpUrl}/api/auth/websocket-ticket`, {
        method: "OPTIONS",
        headers: {
          origin: crossOriginClientOrigin,
          "access-control-request-method": "POST",
          "access-control-request-headers": "authorization",
        },
      });
      NodeAssert.equal(res.status, 204);
      assertCorsPreflightHeaders(res.headers);
    },
  );

  // server.test.ts:5736
  NodeTest.test("responds to browser OTLP trace preflight requests with CORS headers", async () => {
    const res = await http(`${ctx.httpUrl}/api/observability/v1/traces`, {
      method: "OPTIONS",
      headers: {
        origin: "http://localhost:5733",
        "access-control-request-method": "POST",
        "access-control-request-headers": "content-type",
      },
    });
    NodeAssert.equal(res.status, 204);
    assertCorsPreflightHeaders(res.headers);
  });

  // server.test.ts:4754
  NodeTest.test("includes CORS headers on websocket-ticket auth failures", async () => {
    const res = await http<AuthError>(`${ctx.httpUrl}/api/auth/websocket-ticket`, {
      method: "POST",
      headers: { origin: crossOriginClientOrigin },
    });
    NodeAssert.equal(res.status, 401);
    assertCorsResponseHeaders(res.headers);
    NodeAssert.equal(res.body._tag, "EnvironmentAuthInvalidError");
    NodeAssert.equal(res.body.code, "auth_invalid");
    NodeAssert.equal(res.body.reason, "missing_credential");
    NodeAssert.equal(typeof res.body.traceId, "string");
  });

  NodeTest.test(
    "protected HTTP endpoints answer 401 missing_credential without a session",
    async () => {
      for (const [method, path] of [
        ["GET", "/api/orchestration/shell"],
        ["GET", "/api/orchestration/snapshot"],
        ["GET", "/api/auth/clients"],
        ["GET", "/api/auth/pairing-links"],
        ["POST", "/api/auth/clients/revoke-others"],
      ] as const) {
        const res = await http<AuthError>(`${ctx.httpUrl}${path}`, { method });
        NodeAssert.equal(res.status, 401, `${method} ${path}`);
        NodeAssert.equal(res.body._tag, "EnvironmentAuthInvalidError", `${method} ${path}`);
        NodeAssert.equal(res.body.reason, "missing_credential", `${method} ${path}`);
      }
    },
  );

  NodeTest.test("an unknown thread snapshot is a typed 404", async () => {
    const res = await http<{ _tag: string }>(
      `${ctx.httpUrl}/api/orchestration/threads/${encodeURIComponent("thread-does-not-exist")}`,
      { headers: { cookie: await ctx.adminCookie() } },
    );
    NodeAssert.equal(res.status, 404);
    NodeAssert.equal(res.body._tag, "EnvironmentResourceNotFoundError");
  });
});

NodeTest.describe("auth: pairing and access management", () => {
  const ctx = useServer();

  // server.test.ts:4555
  NodeTest.test(
    "does not allow management-only access tokens to operate the environment",
    async () => {
      const token = await exchangeAccessToken(ctx.httpUrl, await ctx.newCredential(), {
        scope: "access:write",
      });
      NodeAssert.equal(token.status, 200);
      NodeAssert.equal(token.body.scope, "access:write");
      const authorization = `Bearer ${token.body.access_token}`;

      const overbroad = await http<AuthError>(`${ctx.httpUrl}/api/auth/pairing-token`, {
        method: "POST",
        headers: { authorization },
        json: {},
      });
      NodeAssert.equal(overbroad.status, 403);
      NodeAssert.equal(overbroad.body.requiredScope, "orchestration:read");

      const narrow = await http(`${ctx.httpUrl}/api/auth/pairing-token`, {
        method: "POST",
        headers: { authorization },
        json: { scopes: ["access:write"] },
      });
      NodeAssert.equal(narrow.status, 200);

      const ticket = await http<{ ticket: string }>(`${ctx.httpUrl}/api/auth/websocket-ticket`, {
        method: "POST",
        headers: { authorization },
      });
      NodeAssert.equal(ticket.status, 200);
      // The RPC side of this test (getConfig fails with EnvironmentAuthorizationError) is in ws.test.ts.
    },
  );

  // server.test.ts:4781
  NodeTest.test(
    "issues authenticated one-time pairing credentials for additional clients",
    async () => {
      const res = await http<{ credential: string; expiresAt: string }>(
        `${ctx.httpUrl}/api/auth/pairing-token`,
        { method: "POST", headers: { cookie: await ctx.adminCookie() }, json: {} },
      );
      NodeAssert.equal(res.status, 200);
      NodeAssert.equal(typeof res.body.credential, "string");
      // Plan §2.3: 12 chars from 23456789ABCDEFGHJKLMNPQRSTUVWXYZ.
      NodeAssert.match(res.body.credential, /^[23456789ABCDEFGHJKLMNPQRSTUVWXYZ]{12}$/);
      NodeAssert.equal(typeof res.body.expiresAt, "string");
      // Note: unlike browser-session, the TS server sends no cache-control on this response.

      NodeAssert.equal(
        (await bootstrapBrowserSession(ctx.httpUrl, res.body.credential)).status,
        200,
      );
      NodeAssert.equal(
        (await bootstrapBrowserSession(ctx.httpUrl, res.body.credential)).status,
        401,
      );
    },
  );

  // server.test.ts:4809
  NodeTest.test(
    "issues pairing credentials for bearer sessions with access management scope",
    async () => {
      const bearer = await ctx.newBearerToken();
      const res = await http<{ credential: string; label?: string }>(
        `${ctx.httpUrl}/api/auth/pairing-token`,
        {
          method: "POST",
          headers: { authorization: `Bearer ${bearer}` },
          json: { label: "Hosted web" },
        },
      );
      NodeAssert.equal(res.status, 200);
      NodeAssert.ok(res.body.credential.length > 0);
      NodeAssert.equal(res.body.label, "Hosted web");
    },
  );

  // server.test.ts:4831
  NodeTest.test("rejects pairing credentials with an empty scope grant", async () => {
    const res = await http<AuthError>(`${ctx.httpUrl}/api/auth/pairing-token`, {
      method: "POST",
      headers: { cookie: await ctx.adminCookie() },
      json: { scopes: [] },
    });
    NodeAssert.equal(res.status, 400);
    NodeAssert.equal(res.body.code, "invalid_request");
    NodeAssert.equal(res.body.reason, "invalid_scope");
  });

  // server.test.ts:4852
  NodeTest.test("rejects unauthenticated pairing credential requests", async () => {
    const res = await http(`${ctx.httpUrl}/api/auth/pairing-token`, { method: "POST", json: {} });
    NodeAssert.equal(res.status, 401);
  });

  // server.test.ts:4863
  NodeTest.test("returns only pairing metadata to access-read HTTP sessions", async () => {
    const reader = await exchangeAccessToken(ctx.httpUrl, await ctx.newCredential(), {
      scope: "access:read",
    });
    NodeAssert.equal(reader.status, 200);
    NodeAssert.equal(reader.body.scope, "access:read");
    const created = await http<{ id: string; credential: string }>(
      `${ctx.httpUrl}/api/auth/pairing-token`,
      {
        method: "POST",
        headers: { cookie: await ctx.adminCookie() },
        json: { label: "Synthetic phone" },
      },
    );
    NodeAssert.equal(created.status, 200);

    const list = await http<Array<{ id: string; label?: string; scopes: Array<string> }>>(
      `${ctx.httpUrl}/api/auth/pairing-links`,
      { headers: { authorization: `Bearer ${reader.body.access_token}` } },
    );
    NodeAssert.equal(list.status, 200);
    NodeAssert.ok(!list.text.includes('"credential"'));
    NodeAssert.ok(!list.text.includes(created.body.credential));
    const listed = list.body.find((link) => link.id === created.body.id);
    NodeAssert.ok(listed);
    NodeAssert.equal(listed.label, "Synthetic phone");
    NodeAssert.deepEqual(listed.scopes, STANDARD_SCOPES);

    const unauthorizedCreate = await http(`${ctx.httpUrl}/api/auth/pairing-token`, {
      method: "POST",
      headers: { authorization: `Bearer ${reader.body.access_token}` },
      json: {},
    });
    NodeAssert.equal(unauthorizedCreate.status, 403);
    const idExchange = await exchangeAccessToken(ctx.httpUrl, created.body.id, {
      scope: "terminal:operate",
    });
    NodeAssert.equal(idExchange.status, 401);
    const authorized = await exchangeAccessToken(ctx.httpUrl, created.body.credential, {
      scope: STANDARD_SCOPES.join(" "),
    });
    NodeAssert.equal(authorized.status, 200);
    NodeAssert.equal(authorized.body.scope, STANDARD_SCOPES.join(" "));
    const reused = await exchangeAccessToken(ctx.httpUrl, created.body.credential, {
      scope: "terminal:operate",
    });
    NodeAssert.equal(reused.status, 401);
  });

  // server.test.ts:4985
  NodeTest.test("lists and revokes pairing links for access management sessions", async () => {
    const owner = await ctx.adminCookie();
    const created = await http<{ id: string; credential: string }>(
      `${ctx.httpUrl}/api/auth/pairing-token`,
      {
        method: "POST",
        headers: { cookie: owner },
        json: {},
      },
    );
    const list = await http<Array<{ id: string }>>(`${ctx.httpUrl}/api/auth/pairing-links`, {
      headers: { cookie: owner },
    });
    const revoke = await http<{ revoked: boolean }>(
      `${ctx.httpUrl}/api/auth/pairing-links/revoke`,
      {
        method: "POST",
        headers: { cookie: owner },
        json: { id: created.body.id },
      },
    );
    const revokedBootstrap = await bootstrapBrowserSession(ctx.httpUrl, created.body.credential);
    NodeAssert.equal(created.status, 200);
    NodeAssert.equal(list.status, 200);
    NodeAssert.ok(list.body.some((entry) => entry.id === created.body.id));
    NodeAssert.equal(revoke.status, 200);
    NodeAssert.equal(revoke.body.revoked, true);
    NodeAssert.equal(revokedBootstrap.status, 401);
  });

  // server.test.ts:5031
  NodeTest.test("rejects pairing credential requests without access management scope", async () => {
    const paired = await ctx.newSessionCookie({ scopes: STANDARD_SCOPES });
    const res = await http<AuthError>(`${ctx.httpUrl}/api/auth/pairing-token`, {
      method: "POST",
      headers: { cookie: paired },
      json: {},
    });
    NodeAssert.equal(res.status, 403);
    NodeAssert.equal(res.body._tag, "EnvironmentScopeRequiredError");
    NodeAssert.equal(res.body.code, "insufficient_scope");
    NodeAssert.equal(res.body.requiredScope, "access:write");
    NodeAssert.equal(typeof res.body.traceId, "string");
  });

  // server.test.ts:5183
  NodeTest.test("separates access inventory reads from credential management writes", async () => {
    const readCookie = await ctx.newSessionCookie({ scopes: ["access:read"] });
    const readList = await http(`${ctx.httpUrl}/api/auth/clients`, {
      headers: { cookie: readCookie },
    });
    const readWrite = await http<AuthError>(`${ctx.httpUrl}/api/auth/pairing-token`, {
      method: "POST",
      headers: { cookie: readCookie },
      json: {},
    });
    const writeCookie = await ctx.newSessionCookie({ scopes: ["access:write"] });
    const writeList = await http<AuthError>(`${ctx.httpUrl}/api/auth/clients`, {
      headers: { cookie: writeCookie },
    });
    NodeAssert.equal(readList.status, 200);
    NodeAssert.equal(readWrite.status, 403);
    NodeAssert.equal(readWrite.body.requiredScope, "access:write");
    NodeAssert.equal(writeList.status, 403);
    NodeAssert.equal(writeList.body.requiredScope, "access:read");
  });

  // server.test.ts:5242
  NodeTest.test("revokes an individual paired client session", async () => {
    const owner = await ctx.adminCookie();
    const paired = await ctx.newSessionCookie({ label: "to be revoked" });
    const clients = await http<
      Array<{ sessionId: string; current: boolean; client: { label?: string } }>
    >(`${ctx.httpUrl}/api/auth/clients`, { headers: { cookie: owner } });
    const target = clients.body.find(
      (entry) => !entry.current && entry.client.label === "to be revoked",
    );
    NodeAssert.ok(target, JSON.stringify(clients.body));
    const revoke = await http<{ revoked: boolean }>(`${ctx.httpUrl}/api/auth/clients/revoke`, {
      method: "POST",
      headers: { cookie: owner },
      json: { sessionId: target.sessionId },
    });
    const after = await http(`${ctx.httpUrl}/api/auth/pairing-token`, {
      method: "POST",
      headers: { cookie: paired },
      json: {},
    });
    NodeAssert.equal(revoke.status, 200);
    NodeAssert.equal(revoke.body.revoked, true);
    NodeAssert.equal(after.status, 401);
  });

  NodeTest.test("refuses to revoke the caller's own session", async () => {
    const owner = await ctx.adminCookie();
    const clients = await http<Array<{ sessionId: string; current: boolean }>>(
      `${ctx.httpUrl}/api/auth/clients`,
      { headers: { cookie: owner } },
    );
    const self = clients.body.find((entry) => entry.current);
    NodeAssert.ok(self);
    const res = await http<{ _tag: string }>(`${ctx.httpUrl}/api/auth/clients/revoke`, {
      method: "POST",
      headers: { cookie: owner },
      json: { sessionId: self.sessionId },
    });
    NodeAssert.equal(res.status, 403);
  });

  // server.test.ts:5072 — runs last in this block: it revokes every other session.
  NodeTest.test(
    "lists paired clients and revokes other sessions while keeping the administrator",
    async () => {
      const owner = await ctx.adminCookie();
      const pairing = await http<{ credential: string; label?: string }>(
        `${ctx.httpUrl}/api/auth/pairing-token`,
        {
          method: "POST",
          headers: { cookie: owner },
          json: { label: "Julius iPhone" },
        },
      );
      NodeAssert.equal(pairing.status, 200);
      NodeAssert.equal(pairing.body.label, "Julius iPhone");
      const pairedBoot = await bootstrapBrowserSession(ctx.httpUrl, pairing.body.credential, {
        "user-agent":
          "Mozilla/5.0 (iPhone; CPU iPhone OS 17_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Mobile/15E148 Safari/604.1",
      });
      const pairedCookie = cookiePair(pairedBoot.setCookie!);

      const before = await http<
        Array<{
          sessionId: string;
          current: boolean;
          client: {
            label?: string;
            deviceType: string;
            ipAddress?: string;
            os?: string;
            browser?: string;
          };
        }>
      >(`${ctx.httpUrl}/api/auth/clients`, { headers: { cookie: owner } });
      NodeAssert.equal(before.status, 200);
      const iphone = before.body.find((entry) => entry.client.label === "Julius iPhone");
      NodeAssert.ok(iphone, JSON.stringify(before.body));
      NodeAssert.equal(iphone.current, false);
      NodeAssert.equal(iphone.client.deviceType, "mobile");
      NodeAssert.equal(iphone.client.os, "iOS");
      NodeAssert.equal(iphone.client.browser, "Safari");
      NodeAssert.equal(iphone.client.ipAddress, "127.0.0.1");

      const revokeOthers = await http<{ revokedCount: number }>(
        `${ctx.httpUrl}/api/auth/clients/revoke-others`,
        {
          method: "POST",
          headers: { cookie: owner },
        },
      );
      NodeAssert.equal(revokeOthers.status, 200);
      NodeAssert.equal(revokeOthers.body.revokedCount, before.body.length - 1);

      const after = await http<Array<{ current: boolean }>>(`${ctx.httpUrl}/api/auth/clients`, {
        headers: { cookie: owner },
      });
      NodeAssert.equal(after.status, 200);
      NodeAssert.equal(after.body.length, 1);
      NodeAssert.equal(after.body[0]?.current, true);

      const revoked = await http<AuthError>(`${ctx.httpUrl}/api/auth/pairing-token`, {
        method: "POST",
        headers: { cookie: pairedCookie },
        json: {},
      });
      NodeAssert.equal(revoked.status, 401);
      NodeAssert.equal(revoked.body._tag, "EnvironmentAuthInvalidError");
      NodeAssert.equal(revoked.body.code, "auth_invalid");
      NodeAssert.equal(revoked.body.reason, "invalid_credential");
      NodeAssert.equal(typeof revoked.body.traceId, "string");
    },
  );
});
