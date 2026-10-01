/**
 * Generates the Rust crate `zc-codex-protocol` (crates/zenith-code/crates/zc-codex-protocol): serde
 * types for the Codex app-server protocol, from the same pinned JSON Schemas that
 * `packages/effect-codex-app-server/scripts/generate.ts` turns into Effect schemas.
 *
 *   node scripts/gen-rust-codex-protocol.ts                    # fetch the pinned schemas from GitHub
 *   node scripts/gen-rust-codex-protocol.ts --schema-dir <dir> # or read a local copy laid out as
 *                                                              # <dir>/json/*.json + <dir>/typescript/*.ts
 *
 * Input: `codex-rs/app-server-protocol/schema/json/codex_app_server_protocol.schemas.json` at
 * UPSTREAM_REF (the same commit as generate.ts). That bundle holds every definition of the
 * per-file schemas generate.ts downloads (root, `v1/`, `v2/` — the v2 ones nested under
 * `definitions.v2`), once each instead of copied into every file. The method lists come from
 * the ts-rs `schema/typescript/{Client,Server}{Request,Notification}.ts` files, parsed with
 * generate.ts's regexes, and are cross-checked against the bundle's own method unions.
 *
 * Reproduced from generate.ts: `PlanType` forced to `string` (unknown plan slugs must not fail
 * `account/read`), the hand-added schemas (GetAuthStatus*, GetConversationSummary*,
 * GitDiffToRemote*, added only when missing), `type: [X, "null"]` read as nullable X,
 * `default: null` ignored, response types resolved by name with the same override table.
 *
 * Rust mapping (see docs/zenith-code/codex.md):
 * - object with properties → struct; extra keys are ignored (as Effect's Struct decode does) and
 *   not written back. Required → `T`; required + nullable → `Option<T>` that must be present;
 *   optional → `Option<T>`; optional + nullable → `Option<Option<T>>` (absent / null / value).
 * - string enums → enum with an `Unknown(String)` catch-all (unknown variants tolerated).
 * - unions of objects sharing a single-literal property (`type`, `method`, `mode`, …) → enum
 *   dispatching on that property, with an `Unknown(Value)` catch-all; other unions → untagged
 *   enums ending in `Other(Value)`.
 * - `integer` → i64, `number` → `Number` (f64 written as an integer when integral, like JS).
 *
 * Only what zenith code uses is generated: every server notification and server request (the
 * runtime decodes all of them, and a payload that does not decode is dropped, as in TS), the
 * client requests in USED_CLIENT_REQUESTS, the `initialized` notification, and their closure.
 */
import * as Fs from "node:fs";
import * as Path from "node:path";
import * as Url from "node:url";
import { execFileSync } from "node:child_process";

const UPSTREAM_REF = "fe74a774532af67b5a4a3dec03ce9469e17f89af";
const RAW_BASE = `https://raw.githubusercontent.com/openai/codex/${UPSTREAM_REF}/codex-rs/app-server-protocol/schema`;

const USED_CLIENT_REQUESTS = [
  "initialize",
  "thread/start",
  "thread/resume",
  "thread/read",
  "thread/turns/list",
  "thread/revert",
  "thread/inject_items",
  "thread/compact/start",
  "turn/start",
  "turn/interrupt",
  "config/mcpServer/reload",
  "feedback/upload",
  "account/read",
  "account/rateLimits/read",
  "account/rateLimitResetCredit/consume",
  "model/list",
  "skills/list",
];

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
type Obj = { [key: string]: Json };

const here = Path.dirname(Url.fileURLToPath(import.meta.url));
const repoRoot = Path.resolve(here, "..", "..");
const crateDir = Path.join(repoRoot, "crates", "zenith-code", "crates", "zc-codex-protocol");

// ---------------------------------------------------------------------------------------------
// Inputs

function argValue(name: string): string | undefined {
  const index = process.argv.indexOf(name);
  return index === -1 ? undefined : process.argv[index + 1];
}

async function readInput(relative: string): Promise<string> {
  const schemaDir = argValue("--schema-dir");
  if (schemaDir) return Fs.readFileSync(Path.join(schemaDir, relative), "utf8");
  const response = await fetch(`${RAW_BASE}/${relative}`, {
    headers: { "user-agent": "zenith-codex-protocol-generator" },
  });
  if (!response.ok) throw new Error(`Failed to fetch ${relative}: ${response.status}`);
  return response.text();
}

// generate.ts's tweaks -------------------------------------------------------------------------

const ManualSchemas: Record<string, Obj> = {
  GetAuthStatusParams: {
    type: "object",
    properties: {
      includeToken: { anyOf: [{ type: "boolean" }, { type: "null" }] },
      refreshToken: { anyOf: [{ type: "boolean" }, { type: "null" }] },
    },
  },
  GetConversationSummaryParams: {
    oneOf: [
      { type: "object", properties: { rolloutPath: { type: "string" } }, required: ["rolloutPath"] },
      {
        type: "object",
        properties: { conversationId: { type: "string" } },
        required: ["conversationId"],
      },
    ],
  },
  GetConversationSummaryResponse: {
    type: "object",
    properties: { summary: {} },
    required: ["summary"],
  },
  GitDiffToRemoteParams: {
    type: "object",
    properties: { cwd: { type: "string" } },
    required: ["cwd"],
  },
  GitDiffToRemoteResponse: {
    type: "object",
    properties: { sha: { type: "string" }, diff: { type: "string" } },
    required: ["sha", "diff"],
  },
  GetAuthStatusResponse: {
    type: "object",
    properties: {
      authMethod: { anyOf: [{}, { type: "null" }] },
      authToken: { anyOf: [{ type: "string" }, { type: "null" }] },
      requiresOpenaiAuth: { anyOf: [{ type: "boolean" }, { type: "null" }] },
    },
    required: ["authMethod", "authToken", "requiresOpenaiAuth"],
  },
};

