/**
 * The TypeScript contracts as an oracle for the Rust port.
 *
 *   node scripts/contracts-oracle.ts < values.jsonl > results.jsonl
 *
 * Reads JSON lines `{"schema": <id>, "value": <wire value>}` on stdin and decodes each value with
 * `Schema.decodeUnknownSync(Schema.toCodecJson(schema))`, the codec the server and the web client
 * use on the wire. Writes one line per input, in order: `{"ok":true}` or `{"ok":false,"issue":…}`.
 *
 * Schema ids (the same as the Rust registry, `zc_contracts::registry`):
 *   - an export name of packages/contracts (`ServerConfig`, `OrchestrationEvent`, …);
 *   - `rpc:<tag>:payload|success|error` (success = one stream item for stream RPCs);
 *   - `http:<group>.<endpoint>:params|payload|success:<n>|error`.
 *
 * Options: `--canonical` also prints the TS re-encoding of each value (`"canonical"`), handy to
 * diff with what Rust produced.
 */
import * as Readline from "node:readline";

import * as Contracts from "../packages/contracts/src/index.ts";
import * as Schema from "effect/Schema";
import * as SchemaIssue from "effect/SchemaIssue";
import * as HttpApi from "effect/unstable/httpapi/HttpApi";
import * as RpcSchema from "effect/unstable/rpc/RpcSchema";

const canonical = process.argv.includes("--canonical");

const schemas = new Map<string, Schema.Top>();
for (const [name, value] of Object.entries(Contracts as Record<string, unknown>)) {
  if (Schema.isSchema(value)) schemas.set(name, value as Schema.Top);
}
for (const [tag, rpc] of (Contracts.WsRpcGroup as any).requests as Map<string, any>) {
  const stream = RpcSchema.getStreamSchemas(rpc.successSchema);
  const errors: Schema.Top[] = [rpc.errorSchema];
  if (stream._tag === "Some") errors.push((stream as any).value.error);
  const kept = errors.filter((s) => s.ast._tag !== "Never");
  schemas.set(`rpc:${tag}:payload`, rpc.payloadSchema);
  schemas.set(
    `rpc:${tag}:success`,
    stream._tag === "Some" ? (stream as any).value.success : rpc.successSchema,
  );
  schemas.set(
    `rpc:${tag}:error`,
    kept.length === 0 ? Schema.Never : kept.length === 1 ? kept[0]! : Schema.Union(kept),
  );
}
const unwrap = (s: any): Schema.Top =>
  s && s.schema && Schema.isSchema(s.schema) && !s.fields ? s.schema : s;
HttpApi.reflect(Contracts.EnvironmentHttpApi as any, {
  onGroup() {},
  onEndpoint(o: any) {
    const e = o.endpoint;
    const base = `http:${o.group.identifier}.${e.identifier}`;
    if (e.params) schemas.set(`${base}:params`, unwrap(e.params));
    const payloads = [...e.payload.values()] as Array<{ schemas: any[] }>;
    if (payloads.length > 0) {
      const list = payloads[0]!.schemas.map(unwrap);
      schemas.set(`${base}:payload`, list.length === 1 ? list[0]! : Schema.Union(list));
    }
    let i = 0;
    for (const [, list] of o.successes as Map<number, any[]>)
      for (const s of list) schemas.set(`${base}:success:${i++}`, unwrap(s));
    const errors = [...(o.errors as Map<number, any[]>).values()].flat().map(unwrap);
    schemas.set(
      `${base}:error`,
      errors.length === 0 ? Schema.Never : errors.length === 1 ? errors[0]! : Schema.Union(errors),
    );
  },
});

const codecs = new Map<string, any>();
const codecFor = (id: string) => {
  let c = codecs.get(id);
  if (!c) {
    const s = schemas.get(id);
    if (!s) return undefined;
    c = Schema.toCodecJson(s);
    codecs.set(id, c);
  }
  return c;
};

const rl = Readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
for await (const line of rl) {
  if (!line.trim()) continue;
  let out: Record<string, unknown>;
  try {
    const { schema, value } = JSON.parse(line) as { schema: string; value: unknown };
    const codec = codecFor(schema);
    if (!codec) {
      out = { ok: false, issue: `unknown schema id ${JSON.stringify(schema)}` };
    } else {
      const decoded = Schema.decodeUnknownExit(codec, { errors: "all" })(value);
      if (decoded._tag === "Success") {
        out = { ok: true };
        if (canonical) out.canonical = Schema.encodeSync(codec)(decoded.value);
      } else {
        const failure =
          (decoded.cause as any).reasons?.find((r: any) => r._tag === "Fail")?.error ??
          decoded.cause;
        const issue = failure?.issue
          ? SchemaIssue.makeFormatterDefault()(failure.issue)
          : String(failure);
        out = { ok: false, issue };
      }
    }
  } catch (error) {
    out = { ok: false, issue: `oracle error: ${String(error)}` };
  }
  process.stdout.write(JSON.stringify(out) + "\n");
}
