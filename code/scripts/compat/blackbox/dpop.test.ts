/**
 * DPoP (RFC 9449) on `/oauth/token` and on authenticated endpoints, black-box. Ported from
 * apps/server/src/server.test.ts:2499-2766; the TestClock case (a pruned replay marker) becomes
 * a stale proof, since a black box cannot move the server's clock.
 */
import * as NodeTest from "node:test";
import * as NodeAssert from "node:assert/strict";
import * as NodeCrypto from "node:crypto";

import { http } from "../lib/client.ts";
import { useServer } from "./harness.ts";

type Jwk = { kty: string; crv: string; x: string; y: string };

const SCOPE = "orchestration:read orchestration:operate terminal:operate review:write";

const makeKey = () => {
  const { privateKey, publicKey } = NodeCrypto.generateKeyPairSync("ec", { namedCurve: "P-256" });
  const jwk = publicKey.export({ format: "jwk" }) as Jwk;
  return { privateKey, jwk: { kty: jwk.kty, crv: jwk.crv, x: jwk.x, y: jwk.y } };
};

const sha256 = (text: string) => NodeCrypto.createHash("sha256").update(text).digest("base64url");

const makeProof = (input: {
  method: string;
  url: string;
  iat: number;
  jti?: string;
  accessToken?: string;
  key?: ReturnType<typeof makeKey>;
}) => {
  const key = input.key ?? makeKey();
  const header = Buffer.from(
    JSON.stringify({ typ: "dpop+jwt", alg: "ES256", jwk: key.jwk }),
  ).toString("base64url");
  const payload = Buffer.from(
    JSON.stringify({
      htm: input.method,
      htu: input.url,
      jti: input.jti ?? NodeCrypto.randomUUID(),
      iat: input.iat,
      ...(input.accessToken ? { ath: sha256(input.accessToken) } : {}),
    }),
  ).toString("base64url");
  const signature = NodeCrypto.sign("sha256", Buffer.from(`${header}.${payload}`), {
    key: key.privateKey,
    dsaEncoding: "ieee-p1363",
  }).toString("base64url");
  return { proof: `${header}.${payload}.${signature}`, key };
};

const nowSeconds = () => Math.floor(Date.now() / 1000);

type TokenBody = {
  access_token?: string;
  token_type?: string;
  _tag?: string;
  code?: string;
  reason?: string;
  dpopFailureReason?: string;
  traceId?: string;
};

const exchange = (httpUrl: string, credential: string, headers: Record<string, string>) =>
  http<TokenBody>(`${httpUrl}/oauth/token`, {
    method: "POST",
    headers,
    form: {
      grant_type: "urn:ietf:params:oauth:grant-type:token-exchange",
      subject_token: credential,
      subject_token_type: "urn:t3:params:oauth:token-type:environment-bootstrap",
      requested_token_type: "urn:ietf:params:oauth:token-type:access_token",
      scope: SCOPE,
    },
  });

const assertDpopFailure = (
  res: { status: number; body: TokenBody; headers: Record<string, string> },
  reason: string,
) => {
  NodeAssert.equal(res.status, 401);
  NodeAssert.equal(res.body._tag, "EnvironmentAuthInvalidError");
  NodeAssert.equal(res.body.code, "auth_invalid");
  NodeAssert.equal(res.body.reason, "invalid_credential");
  NodeAssert.equal(res.body.dpopFailureReason, reason);
  NodeAssert.equal(typeof res.body.traceId, "string");
  NodeAssert.equal(res.headers["www-authenticate"], "DPoP");
};