// Codex adds plan slugs between protocol refreshes; the plan is only a label.
const DefinitionOverrides: Record<string, Obj> = { PlanType: { type: "string" } };

const ResponseOverrides: Record<string, string> = {
  "account/logout": "LogoutAccountResponse",
  "account/rateLimits/read": "GetAccountRateLimitsResponse",
  "account/usage/read": "GetAccountTokenUsageResponse",
  "account/workspaceMessages/read": "GetWorkspaceMessagesResponse",
  "config/batchWrite": "ConfigWriteResponse",
  "config/mcpServer/reload": "McpServerRefreshResponse",
  "config/value/write": "ConfigWriteResponse",
  "configRequirements/read": "ConfigRequirementsReadResponse",
  "externalAgentConfig/import/readHistories": "ExternalAgentConfigImportHistoriesReadResponse",
};

function isObj(value: Json | undefined): value is Obj {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function normalizeNullableTypes(value: Json): Json {
  if (Array.isArray(value)) return value.map(normalizeNullableTypes);
  if (!isObj(value)) return value;
  const node: Obj = Object.fromEntries(
    Object.entries(value).map(([key, child]) => [key, normalizeNullableTypes(child)]),
  );
  const types = node.type;
  if (!Array.isArray(types)) return node;
  const strings = types.filter((entry): entry is string => typeof entry === "string");
  if (strings.length !== types.length || !strings.includes("null")) return node;
  const nonNull = strings.filter((entry) => entry !== "null");
  if (nonNull.length !== 1) return node;
  const { type: _type, ...rest } = node;
  return { anyOf: [{ ...rest, type: nonNull[0]! }, { type: "null" }] };
}

function stripNullDefaults(value: Json): Json {
  if (Array.isArray(value)) return value.map(stripNullDefaults);
  if (!isObj(value)) return value;
  return Object.fromEntries(
    Object.entries(value)
      .filter(([key, child]) => !(key === "default" && child === null))
      .map(([key, child]) => [key, stripNullDefaults(child)]),
  );
}

// Method lists: generate.ts's regexes over the ts-rs files --------------------------------------

interface MethodEntry {
  readonly method: string;
  readonly paramsType?: string;
}

function parseRequestEntries(contents: string): MethodEntry[] {
  const pattern = /\{\s*"method":\s*"([^"]+)",\s*id:\s*RequestId,\s*params(\??):\s*([^,}|]+)/g;
  const entries: MethodEntry[] = [];
  for (let match = pattern.exec(contents); match; match = pattern.exec(contents)) {
    entries.push({ method: match[1]!, paramsType: `${match[2] ? "Nullable" : ""}${match[3]!.trim()}` });
  }
  return entries;
}

function parseNotificationEntries(contents: string): MethodEntry[] {
  const pattern = /\{\s*"method":\s*"([^"]+)"(?:,\s*"params":\s*([^ }]+))?\s*\}/g;
  const entries: MethodEntry[] = [];
  for (let match = pattern.exec(contents); match; match = pattern.exec(contents)) {
    entries.push({ method: match[1]!, ...(match[2] ? { paramsType: match[2].trim() } : {}) });
  }
  return entries;
}

// ---------------------------------------------------------------------------------------------
// Naming

const RUST_KEYWORDS = new Set([
  "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
  "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
  "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
  "use", "where", "while", "abstract", "become", "box", "do", "final", "macro", "override", "priv",
  "typeof", "unsized", "virtual", "yield", "try", "gen",
]);

function words(value: string): string[] {
  return value
    .replace(/([a-z0-9])([A-Z])/g, "$1 $2")
    .replace(/([A-Z]+)([A-Z][a-z])/g, "$1 $2")
    .split(/[^A-Za-z0-9]+/)
    .filter(Boolean);
}

function pascal(value: string): string {
  const result = words(value)
    .map((word) => word[0]!.toUpperCase() + word.slice(1))
    .join("");
  if (!result) return "Empty";
  return /^[0-9]/.test(result) ? `V${result}` : result;
}

function snake(value: string): string {
  const result = words(value)
    .map((word) => word.toLowerCase())
    .join("_");
  if (!result) return "field";
  const safe = /^[0-9]/.test(result) ? `n_${result}` : result;
  return RUST_KEYWORDS.has(safe) ? `r#${safe}` : safe;
}

// ---------------------------------------------------------------------------------------------
// Definitions

interface Definitions {
  readonly byRef: Map<string, { readonly name: string; readonly schema: Json }>;
  readonly byName: Map<string, Json>;
}

function stripDocs(value: Json): Json {
  if (Array.isArray(value)) return value.map(stripDocs);
  if (!isObj(value)) return value;
  return Object.fromEntries(
    Object.entries(value)
      .filter(([key]) => key !== "description" && key !== "title" && key !== "$schema")
      .map(([key, child]) => [key, stripDocs(child)]),
  );
}

function canonical(value: Json): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (!isObj(value)) return JSON.stringify(value);
  return `{${Object.keys(value)
    .toSorted()
    .map((key) => `${JSON.stringify(key)}:${canonical(value[key]!)}`)
    .join(",")}}`;
}

function prepare(schema: Json, name: string): Json {
  return stripNullDefaults(normalizeNullableTypes(DefinitionOverrides[name] ?? schema));
}

