/**
 * The contracts oracle: every wire schema the web client decodes with, looked up by RPC tag or by
 * HTTP method + path, and decoded exactly the way the client does it (`Schema.toCodecJson`).
 *
 * Loaded from the worktree's own `packages/contracts/src` (not from a built package) so that the
 * oracle is always the contracts at HEAD.
 */
import { EnvironmentHttpApi, WsRpcGroup } from "../../../packages/contracts/src/index.ts";
import { Result, Schema, SchemaIssue } from "effect";
import * as HttpApi from "effect/unstable/httpapi/HttpApi";
import * as Rpc from "effect/unstable/rpc/Rpc";
import * as RpcSchema from "effect/unstable/rpc/RpcSchema";

export type DecodeOutcome =
  | { readonly ok: true; readonly value: unknown; readonly canonical?: unknown }
  | { readonly ok: false; readonly message: string };

const formatIssue = SchemaIssue.makeFormatterDefault();

type Codec = {
  decode: (input: unknown) => DecodeOutcome;
  /** Re-encodes a decoded value; used to detect non-canonical (but accepted) encodings. */
  encode: (value: unknown) => unknown;
};

const codecCache = new WeakMap<object, Codec>();

/** A JSON codec for a schema, exactly as Effect RPC and HttpApi build it. */
export const jsonCodec = (schema: Schema.Top): Codec => {
  const cached = codecCache.get(schema);
  if (cached) return cached;
  const codec = Schema.toCodecJson(schema) as unknown as Schema.Codec<unknown, unknown>;
  const decoder = Schema.decodeUnknownResult(codec);
  const encoder = Schema.encodeUnknownResult(codec);
  const result: Codec = {
    decode: (input) => {
      try {
        const out = decoder(input);
        if (Result.isSuccess(out)) return { ok: true, value: out.success };
        return { ok: false, message: formatIssue(out.failure.issue) };
      } catch (error) {
        return { ok: false, message: `decoder threw: ${String(error)}` };
      }
    },
    encode: (value) => {
      try {
        const out = encoder(value);
        return Result.isSuccess(out) ? out.success : undefined;
      } catch {
        return undefined;
      }
    },
  };
  codecCache.set(schema, result);
  return result;
};

// ---------------------------------------------------------------------------------------------
// WebSocket RPC
// ---------------------------------------------------------------------------------------------

export interface RpcSpec {
  readonly tag: string;
  readonly isStream: boolean;
  readonly payload: Codec;
  /** Decodes `Chunk.values` (a non-empty array of stream items). Undefined for unary RPCs. */
  readonly chunk: Codec | undefined;
  readonly exit: Codec;
}

const rpcSpecs = new Map<string, RpcSpec>();
for (const [tag, rpc] of WsRpcGroup.requests as ReadonlyMap<string, Rpc.Any>) {
  const anyRpc = rpc as unknown as {
    payloadSchema: Schema.Top;
    successSchema: Schema.Top;
  };
  const streamSchemas = RpcSchema.isStreamSchema(anyRpc.successSchema)
    ? (anyRpc.successSchema as unknown as { success: Schema.Top })
    : undefined;
  rpcSpecs.set(tag, {
    tag,
    isStream: streamSchemas !== undefined,
    payload: jsonCodec(anyRpc.payloadSchema),
    chunk: streamSchemas ? jsonCodec(Schema.NonEmptyArray(streamSchemas.success)) : undefined,
    exit: jsonCodec(Rpc.exitSchema(rpc as never) as unknown as Schema.Top),
  });
}

export const getRpcSpec = (tag: string): RpcSpec | undefined => rpcSpecs.get(tag);
export const rpcTags = (): ReadonlyArray<string> => [...rpcSpecs.keys()];

export const defectCodec = jsonCodec(Schema.Defect());

// ---------------------------------------------------------------------------------------------
// HTTP API
// ---------------------------------------------------------------------------------------------

export interface HttpEndpointSpec {
  readonly name: string;
  readonly method: string;
  readonly path: string;
  readonly match: (pathname: string) => Record<string, string> | undefined;
  /** content-type → codec of the request payload (JSON body, form body or query string). */
  readonly payload: ReadonlyMap<string, Codec>;
  readonly params: Codec | undefined;
  readonly successes: ReadonlyMap<number, Codec>;
  readonly errors: ReadonlyMap<number, Codec>;
}

const unionCodec = (schemas: ReadonlyArray<Schema.Top>): Codec =>
  jsonCodec(schemas.length === 1 ? schemas[0]! : Schema.Union(schemas as Array<Schema.Top>));

const unwrapBody = (schema: Schema.Top): Schema.Top => {
  // HttpApiSchema.withHeaders wraps the body schema; the body is what goes on the wire.
  const maybe = schema as unknown as { schema?: Schema.Top; ast: unknown };
  return maybe.schema && typeof maybe.schema === "object" && "ast" in maybe.schema
    ? maybe.schema
    : schema;
};

const compilePath = (path: string) => {
  const names: Array<string> = [];
  const pattern = path
    .split("/")
    .map((segment) => {
      if (segment.startsWith(":")) {
        names.push(segment.slice(1));
        return "([^/]+)";
      }
      return segment.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    })
    .join("/");
  const regex = new RegExp(`^${pattern}$`);
  return (pathname: string) => {
    const m = regex.exec(pathname);
    if (!m) return undefined;
    const params: Record<string, string> = {};
    names.forEach((name, i) => {
      params[name] = decodeURIComponent(m[i + 1] ?? "");
    });
    return params;
  };
};

const httpEndpoints: Array<HttpEndpointSpec> = [];
HttpApi.reflect(EnvironmentHttpApi, {
  onGroup() {},
  onEndpoint({ endpoint, successes, errors }) {
    const ep = endpoint as unknown as {
      identifier: string;
      method: string;
      path: string;
      params?: Schema.Top;
      payload: Map<string, { schemas: Iterable<Schema.Top> }>;
    };
    const payload = new Map<string, Codec>();
    for (const [contentType, entry] of ep.payload) {
      payload.set(contentType, unionCodec([...entry.schemas]));
    }
    const toCodecs = (map: Map<number, Array<Schema.Top>>) =>
      new Map([...map].map(([status, list]) => [status, unionCodec(list.map(unwrapBody))]));
    httpEndpoints.push({
      name: ep.identifier,
      method: ep.method,
      path: ep.path,
      match: compilePath(ep.path),
      payload,
      params: ep.params ? jsonCodec(ep.params) : undefined,
      successes: toCodecs(successes as unknown as Map<number, Array<Schema.Top>>),
      errors: toCodecs(errors as unknown as Map<number, Array<Schema.Top>>),
    });
  },
});

export const findHttpEndpoint = (
  method: string,
  pathname: string,
): { spec: HttpEndpointSpec; params: Record<string, string> } | undefined => {
  for (const spec of httpEndpoints) {
    if (spec.method !== method.toUpperCase()) continue;
    const params = spec.match(pathname);
    if (params) return { spec, params };
  }
  return undefined;
};

export const httpEndpointList = (): ReadonlyArray<HttpEndpointSpec> => httpEndpoints;

/** Raw (non-HttpApi) routes whose JSON bodies we still check. */
export const ZenithEmbedJson = jsonCodec(
  Schema.Struct({ parentOrigins: Schema.Array(Schema.String) }),
);