NodeTest.describe("dpop: token exchange and proof-bound sessions", () => {
  const ctx = useServer();

  // server.test.ts:2499
  NodeTest.test(
    "exchanges a credential for a DPoP-bound access token without bearer downgrade",
    async () => {
      const tokenUrl = `${ctx.httpUrl}/oauth/token`;
      const tokenProof = makeProof({ method: "POST", url: tokenUrl, iat: nowSeconds() });
      const token = await exchange(ctx.httpUrl, await ctx.newCredential(), {
        dpop: tokenProof.proof,
      });
      NodeAssert.equal(token.status, 200, token.text);
      NodeAssert.equal(token.headers["cache-control"], "no-store");
      NodeAssert.equal(token.body.token_type, "DPoP");
      const accessToken = token.body.access_token!;

      const sessionUrl = `${ctx.httpUrl}/api/auth/session`;
      const bearer = await http<{ authenticated: boolean }>(sessionUrl, {
        headers: { authorization: `Bearer ${accessToken}` },
      });
      NodeAssert.equal(bearer.body.authenticated, false);

      const sessionProof = makeProof({
        method: "GET",
        url: sessionUrl,
        iat: nowSeconds(),
        accessToken,
        key: tokenProof.key,
      });
      const bound = await http<{ authenticated: boolean; sessionMethod?: string }>(sessionUrl, {
        headers: { authorization: `DPoP ${accessToken}`, dpop: sessionProof.proof },
      });
      NodeAssert.equal(bound.body.authenticated, true);
      NodeAssert.equal(bound.body.sessionMethod, "dpop-access-token");

      // On an authenticated endpoint: no proof, or another key's proof, is a 401 with a
      // DPoP challenge; the right proof gets a ticket.
      const ticketUrl = `${ctx.httpUrl}/api/auth/websocket-ticket`;
      const noProof = await http<TokenBody>(ticketUrl, {
        method: "POST",
        headers: { authorization: `DPoP ${accessToken}` },
      });
      assertDpopFailure(noProof, "invalid_proof");
      const otherKey = makeProof({
        method: "POST",
        url: ticketUrl,
        iat: nowSeconds(),
        accessToken,
      });
      const wrongKey = await http<TokenBody>(ticketUrl, {
        method: "POST",
        headers: { authorization: `DPoP ${accessToken}`, dpop: otherKey.proof },
      });
      assertDpopFailure(wrongKey, "key_mismatch");
      const ticketProof = makeProof({
        method: "POST",
        url: ticketUrl,
        iat: nowSeconds(),
        accessToken,
        key: tokenProof.key,
      });
      const ticket = await http<{ ticket: string }>(ticketUrl, {
        method: "POST",
        headers: { authorization: `DPoP ${accessToken}`, dpop: ticketProof.proof },
      });
      NodeAssert.equal(ticket.status, 200, ticket.text);
      NodeAssert.equal(typeof ticket.body.ticket, "string");
    },
  );

  NodeTest.test("refuses DPoP authorization with a token that is not proof-bound", async () => {
    const bearer = await ctx.newBearerToken();
    const ticketUrl = `${ctx.httpUrl}/api/auth/websocket-ticket`;
    const res = await http<TokenBody>(ticketUrl, {
      method: "POST",
      headers: {
        authorization: `DPoP ${bearer}`,
        dpop: makeProof({ method: "POST", url: ticketUrl, iat: nowSeconds() }).proof,
      },
    });
    assertDpopFailure(res, "invalid_proof");
  });

  // server.test.ts:2575
  NodeTest.test("reports clock skew for a future-dated proof", async () => {
    const proof = makeProof({
      method: "POST",
      url: `${ctx.httpUrl}/oauth/token`,
      iat: nowSeconds() + 25,
    });
    assertDpopFailure(
      await exchange(ctx.httpUrl, await ctx.newCredential(), { dpop: proof.proof }),
      "time_window",
    );
  });

  // Adapted from server.test.ts:2661 (the server clock cannot be moved from outside).
  NodeTest.test("rejects a stale proof by time alone", async () => {
    const proof = makeProof({
      method: "POST",
      url: `${ctx.httpUrl}/oauth/token`,
      iat: nowSeconds() - 302,
    });
    assertDpopFailure(
      await exchange(ctx.httpUrl, await ctx.newCredential(), { dpop: proof.proof }),
      "time_window",
    );
  });

  // server.test.ts:2607
  NodeTest.test("rejects replayed proofs across token exchanges", async () => {
    const proof = makeProof({
      method: "POST",
      url: `${ctx.httpUrl}/oauth/token`,
      iat: nowSeconds(),
    });
    const first = await exchange(ctx.httpUrl, await ctx.newCredential(), { dpop: proof.proof });
    const replay = await exchange(ctx.httpUrl, await ctx.newCredential(), { dpop: proof.proof });
    NodeAssert.equal(first.status, 200, first.text);
    assertDpopFailure(replay, "replay");
  });

  // server.test.ts:2695
  NodeTest.test("ignores forwarded host headers when validating the proof URL", async () => {
    const proof = makeProof({
      method: "POST",
      url: `${ctx.httpUrl}/oauth/token`,
      iat: nowSeconds(),
    });
    const res = await exchange(ctx.httpUrl, await ctx.newCredential(), {
      dpop: proof.proof,
      "x-forwarded-host": "environment.example.test",
    });
    NodeAssert.equal(res.status, 200, res.text);
    NodeAssert.equal(res.body.token_type, "DPoP");
  });

  // server.test.ts:2730
  NodeTest.test("rejects proofs bound to a spoofed forwarded host", async () => {
    const spoofed = new URL(`${ctx.httpUrl}/oauth/token`);
    spoofed.hostname = "environment.example.test";
    const proof = makeProof({ method: "POST", url: spoofed.href, iat: nowSeconds() });
    assertDpopFailure(
      await exchange(ctx.httpUrl, await ctx.newCredential(), {
        dpop: proof.proof,
        "x-forwarded-host": spoofed.host,
      }),
      "request_mismatch",
    );
  });

  NodeTest.test("rejects a malformed proof as invalid_proof", async () => {
    assertDpopFailure(
      await exchange(ctx.httpUrl, await ctx.newCredential(), { dpop: "not.a-proof" }),
      "invalid_proof",
    );
  });
});