function loadDefinitions(bundle: Obj): Definitions {
  const definitions = bundle.definitions as Obj;
  const v2 = definitions.v2 as Obj;
  const byRef = new Map<string, { name: string; schema: Json }>();
  const byName = new Map<string, Json>();
  for (const [name, schema] of Object.entries(v2)) {
    const prepared = prepare(schema, name);
    byRef.set(`#/definitions/v2/${name}`, { name, schema: prepared });
    byName.set(name, prepared);
  }
  for (const [name, schema] of Object.entries(definitions)) {
    if (name === "v2") continue;
    const prepared = prepare(schema, name);
    const existing = byName.get(name);
    if (existing !== undefined) {
      const same =
        canonical(stripDocs(existing)).replaceAll("#/definitions/v2/", "#/definitions/") ===
        canonical(stripDocs(prepared)).replaceAll("#/definitions/v2/", "#/definitions/");
      if (!same) {
        // Same name, different shape: keep both, the root one prefixed.
        const rootName = `Root${name}`;
        byRef.set(`#/definitions/${name}`, { name: rootName, schema: prepared });
        byName.set(rootName, prepared);
        console.warn(`warning: root and v2 definitions of ${name} differ; root is ${rootName}`);
        continue;
      }
      byRef.set(`#/definitions/${name}`, { name, schema: existing });
      continue;
    }
    byRef.set(`#/definitions/${name}`, { name, schema: prepared });
    byName.set(name, prepared);
  }
  for (const [name, schema] of Object.entries(ManualSchemas)) {
    if (!byName.has(name)) {
      const prepared = stripNullDefaults(normalizeNullableTypes(schema));
      byName.set(name, prepared);
      byRef.set(`#/manual/${name}`, { name, schema: prepared });
    }
  }
  return { byRef, byName };
}

// ---------------------------------------------------------------------------------------------
// Rust emission

interface Emitter {
  readonly defs: Definitions;
  readonly items: Map<string, string>; // type name -> code
  readonly order: string[];
  readonly pending: string[]; // definition names to emit
  readonly requested: Set<string>;
  readonly usedNames: Set<string>;
}

function refName(emitter: Emitter, ref: string): string {
  const entry = emitter.defs.byRef.get(ref);
  if (!entry) throw new Error(`Unresolved $ref ${ref}`);
  requestDefinition(emitter, entry.name);
  return entry.name;
}

function requestDefinition(emitter: Emitter, name: string) {
  if (emitter.requested.has(name)) return;
  emitter.requested.add(name);
  emitter.usedNames.add(name);
  emitter.pending.push(name);
}

function deref(emitter: Emitter, schema: Json): Json {
  let current = schema;
  for (let depth = 0; depth < 16; depth += 1) {
    if (isObj(current) && typeof current.$ref === "string") {
      const entry = emitter.defs.byRef.get(current.$ref);
      if (!entry) throw new Error(`Unresolved $ref ${current.$ref}`);
      current = entry.schema;
      continue;
    }
    if (
      isObj(current) &&
      Array.isArray(current.allOf) &&
      current.allOf.length === 1 &&
      !("properties" in current)
    ) {
      current = current.allOf[0]!;
      continue;
    }
    return current;
  }
  return current;
}

function docComment(schema: Json, indent = ""): string {
  if (!isObj(schema) || typeof schema.description !== "string") return "";
  const text = schema.description.trim();
  if (!text) return "";
  return (
    text
      .split("\n")
      .map((line) => `${indent}///${line.trim() ? ` ${line.trimEnd().replaceAll("*/", "* /")}` : ""}`)
      .join("\n") + "\n"
  );
}

function uniqueName(emitter: Emitter, base: string): string {
  let name = base;
  let counter = 2;
  while (emitter.usedNames.has(name) || emitter.defs.byName.has(name)) {
    name = `${base}${counter}`;
    counter += 1;
  }
  emitter.usedNames.add(name);
  return name;
}

function nullableInner(schema: Json): Json | undefined {
  if (!isObj(schema) || !Array.isArray(schema.anyOf)) return undefined;
  const alternatives = schema.anyOf;
  const nonNull = alternatives.filter((alt) => !(isObj(alt) && alt.type === "null"));
  if (nonNull.length === alternatives.length - 1 && nonNull.length >= 1) {
    if (nonNull.length === 1) return nonNull[0]!;
    return { ...schema, anyOf: nonNull };
  }
  return undefined;
}

function isValueSchema(schema: Json): boolean {
  if (schema === true) return true;
  if (!isObj(schema)) return false;
  const keys = Object.keys(schema).filter(
    (key) => key !== "description" && key !== "title" && key !== "default" && key !== "$schema",
  );
  return keys.length === 0;
}

/** Rust type expression for a schema in field/item position; may emit an inline named type. */
function typeOf(emitter: Emitter, schema: Json, nameHint: string): string {
  if (isValueSchema(schema)) return "serde_json::Value";
  if (!isObj(schema)) return "serde_json::Value";
  if (typeof schema.$ref === "string") return refName(emitter, schema.$ref);
  if (Array.isArray(schema.allOf) && schema.allOf.length === 1 && !("properties" in schema)) {
    return typeOf(emitter, schema.allOf[0]!, nameHint);
  }
  const inner = nullableInner(schema);
  if (inner !== undefined) return `Option<${typeOf(emitter, inner, nameHint)}>`;
  const type = schema.type;
  if (Array.isArray(schema.enum) && (type === "string" || type === undefined)) {
    const values = schema.enum.filter((value): value is string => typeof value === "string");
    if (values.length === 1 && type === "string") return "String";
    const name = uniqueName(emitter, nameHint);
    emitStringEnum(emitter, name, values, schema);
    return name;
  }
  if (Array.isArray(schema.oneOf) || Array.isArray(schema.anyOf)) {
    const name = uniqueName(emitter, nameHint);
    emitUnion(emitter, name, schema);
    return name;
  }
  switch (type) {
    case "string":
      return "String";
    case "integer":
      return "i64";
    case "number":
      return "crate::Number";
    case "boolean":
      return "bool";
    case "null":
      return "()";
    case "array": {
      const items = schema.items;
      if (items === undefined) return "Vec<serde_json::Value>";
      return `Vec<${typeOf(emitter, items, `${nameHint}Item`)}>`;
    }
    case "object":
    case undefined: {
      if (isObj(schema.properties) && Object.keys(schema.properties).length > 0) {
        const name = uniqueName(emitter, nameHint);
        emitStruct(emitter, name, schema);
        return name;
      }
      const additional = schema.additionalProperties;
      if (isObj(additional) && !isValueSchema(additional)) {
        return `std::collections::BTreeMap<String, ${typeOf(emitter, additional, `${nameHint}Value`)}>`;
      }
      return "serde_json::Map<String, serde_json::Value>";
    }
    default:
      return "serde_json::Value";
  }
}

function fieldLines(
  emitter: Emitter,
  parent: string,
  properties: Obj,
  required: ReadonlySet<string>,
): string[] {
  const lines: string[] = [];
  const used = new Set<string>();
  for (const [jsonName, propertySchema] of Object.entries(properties)) {
    let field = snake(jsonName.replace(/^\$/, ""));
    if (jsonName.startsWith("_") && field === snake(jsonName.slice(1))) field = snake(jsonName);
    while (used.has(field)) field = `${field}_`;
    used.add(field);
    const isRequired = required.has(jsonName);
    const hint = `${parent}${pascal(jsonName)}`;
    const valueLike = isValueSchema(propertySchema);
    const inner = nullableInner(propertySchema);
    const attrs: string[] = [];
    if (field.replace(/^r#/, "") !== jsonName) attrs.push(`rename = ${JSON.stringify(jsonName)}`);
    let rustType: string;
    if (valueLike) {
      if (isRequired) {
        rustType = "serde_json::Value";
      } else {
        rustType = "Option<serde_json::Value>";
        attrs.push(
          "default",
          'skip_serializing_if = "Option::is_none"',
          'deserialize_with = "crate::serde_helpers::value_present"',
        );
      }
    } else if (inner !== undefined) {
      const innerType = typeOf(emitter, inner, hint);
      if (isRequired) {
        rustType = `Option<${innerType}>`;
        attrs.push('deserialize_with = "crate::serde_helpers::nullable"');
      } else {
        rustType = `Option<Option<${innerType}>>`;
        attrs.push(
          "default",
          'skip_serializing_if = "Option::is_none"',
          'with = "crate::serde_helpers::double_option"',
        );
      }
    } else {
      const innerType = typeOf(emitter, propertySchema, hint);
      if (isRequired) {
        rustType = innerType;
      } else {
        rustType = `Option<${innerType}>`;
        attrs.push("default", 'skip_serializing_if = "Option::is_none"');
      }
    }
    const doc = docComment(propertySchema, "    ");
    lines.push(
      `${doc}${attrs.length > 0 ? `    #[serde(${attrs.join(", ")})]\n` : ""}    pub ${field}: ${rustType},`,
    );
  }
  return lines;
}

function emitStruct(emitter: Emitter, name: string, schema: Obj) {
  emitter.order.push(name);
  emitter.items.set(name, ""); // reserve position
  const properties = (schema.properties as Obj | undefined) ?? {};
  const required = new Set(
    (Array.isArray(schema.required) ? schema.required : []).filter(
      (entry): entry is string => typeof entry === "string",
    ),
  );
  const fields = fieldLines(emitter, name, properties, required);
  emitter.items.set(
    name,
    `${docComment(schema)}#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]\npub struct ${name} {\n${fields.join("\n")}\n}\n`,
  );
}

function emitStringEnum(emitter: Emitter, name: string, values: readonly string[], schema: Json) {
  emitter.order.push(name);
  const used = new Set<string>();
  const variants = values.map((value) => {
    let variant = pascal(value);
    if (variant === "Unknown") variant = "UnknownValue";
    while (used.has(variant)) variant = `${variant}_`;
    used.add(variant);
    return { value, variant };
  });
  const code = [
    `${docComment(schema)}#[derive(Debug, Clone, PartialEq, Eq, Hash)]`,
    `pub enum ${name} {`,
    ...variants.map(({ value, variant }) => `    /// \`${JSON.stringify(value).slice(1, -1)}\`\n    ${variant},`),
    "    /// A value this protocol version does not list (tolerated).",
    "    Unknown(String),",
    "}",
    "",
    `impl ${name} {`,
    `    pub const ALL: &'static [${name}] = &[${variants.map(({ variant }) => `${name}::${variant}`).join(", ")}];`,
    "    pub fn as_str(&self) -> &str {",
    "        match self {",
    ...variants.map(({ value, variant }) => `            ${name}::${variant} => ${JSON.stringify(value)},`),
    `            ${name}::Unknown(value) => value.as_str(),`,
    "        }",
    "    }",
    "    pub fn from_wire(value: &str) -> Self {",
    "        match value {",
    ...variants.map(({ value, variant }) => `            ${JSON.stringify(value)} => ${name}::${variant},`),
    `            other => ${name}::Unknown(other.to_string()),`,
    "        }",
    "    }",
    "}",
    "",
    `crate::string_enum_serde!(${name});`,
    "",
  ].join("\n");
  emitter.items.set(name, code);
}

interface UnionVariant {
  readonly schema: Json; // as written (maybe a $ref)
  readonly resolved: Json;
}

function singleLiteral(emitter: Emitter, schema: Json): string | undefined {
  const resolved = deref(emitter, schema);
  if (!isObj(resolved)) return undefined;
  if (Array.isArray(resolved.enum) && resolved.enum.length === 1 && typeof resolved.enum[0] === "string") {
    return resolved.enum[0];
  }
  if (typeof resolved.const === "string") return resolved.const;
  return undefined;
}

function flattenAlternatives(emitter: Emitter, schema: Obj): UnionVariant[] {
  const key = Array.isArray(schema.oneOf) ? "oneOf" : "anyOf";
  const alternatives = (schema[key] as Json[]).filter((alt) => !(isObj(alt) && alt.type === "null"));
  // Fold shared fields into each object alternative (generate.ts's adaptSchemaForEffect).
  const shared = isObj(schema.properties) ? schema.properties : undefined;
  const sharedRequired = Array.isArray(schema.required) ? schema.required : [];
  return alternatives.map((alt) => {
    let written = alt;
    if (shared && isObj(alt) && alt.type === "object") {
      written = {
        ...alt,
        properties: { ...shared, ...((alt.properties as Obj | undefined) ?? {}) },
        required: [...new Set([...sharedRequired, ...((alt.required as Json[] | undefined) ?? [])])],
      };
    }
    return { schema: written, resolved: deref(emitter, written) };
  });
}

function emitUnion(emitter: Emitter, name: string, schema: Obj) {
  emitter.order.push(name);
  emitter.items.set(name, "");
  const variants = flattenAlternatives(emitter, schema);

  // All string literals → one string enum.
  if (
    variants.every(
      ({ resolved }) =>
        isObj(resolved) && Array.isArray(resolved.enum) && (resolved.type === "string" || resolved.type === undefined),
    )
  ) {
    emitter.order.pop();
    emitter.items.delete(name);
    const values = variants.flatMap(({ resolved }) =>
      ((resolved as Obj).enum as Json[]).filter((value): value is string => typeof value === "string"),
    );
    emitStringEnum(emitter, name, [...new Set(values)], schema);
    return;
  }

  // Objects sharing a single-literal property → tagged.
  const objectVariants = variants.every(
    ({ resolved }) => isObj(resolved) && isObj(resolved.properties),
  );
  if (objectVariants && variants.length > 0) {
    const propertySets = variants.map(({ resolved }) => Object.keys((resolved as Obj).properties as Obj));
    const common = propertySets[0]!.filter((key) => propertySets.every((set) => set.includes(key)));
    const preferred = ["type", "method", "mode", "kind", "handlerType", "status", "action", "_tag"];
    const candidates = [
      ...preferred.filter((key) => common.includes(key)),
      ...common.filter((key) => !preferred.includes(key)),
    ];
    const tag = candidates.find((key) => {
      const literals = variants.map(({ resolved }) =>
        singleLiteral(emitter, ((resolved as Obj).properties as Obj)[key]!),
      );
      return literals.every((value) => value !== undefined) && new Set(literals).size === literals.length;
    });
    if (tag) {
      const arms: { literal: string; variant: string; type: string }[] = [];
      const usedVariants = new Set<string>();
      for (const { schema: written, resolved } of variants) {
        const literal = singleLiteral(emitter, ((resolved as Obj).properties as Obj)[tag]!)!;
        let variant = pascal(literal);
        if (variant === "Unknown") variant = "UnknownValue";
        while (usedVariants.has(variant)) variant = `${variant}_`;
        usedVariants.add(variant);
        let type: string;
        if (isObj(written) && typeof written.$ref === "string") {
          type = refName(emitter, written.$ref);
        } else {
          const structName = uniqueName(emitter, `${name}${variant}`);
          emitStruct(emitter, structName, resolved as Obj);
          type = structName;
        }
        arms.push({ literal, variant, type });
      }
      emitter.items.set(
        name,
        [
          `${docComment(schema)}#[derive(Debug, Clone, PartialEq, Serialize)]`,
          "#[serde(untagged)]",
          `pub enum ${name} {`,
          ...arms.map(({ literal, variant, type }) => `    /// \`${tag}: ${JSON.stringify(literal).slice(1, -1)}\`\n    ${variant}(${type}),`),
          `    /// A \`${tag}\` this protocol version does not list (tolerated, kept as sent).`,
          "    Unknown(serde_json::Value),",
          "}",
          "",
          `impl ${name} {`,
          `    pub const TAG: &'static str = ${JSON.stringify(tag)};`,
          `    /// The \`${tag}\` value.`,
          "    pub fn tag(&self) -> Option<&str> {",
          "        match self {",
          ...arms.map(({ literal, variant }) => `            ${name}::${variant}(_) => Some(${JSON.stringify(literal)}),`),
          `            ${name}::Unknown(value) => value.get(${JSON.stringify(tag)}).and_then(|tag| tag.as_str()),`,
          "        }",
          "    }",
          "}",
          "",
          `impl Default for ${name} {`,
          `    fn default() -> Self {`,
          `        ${name}::Unknown(serde_json::Value::Null)`,
          "    }",
          "}",
          "",
          `impl<'de> Deserialize<'de> for ${name} {`,
          "    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {",
          "        let value = serde_json::Value::deserialize(deserializer)?;",
          `        let tag = value.get(${JSON.stringify(tag)}).and_then(|tag| tag.as_str()).map(str::to_owned);`,
          "        match tag.as_deref() {",
          ...arms.map(
            ({ literal, variant }) =>
              `            Some(${JSON.stringify(literal)}) => serde_json::from_value(value).map(${name}::${variant}).map_err(serde::de::Error::custom),`,
          ),
          `            Some(_) => Ok(${name}::Unknown(value)),`,
          `            None => Err(serde::de::Error::custom(${JSON.stringify(`${name}: missing or non-string \`${tag}\``)})),`,
          "        }",
          "    }",
          "}",
          "",
        ].join("\n"),
      );
      return;
    }
  }

  // Anything else: untagged, tried in order, then anything.
  const arms: { variant: string; type: string }[] = [];
  variants.forEach(({ schema: written }, index) => {
    const type = typeOf(emitter, written, `${name}Variant${index}`);
    if (arms.some((arm) => arm.type === type)) return;
    arms.push({ variant: `V${index}`, type });
  });
  emitter.items.set(
    name,
    [
      `${docComment(schema)}#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]`,
      "#[serde(untagged)]",
      `pub enum ${name} {`,
      ...arms.map(({ variant, type }) => `    ${variant}(${type}),`),
      "    /// A value no listed member accepts (tolerated, kept as sent).",
      "    Other(serde_json::Value),",
      "}",
      "",
      `impl Default for ${name} {`,
      `    fn default() -> Self {`,
      `        ${name}::Other(serde_json::Value::Null)`,
      "    }",
      "}",
      "",
    ].join("\n"),
  );
}

function emitDefinition(emitter: Emitter, name: string) {
  const schema = emitter.defs.byName.get(name)!;
  if (isValueSchema(schema)) {
    emitter.order.push(name);
    emitter.items.set(name, `${docComment(schema)}pub type ${name} = serde_json::Value;\n`);
    return;
  }
  const obj = schema as Obj;
  const inner = nullableInner(obj);
  const isTypeAlias =
    typeof obj.$ref === "string" ||
    inner !== undefined ||
    (Array.isArray(obj.allOf) && obj.allOf.length === 1 && !("properties" in obj)) ||
    (["string", "integer", "number", "boolean", "array"].includes(obj.type as string) &&
      !Array.isArray(obj.enum)) ||
    ((obj.type === "object" || obj.type === undefined) &&
      !(isObj(obj.properties) && Object.keys(obj.properties).length > 0) &&
      !Array.isArray(obj.oneOf) &&
      !Array.isArray(obj.anyOf) &&
      !Array.isArray(obj.enum));
  if (isTypeAlias) {
    emitter.order.push(name);
    emitter.items.set(name, "");
    const rust = typeOf(emitter, schema, `${name}Inner`);
    emitter.items.set(name, `${docComment(schema)}pub type ${name} = ${rust};\n`);
    return;
  }
  if (Array.isArray(obj.enum)) {
    emitStringEnum(
      emitter,
      name,
      obj.enum.filter((value): value is string => typeof value === "string"),
      schema,
    );
    return;
  }
  if (Array.isArray(obj.oneOf) || Array.isArray(obj.anyOf)) {
    emitUnion(emitter, name, obj);
    return;
  }
  emitStruct(emitter, name, obj);
}

// ---------------------------------------------------------------------------------------------
// Methods

interface MethodSpec {
  readonly method: string;
  readonly params?: string; // Rust type name; undefined = no params
  readonly response?: string;
}

function methodConst(method: string): string {
  return method
    .split("/")
    .flatMap((segment) => words(segment))
    .map((word) => word.toUpperCase())
    .join("_");
}

function variantName(method: string): string {
  return method
    .split("/")
    .map((segment) => pascal(segment))
    .join("");
}

function unionMethods(emitter: Emitter, union: Obj): Map<string, string | undefined> {
  const methods = new Map<string, string | undefined>();
  for (const alt of union.oneOf as Json[]) {
    const properties = (alt as Obj).properties as Obj;
    const method = singleLiteral(emitter, properties.method!)!;
    const params = properties.params;
    methods.set(method, isObj(params) && typeof params.$ref === "string" ? params.$ref : undefined);
  }
  return methods;
}

function resolveResponse(emitter: Emitter, method: string, paramsName: string | undefined): string {
  const override = ResponseOverrides[method];
  const candidates = [
    ...(override ? [override] : []),
    ...(paramsName ? [paramsName.replace(/^Nullable/, "").replace(/Params$/, "Response")] : []),
    `${method
      .split("/")
      .flatMap((segment) => segment.split(/(?=[A-Z])/))
      .flatMap((segment) => segment.split(/[-_]/))
      .filter(Boolean)
      .map((segment) => segment[0]!.toUpperCase() + segment.slice(1))
      .join("")}Response`,
  ];
  for (const candidate of candidates) {
    if (emitter.defs.byName.has(candidate)) {
      requestDefinition(emitter, candidate);
      return candidate;
    }
  }
  throw new Error(`Unable to resolve the response type of ${method}`);
}

// ---------------------------------------------------------------------------------------------

async function main() {
  const bundle = JSON.parse(await readInput("json/codex_app_server_protocol.schemas.json")) as Obj;
  const defs = loadDefinitions(bundle);
  const emitter: Emitter = {
    defs,
    items: new Map(),
    order: [],
    pending: [],
    requested: new Set(),
    usedNames: new Set(),
  };
  const definitions = bundle.definitions as Obj;

  const tsLists = {
    clientRequests: parseRequestEntries(await readInput("typescript/ClientRequest.ts")),
    clientNotifications: parseNotificationEntries(await readInput("typescript/ClientNotification.ts")),
    serverRequests: parseRequestEntries(await readInput("typescript/ServerRequest.ts")),
    serverNotifications: parseNotificationEntries(await readInput("typescript/ServerNotification.ts")),
  };
  const bundleLists = {
    clientRequests: unionMethods(emitter, definitions.ClientRequest as Obj),
    clientNotifications: unionMethods(emitter, definitions.ClientNotification as Obj),
    serverRequests: unionMethods(emitter, definitions.ServerRequest as Obj),
    serverNotifications: unionMethods(emitter, definitions.ServerNotification as Obj),
  };
  // The ts-rs files are the authority, as in generate.ts. They also list methods the bundle's
  // unions leave out: the legacy ones whose schemas generate.ts adds by hand (ManualSchemas)
  // and experimental notifications (`rawResponse*`); the rest must agree with the bundle.
  for (const key of Object.keys(tsLists) as (keyof typeof tsLists)[]) {
    const fromTs = new Set(tsLists[key].map((entry) => entry.method));
    for (const method of bundleLists[key].keys()) {
      if (!fromTs.has(method)) throw new Error(`${key}: ${method} is only in the JSON bundle.`);
    }
    for (const entry of tsLists[key]) {
      if (bundleLists[key].has(entry.method)) {
        const ref = bundleLists[key].get(entry.method);
        const bundleParams = ref === undefined ? undefined : emitter.defs.byRef.get(ref)?.name;
        if (bundleParams !== undefined && bundleParams !== entry.paramsType) {
          throw new Error(`${key}: ${entry.method} takes ${bundleParams} in the bundle, ${entry.paramsType} in ts-rs.`);
        }
      } else {
        console.warn(`note: ${key}: ${entry.method} (${entry.paramsType}) is not in the bundle's union`);
      }
    }
  }

  const paramsOf = (paramsType: string | undefined) => {
    if (paramsType === undefined || paramsType === "undefined") return undefined;
    if (!emitter.defs.byName.has(paramsType)) throw new Error(`Unable to resolve schema type name: ${paramsType}`);
    requestDefinition(emitter, paramsType);
    return paramsType;
  };

  const serverNotifications: MethodSpec[] = tsLists.serverNotifications.map((entry) => {
    const params = paramsOf(entry.paramsType);
    return { method: entry.method, ...(params ? { params } : {}) };
  });
  const serverRequests: MethodSpec[] = tsLists.serverRequests.map((entry) => {
    const params = paramsOf(entry.paramsType);
    return {
      method: entry.method,
      ...(params ? { params } : {}),
      response: resolveResponse(emitter, entry.method, entry.paramsType),
    };
  });
  const clientRequests: MethodSpec[] = USED_CLIENT_REQUESTS.map((method) => {
    const entry = tsLists.clientRequests.find((candidate) => candidate.method === method);
    if (!entry) throw new Error(`Unknown client request ${method}`);
    const params = paramsOf(entry.paramsType);
    return {
      method,
      ...(params ? { params } : {}),
      response: resolveResponse(emitter, method, entry.paramsType),
    };
  });
  const clientNotifications: MethodSpec[] = tsLists.clientNotifications.map((entry) => {
    const params = paramsOf(entry.paramsType);
    return { method: entry.method, ...(params ? { params } : {}) };
  });
  // Not reachable from a method signature but used: the user-input answers the runtime
  // reports, and the experimental `turn/start` fields TS adds with `Schema.fieldsAssign`.
  for (const name of ["ToolRequestUserInputResponse", "CollaborationMode", "AdditionalContextEntry"]) {
    requestDefinition(emitter, name);
  }

  while (emitter.pending.length > 0) {
    emitDefinition(emitter, emitter.pending.shift()!);
  }

  const prelude = [
    "// This file is generated by code/scripts/gen-rust-codex-protocol.ts. Do not edit by hand.",
    `// Upstream protocol: openai/codex@${UPSTREAM_REF} (codex-rs/app-server-protocol/schema).`,
    "",
  ];
  const typesOut = [
    ...prelude,
    "#![allow(clippy::large_enum_variant, clippy::enum_variant_names, non_camel_case_types)]",
    "",
    "use serde::{Deserialize, Serialize};",
    "",
    ...emitter.order.map((name) => emitter.items.get(name)!),
  ].join("\n");

  const methodTable = (constant: string, specs: readonly MethodSpec[]) =>
    [
      `pub const ${constant}: &[&str] = &[`,
      ...specs.map((spec) => `    ${JSON.stringify(spec.method)},`),
      "];",
      "",
    ].join("\n");

  const methodConsts = (specs: readonly MethodSpec[]) =>
    specs.map((spec) => `pub const ${methodConst(spec.method)}: &str = ${JSON.stringify(spec.method)};`).join("\n");

  const serverNotificationEnum = [
    "/// A decoded server notification: one variant per method of the pinned protocol.",
    "#[derive(Debug, Clone, PartialEq)]",
    "pub enum ServerNotification {",
    ...serverNotifications.map((spec) => `    ${variantName(spec.method)}(${spec.params ?? "()"}),`),
    "}",
    "",
    "impl ServerNotification {",
    "    pub fn method(&self) -> &'static str {",
    "        match self {",
    ...serverNotifications.map((spec) => `            ServerNotification::${variantName(spec.method)}(_) => ${JSON.stringify(spec.method)},`),
    "        }",
    "    }",
    "",
    "    /// Decodes `params` for a known `method`: `None` when the method is not part of the",
    "    /// protocol, `Some(Err)` when the params do not match its schema (TS drops those).",
    "    pub fn decode(method: &str, params: Option<serde_json::Value>) -> Option<Result<Self, serde_json::Error>> {",
    "        let params = params.unwrap_or(serde_json::Value::Null);",
    "        Some(match method {",
    ...serverNotifications.map((spec) =>
      spec.params
        ? `            ${JSON.stringify(spec.method)} => serde_json::from_value(params).map(ServerNotification::${variantName(spec.method)}),`
        : `            ${JSON.stringify(spec.method)} => Ok(ServerNotification::${variantName(spec.method)}(())),`,
    ),
    "            _ => return None,",
    "        })",
    "    }",
    "",
    "    /// The params as decoded (keys outside the schema dropped), as JSON.",
    "    pub fn params_value(&self) -> serde_json::Value {",
    "        match self {",
    ...serverNotifications.map((spec) =>
      spec.params
        ? `            ServerNotification::${variantName(spec.method)}(params) => serde_json::to_value(params).unwrap_or(serde_json::Value::Null),`
        : `            ServerNotification::${variantName(spec.method)}(_) => serde_json::Value::Null,`,
    ),
    "        }",
    "    }",
    "}",
    "",
  ].join("\n");

  const serverRequestEnum = [
    "/// A decoded server request (server → client): one variant per method.",
    "#[derive(Debug, Clone, PartialEq)]",
    "pub enum ServerRequest {",
    ...serverRequests.map((spec) => `    ${variantName(spec.method)}(${spec.params ?? "()"}),`),
    "}",
    "",
    "impl ServerRequest {",
    "    pub fn method(&self) -> &'static str {",
    "        match self {",
    ...serverRequests.map((spec) => `            ServerRequest::${variantName(spec.method)}(_) => ${JSON.stringify(spec.method)},`),
    "        }",
    "    }",
    "",
    "    /// `None` when the method is not part of the protocol, `Some(Err)` when the params do",
    "    /// not match its schema.",
    "    pub fn decode(method: &str, params: Option<serde_json::Value>) -> Option<Result<Self, serde_json::Error>> {",
    "        let params = params.unwrap_or(serde_json::Value::Null);",
    "        Some(match method {",
    ...serverRequests.map((spec) =>
      spec.params
        ? `            ${JSON.stringify(spec.method)} => serde_json::from_value(params).map(ServerRequest::${variantName(spec.method)}),`
        : `            ${JSON.stringify(spec.method)} => Ok(ServerRequest::${variantName(spec.method)}(())),`,
    ),
    "            _ => return None,",
    "        })",
    "    }",
    "",
    "    pub fn params_value(&self) -> serde_json::Value {",
    "        match self {",
    ...serverRequests.map((spec) =>
      spec.params
        ? `            ServerRequest::${variantName(spec.method)}(params) => serde_json::to_value(params).unwrap_or(serde_json::Value::Null),`
        : `            ServerRequest::${variantName(spec.method)}(_) => serde_json::Value::Null,`,
    ),
    "        }",
    "    }",
    "}",
    "",
    "/// The response type of each server request.",
    "pub mod server_responses {",
    ...serverRequests.map((spec) => `    pub type ${variantName(spec.method)} = super::${spec.response};`),
    "}",
    "",
  ].join("\n");

  const clientRequestMarkers = [
    "/// A client → server request: method name, params and response types.",
    "pub trait ClientRequest {",
    "    const METHOD: &'static str;",
    "    /// False when the method takes no params (`params: undefined`): nothing is sent.",
    "    const HAS_PARAMS: bool;",
    "    type Params: Serialize;",
    "    type Response: serde::de::DeserializeOwned;",
    "}",
    "",
    "/// One marker type per client request zenith code sends.",
    "pub mod client_requests {",
    "    use super::*;",
    ...clientRequests.flatMap((spec) => [
      `    /// \`${spec.method}\``,
      `    pub enum ${variantName(spec.method)} {}`,
      `    impl ClientRequest for ${variantName(spec.method)} {`,
      `        const METHOD: &'static str = ${JSON.stringify(spec.method)};`,
      `        const HAS_PARAMS: bool = ${spec.params ? "true" : "false"};`,
      `        type Params = ${spec.params ?? "()"};`,
      `        type Response = ${spec.response};`,
      "    }",
    ]),
    "}",
    "",
  ].join("\n");

  const methodsOut = [
    ...prelude,
    "use serde::Serialize;",
    "",
    "#[allow(unused_imports)]",
    "use crate::types::*;",
    "",
    `/// The upstream commit the types follow.\npub const UPSTREAM_REF: &str = ${JSON.stringify(UPSTREAM_REF)};`,
    "",
    "/// Method name constants.",
    "pub mod names {",
    ...methodConsts([
      ...serverNotifications,
      ...serverRequests,
      ...clientRequests,
      ...clientNotifications,
    ].filter((spec, index, all) => all.findIndex((other) => other.method === spec.method) === index))
      .split("\n")
      .map((line) => `    ${line}`),
    "}",
    "",
    methodTable("SERVER_NOTIFICATION_METHODS", serverNotifications),
    methodTable("SERVER_REQUEST_METHODS", serverRequests),
    methodTable("CLIENT_NOTIFICATION_METHODS", clientNotifications),
    methodTable("CLIENT_REQUEST_METHODS_USED", clientRequests),
    serverNotificationEnum,
    serverRequestEnum,
    clientRequestMarkers,
  ].join("\n");

  const srcDir = Path.join(crateDir, "src");
  Fs.mkdirSync(srcDir, { recursive: true });
  Fs.writeFileSync(Path.join(srcDir, "types.rs"), typesOut);
  Fs.writeFileSync(Path.join(srcDir, "methods.rs"), methodsOut);
  execFileSync("rustfmt", ["--edition", "2021", Path.join(srcDir, "types.rs"), Path.join(srcDir, "methods.rs")], {
    stdio: "inherit",
  });
  console.log(
    `Generated ${emitter.order.length} types, ${serverNotifications.length} server notifications, ${serverRequests.length} server requests, ${clientRequests.length} client requests from ${UPSTREAM_REF}.`,
  );
}

await main();
