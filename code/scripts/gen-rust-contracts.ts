/**
 * Generates the Rust crate `zc-contracts` (crates/zenith-code/crates/zc-contracts) from the
 * Effect schemas of `packages/contracts`.
 *
 *   node scripts/gen-rust-contracts.ts                 # regenerate code + sampled fixtures
 *   node scripts/gen-rust-contracts.ts --harvest <db>  # also harvest real payloads from a COPY of
 *                                                       # state.sqlite (written to fixtures/harvested,
 *                                                       # which is git-ignored: it holds personal data)
 *
 * It walks the SchemaAST of every export of `packages/contracts/src/index.ts`, of every RPC of
 * `WsRpcGroup` and of every endpoint of `EnvironmentHttpApi`, and emits serde types for the
 * ENCODED (wire) side, following the rules of docs/zenith-code-rust-plan.md §1.5. The mapping
 * table and the known deviations are in docs/zenith-code/contracts.md.
 *
 * The output is deterministic: names come from export order, traversal is depth-first in
 * declaration order, and fixtures are sampled with fixed seeds.
 */
import * as Fs from "node:fs";
import * as Path from "node:path";
import * as Url from "node:url";
import { execFileSync } from "node:child_process";

import * as Contracts from "../packages/contracts/src/index.ts";
import * as Effect from "effect/Effect";
import * as Schema from "effect/Schema";
import * as AST from "effect/SchemaAST";
import * as Arbitrary from "effect/unstable/arbitrary/Arbitrary";
import * as HttpApi from "effect/unstable/httpapi/HttpApi";
import * as RpcSchema from "effect/unstable/rpc/RpcSchema";

const HERE = Path.dirname(Url.fileURLToPath(import.meta.url));
const CODE_ROOT = Path.resolve(HERE, "..");
const REPO_ROOT = Path.resolve(CODE_ROOT, "..");
const CONTRACTS_SRC = Path.join(CODE_ROOT, "packages/contracts/src");
const CRATE = Path.join(REPO_ROOT, "crates/zenith-code/crates/zc-contracts");
const GEN_DIR = Path.join(CRATE, "src/generated");
const FIXTURES = Path.join(CRATE, "fixtures");

const args = process.argv.slice(2);
const harvestIndex = args.indexOf("--harvest");
const HARVEST_DB = harvestIndex >= 0 ? args[harvestIndex + 1] : undefined;

// ---------------------------------------------------------------------------------------------
// Export ranking and modules (from the source text, so names are stable and intuitive)
// ---------------------------------------------------------------------------------------------

const indexSource = Fs.readFileSync(Path.join(CONTRACTS_SRC, "index.ts"), "utf8");
const moduleFiles = [...indexSource.matchAll(/export \* from "\.\/(\w+)\.ts";/g)].map((m) => m[1]!);
const exportRank = new Map<string, number>();
const exportModule = new Map<string, string>();
{
  let rank = 0;
  for (const file of moduleFiles) {
    const text = Fs.readFileSync(Path.join(CONTRACTS_SRC, `${file}.ts`), "utf8");
    for (const m of text.matchAll(/^export (?:const|class|function|let) (\w+)/gm)) {
      if (!exportRank.has(m[1]!)) {
        exportRank.set(m[1]!, rank++);
        exportModule.set(m[1]!, file);
      }
    }
  }
}
const allExports = Object.entries(Contracts as Record<string, unknown>)
  .filter(([, v]) => Schema.isSchema(v))
  .map(([k, v]) => [k, v as Schema.Top] as const)
  .sort(
    ([a], [b]) => (exportRank.get(a) ?? 1e9) - (exportRank.get(b) ?? 1e9) || a.localeCompare(b),
  );

// ---------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------

type A = AST.AST;
const any = (x: unknown) => x as any;

function finalNode(ast: A): A {
  let n = ast;
  while (n.encoding) n = n.encoding[n.encoding.length - 1]!.to;
  return n;
}
function strip(ast: A): A {
  return ast.encoding ? AST.replaceEncoding(ast, undefined) : ast;
}
function ownerOf(ast: A): A {
  return any(AST).getContextOwner ? any(AST).getContextOwner(ast) : ast;
}
function repId(ast: A): string | undefined {
  return any(ast).annotations?.representation?.id;
}
function hasCheck(checks: ReadonlyArray<any> | undefined, id: string): boolean {
  return (checks ?? []).some(
    (c: any) =>
      c.annotations?.representation?.id === id ||
      (c._tag === "FilterGroup" && hasCheck(c.checks, id)),
  );
}
function brandsOf(ast: A): string[] {
  const out: string[] = [];
  const add = (a: any) => {
    if (a && Array.isArray(a.brands)) out.push(...a.brands);
  };
  add(ast.annotations);
  for (const c of ast.checks ?? []) {
    add((c as any).annotations);
    if ((c as any)._tag === "FilterGroup") for (const cc of (c as any).checks) add(cc.annotations);
  }
  return out;
}
function describe(ast: A): string | undefined {
  const a: any = ast.checks
    ? (ast.checks[ast.checks.length - 1] as any).annotations
    : ast.annotations;
  const d = a?.description ?? ast.annotations?.description;
  return typeof d === "string" ? d : undefined;
}
function keyDescription(ast: A): string | undefined {
  const d = any(ast).context?.annotations?.description;
  return typeof d === "string" ? d : describe(ast);
}

const SINGLETONS = new Set<A>(
  [
    Schema.String,
    Schema.Number,
    Schema.Boolean,
    Schema.Unknown,
    Schema.Any,
    Schema.Null,
    Schema.Undefined,
    Schema.Void,
    Schema.Never,
    Schema.ObjectKeyword,
  ].map((s) => s.ast),
);
const PRIMITIVE_TAGS = new Set([
  "String",
  "Number",
  "Boolean",
  "Unknown",
  "Any",
  "Null",
  "Undefined",
  "Void",
  "Never",
  "ObjectKeyword",
  "BigInt",
  "Symbol",
]);
function isBarePrimitive(ast: A): boolean {
  return SINGLETONS.has(ast) || (PRIMITIVE_TAGS.has(ast._tag) && !ast.checks && !ast.encoding);
}

function words(s: string): string[] {
  return s
    .replace(/([a-z0-9])([A-Z])/g, "$1 $2")
    .replace(/([A-Z]+)([A-Z][a-z])/g, "$1 $2")
    .split(/[^A-Za-z0-9]+/)
    .filter(Boolean);
}
function pascal(s: string): string {
  const w = words(s).map((x) => x[0]!.toUpperCase() + x.slice(1));
  let out = w.join("");
  if (out === "") return "";
  if (/^[0-9]/.test(out)) out = "V" + out;
  return out;
}
const RUST_KEYWORDS = new Set(
  "as break const continue crate else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while async await dyn abstract become box do final macro override priv typeof unsized virtual yield try gen".split(
    " ",
  ),
);
function snake(s: string): string {
  let out = words(s)
    .map((x) => x.toLowerCase())
    .join("_");
  if (out === "") out = "field";
  if (/^[0-9]/.test(out)) out = "n" + out;
  if (RUST_KEYWORDS.has(out)) {
    return ["self", "Self", "super", "crate"].includes(out) ? out + "_" : "r#" + out;
  }
  return out;
}
function rustStr(s: string): string {
  return JSON.stringify(s).replace(/\\u([0-9a-fA-F]{4})/g, "\\u{$1}");
}
function rawStr(s: string): string {
  let hashes = "#";
  while (s.includes('"' + hashes)) hashes += "#";
  return `r${hashes}"${s}"${hashes}`;
}
function docLines(doc: string | undefined, indent = ""): string {
  if (!doc) return "";
  return (
    doc
      .trim()
      .split("\n")
      .map((l) => `${indent}///${l.trim() ? " " + l.trimEnd().replace(/^\s*\*\s?/, "") : ""}`)
      .join("\n") + "\n"
  );
}

// ---------------------------------------------------------------------------------------------
// Rust type model
// ---------------------------------------------------------------------------------------------

type Nullability = "none" | "undef" | "null";
type JsonKind = "string" | "number" | "bool" | "object" | "array" | "null" | "any";
interface Info {
  kinds: Set<JsonKind>;
  /** string/number/bool literal values when the value itself is a literal */
  literals?: Array<string | number | boolean>;
  /** required literal-valued keys, for object kinds */
  litFields?: Map<string, Array<string | number | boolean>>;
}
interface TRef {
  base: string;
  nullable: Nullability;
  /** `base` itself is an `Option<..>` alias */
  baseIsOption?: boolean;
  /** inner type when base is `Box<inner>` */
  boxedInner?: string;
  /** forward-compatible member: unknown values decode as absent/null */
  lenient?: boolean;
  info: Info;
  /** name of the generated item, if any */
  item?: string;
}
function render(t: TRef): string {
  return t.nullable !== "none" && !t.baseIsOption ? `Option<${t.base}>` : t.base;
}
function renderUnboxed(t: TRef): string {
  const base = t.boxedInner ?? t.base;
  return t.nullable !== "none" && !t.baseIsOption ? `Option<${base}>` : base;
}

interface Field {
  wire: string;
  rust: string;
  ty: TRef;
  keyOptional: boolean;
  defaultJson?: unknown;
  omitOnEncode?: boolean;
  omittedWhenNull?: boolean;
  doc?: string;
}
interface Member {
  variant: string;
  ty?: TRef; // undefined for literal members
  literal?: string | number | boolean;
  info: Info;
}
type Item =
  | { kind: "struct"; name: string; module: string; doc?: string; fields: Field[]; rest?: TRef }
  | { kind: "newtype"; name: string; module: string; doc?: string; inner: "String" | "i64" }
  | { kind: "alias"; name: string; module: string; doc?: string; target: string }
  | {
      kind: "strEnum";
      name: string;
      module: string;
      doc?: string;
      variants: Array<{ rust: string; wire: string }>;
    }
  | {
      kind: "union";
      name: string;
      module: string;
      doc?: string;
      members: Member[];
      tagKey?: string;
    };

const items = new Map<string, Item>(); // by Rust name
const itemOrder: string[] = [];
const takenNames = new Set<string>([
  "Option",
  "Vec",
  "Box",
  "String",
  "Result",
  "Some",
  "None",
  "Value",
  "Self",
]);
const deviations: string[] = [];

function addItem(item: Item) {
  if (items.has(item.name)) throw new Error(`duplicate item ${item.name}`);
  items.set(item.name, item);
  itemOrder.push(item.name);
}
function freshName(base: string): string {
  let name = base || "Anon";
  if (!/^[A-Za-z]/.test(name)) name = "T" + name;
  if (!takenNames.has(name)) {
    takenNames.add(name);
    return name;
  }
  for (let i = 2; ; i++) {
    if (!takenNames.has(name + i)) {
      takenNames.add(name + i);
      return name + i;
    }
  }
}

// ---------------------------------------------------------------------------------------------
// Names for AST nodes
// ---------------------------------------------------------------------------------------------

const names = new Map<A, string>();
const nodeOfName = new Map<string, A>();
const structural = new Map<unknown, Array<{ node: A; name: string }>>();
const canonicalOfExport = new Map<string, string>(); // export name -> canonical export name

function structKey(ast: A): unknown {
  switch (ast._tag) {
    case "Objects":
      return any(ast).propertySignatures;
    case "Union":
      return any(ast).types;
    case "Declaration":
      return ast.encoding ?? any(ast).run;
    default:
      return undefined;
  }
}
function registerName(ast: A, name: string) {
  if (isBarePrimitive(ast)) return;
  if (!names.has(ast)) names.set(ast, name);
  if (!nodeOfName.has(name)) nodeOfName.set(name, ast);
  const key = structKey(ast);
  if (key !== undefined && (any(key).length ?? 1) > 0) {
    const list = structural.get(key) ?? [];
    if (!list.some((e) => e.node === ast)) list.push({ node: ast, name });
    structural.set(key, list);
  }
}
function lookupName(ast: A): string | undefined {
  const direct = names.get(ast) ?? names.get(ownerOf(ast));
  if (direct) return direct;
  const key = structKey(ast);
  if (key === undefined) return undefined;
  const list = structural.get(key);
  return list?.find((e) => e.node.encoding === ast.encoding)?.name;
}

for (const [name] of allExports) takenNames.add(name);
for (const [name, schema] of allExports) {
  const ast = schema.ast;
  const existing = names.get(ast);
  if (existing) {
    canonicalOfExport.set(name, existing);
    continue;
  }
  if (isBarePrimitive(ast)) continue;
  takenNames.add(name);
  registerName(ast, name);
  canonicalOfExport.set(name, name);
}
for (const [name, schema] of allExports) {
  if (canonicalOfExport.get(name) !== name) continue;
  const ast = schema.ast;
  try {
    const enc = AST.toEncoded(ast);
    if (enc !== ast && ["Objects", "Union"].includes(enc._tag)) registerName(enc, name);
  } catch {
    /* some declarations cannot be flipped; fine */
  }
  if (ast._tag === "Declaration" && ast.encoding) {
    const fin = finalNode(ast);
    if (fin._tag === "Objects") registerName(fin, name);
  }
}

// ---------------------------------------------------------------------------------------------
// Literal unit types
// ---------------------------------------------------------------------------------------------

const lits = new Map<string, { name: string; value: string | number | boolean }>();
function litRef(value: string | number | boolean): TRef {
  const key = typeof value + ":" + String(value);
  let lit = lits.get(key);
  if (!lit) {
    let base: string;
    if (typeof value === "string") base = "Lit" + (pascal(value) || "Empty");
    else if (typeof value === "boolean") base = value ? "LitTrue" : "LitFalse";
    else base = "Lit" + String(value).replace("-", "Neg").replace(".", "_");
    lit = { name: freshName(base), value };
    lits.set(key, lit);
  }
  return {
    base: lit.name,
    nullable: "none",
    info: { kinds: new Set([jsonKindOf(value)]), literals: [value] },
  };
}
function jsonKindOf(v: string | number | boolean): JsonKind {
  return typeof v === "string" ? "string" : typeof v === "number" ? "number" : "bool";
}

// ---------------------------------------------------------------------------------------------
// The walker
// ---------------------------------------------------------------------------------------------

interface Hint {
  base: string;
  module: string;
}
const prim = (base: string, kinds: JsonKind[], extra: Partial<TRef> = {}): TRef => ({
  base,
  nullable: "none",
  info: { kinds: new Set(kinds) },
  ...extra,
});
const VALUE = () => prim("serde_json::Value", ["any"]);

const namedRefs = new Map<A, TRef>(); // named node -> ref (also placeholder while building)
const anonRefs = new Map<A, TRef>(); // anonymous node -> ref
const itemInfo = new Map<string, Info>();

function ref(ast: A, hint: Hint): TRef {
  const owner = ownerOf(ast);
  const name = lookupName(ast);
  if (name) return namedRef(owner, name);
  const cached = anonRefs.get(owner);
  if (cached) return cached;
  const t = shapeOf(owner, undefined, hint);
  anonRefs.set(owner, t);
  return t;
}

function namedRef(node: A, name: string): TRef {
  const canonicalNode = nodeOfName.get(name) ?? node;
  const cached = namedRefs.get(canonicalNode);
  if (cached) return cached;
  const module = exportModule.get(name) ?? "misc";
  const placeholder: TRef = {
    base: name,
    nullable: "none",
    info: { kinds: new Set(["any"]) },
    item: name,
  };
  namedRefs.set(canonicalNode, placeholder);
  const t = shapeOf(canonicalNode, name, { base: name, module });
  let out: TRef;
  if (t.item === name) {
    out = t;
  } else {
    // the shape is a primitive or a reference to another item: alias or newtype
    const brands = brandsOf(canonicalNode);
    const doc = describe(canonicalNode);
    if (brands.length > 0 && t.nullable === "none" && (t.base === "String" || t.base === "i64")) {
      addItem({ kind: "newtype", name, module, doc, inner: t.base as "String" | "i64" });
      out = { base: name, nullable: "none", info: t.info, item: name };
    } else {
      addItem({ kind: "alias", name, module, doc, target: render(t) });
      out = {
        base: name,
        nullable: t.nullable,
        baseIsOption: t.nullable !== "none" || t.baseIsOption,
        lenient: t.lenient,
        info: t.info,
        item: name,
      };
    }
  }
  itemInfo.set(name, out.info);
  Object.assign(placeholder, out);
  return placeholder;
}

/** Shapes a node. When `name` is given, the item created (if any) gets that name. */
function shapeOf(ast: A, name: string | undefined, hint: Hint): TRef {
  if (ast.encoding) return shapeEncoded(ast, name, hint);
  switch (ast._tag) {
    case "String":
    case "TemplateLiteral":
    case "Symbol":
    case "BigInt":
      return prim("String", ["string"]);
    case "Number":
      if (hasCheck(ast.checks, "effect/schema/isInt")) return prim("i64", ["number"]);
      return prim("JsNumber", ["number", "string"]);
    case "Boolean":
      return prim("bool", ["bool"]);
    case "Literal": {
      const v = any(ast).literal;
      if (typeof v === "bigint") return litRef(String(v));
      return litRef(v);
    }
    case "Null":
      return prim("()", ["null"], { nullable: "none" });
    case "Undefined":
    case "Void":
      return prim("()", ["null"]);
    case "Never":
      return prim("Never", []);
    case "Unknown":
    case "Any":
    case "ObjectKeyword":
      return VALUE();
    case "Enum":
      throw new Error("Enum schemas are not supported");
    case "Suspend": {
      const inner = ref(any(ast).thunk(), hint);
      return {
        ...inner,
        base: `Box<${inner.base}>`,
        boxedInner: inner.base,
        baseIsOption: false,
        nullable: inner.baseIsOption ? "none" : inner.nullable,
        item: undefined,
      };
    }
    case "Declaration":
      return shapeDeclaration(ast, name, hint);
    case "Arrays":
      return shapeArrays(ast, name, hint);
    case "Objects":
      return shapeObjects(ast, name, hint);
    case "Union":
      return shapeUnion(ast, name, hint);
  }
  throw new Error(`unsupported AST ${ast._tag}`);
}

/** Transformed schemas whose encoder does not write the type side (see `shapeEncoded`). */
const ENCODED_SIDE_SCHEMAS = new Set(["ProjectIconOverride"]);

function shapeEncoded(ast: A, name: string | undefined, hint: Hint): TRef {
  const last = ast.encoding![ast.encoding!.length - 1]!.to;
  const fin = finalNode(ast);
  // Forward-compatible helpers: the wire side is Unknown, the type side says what is expected.
  if (fin._tag === "Unknown" && ast._tag !== "Unknown") {
    return { ...shapeOf(strip(ast), name, hint), lenient: true };
  }
  if (
    ast._tag === "Arrays" &&
    fin._tag === "Arrays" &&
    any(fin).elements.length === 0 &&
    any(fin).rest.length === 1 &&
    any(fin).rest[0]._tag === "Unknown"
  ) {
    const el = any(ast).rest[0] as A;
    const t = ref(el, { base: hint.base + "Item", module: hint.module });
    return {
      base: `LenientVec<${renderUnboxed(t)}>`,
      nullable: "none",
      info: { kinds: new Set(["array"]) },
    };
  }
  if (ast._tag === "Declaration") {
    if (repId(ast) === "effect/schema/DateTimeUtc" && fin._tag === "String")
      return prim("DateTimeUtc", ["string"]);
    return refOrShape(last, name, hint);
  }
  const structuralTags = ["Objects", "Union", "Arrays"];
  // The encoder of these writes a different shape than the type side (ProjectIconOverride
  // encodes a monogram as `{kind: "lucide", name: "folder-code", monogramText}` for older
  // peers), so the wire is the encoded union, which Rust must read and write as is.
  if (name !== undefined && ENCODED_SIDE_SCHEMAS.has(name) && structuralTags.includes(fin._tag)) {
    return shapeOf(fin, name, hint);
  }
  if (structuralTags.includes(ast._tag) && structuralTags.includes(fin._tag)) {
    // A legacy-decoding transformation (decodes older shapes): the encoder writes the
    // canonical shape, which is the type side encoded. Model that.
    if (!(AST.isOptional(fin) && !AST.isOptional(ast))) {
      deviations.push(
        `${name ?? hint.base}: also decodes legacy shapes in TS (${fin._tag} → ${ast._tag}); Rust accepts the canonical shape only`,
      );
    }
    return shapeOf(strip(ast), name, hint);
  }
  return refOrShape(last, name, hint);
}

/** The encoded side `to` of a link: use its own name if it has one, else shape it under ours. */
function refOrShape(to: A, name: string | undefined, hint: Hint): TRef {
  const n = lookupName(to);
  if (n && n !== name) return namedRef(ownerOf(to), n);
  return shapeOf(ownerOf(to), name, hint);
}

const declCodecs = new Map<string, TRef>();
function shapeDeclaration(ast: A, name: string | undefined, hint: Hint): TRef {
  const id = repId(ast);
  const tps = any(ast).typeParameters as A[];
  switch (id) {
    case "effect/schema/Option": {
      const inner = ref(tps[0]!, { base: hint.base + "Value", module: hint.module });
      return {
        base: `EOption<${renderUnboxed(inner)}>`,
        nullable: "none",
        info: { kinds: new Set(["object"]), litFields: new Map([["_tag", ["Some", "None"]]]) },
      };
    }
    case "effect/schema/DateTimeUtc":
      return prim("DateTimeUtc", ["string"]);
    case "effect/schema/Uint8Array":
      return prim("Base64Bytes", ["string"]);
    case "effect/schema/Json":
      return VALUE();
  }
  const getLink = any(ast).annotations?.toCodecJson ?? any(ast).annotations?.toCodec;
  if (typeof getLink !== "function") return VALUE();
  const link = getLink(tps.map((tp) => Schema.make(AST.toEncoded(tp))));
  if (!link) return VALUE();
  const key = id ?? "";
  if (tps.length === 0 && key && declCodecs.has(key)) return declCodecs.get(key)!;
  const declName = name ?? (id ? freshName("Effect" + pascal(id.split("/").pop()!)) : undefined);
  const t = shapeOf(link.to, declName, {
    base: declName ?? hint.base,
    module: declName && !name ? "prim_codecs" : hint.module,
  });
  if (tps.length === 0 && key) declCodecs.set(key, t);
  return t;
}

function shapeArrays(ast: A, name: string | undefined, hint: Hint): TRef {
  const elements = any(ast).elements as A[];
  const rest = any(ast).rest as A[];
  const arr = (base: string): TRef => ({
    base,
    nullable: "none",
    info: { kinds: new Set(["array"]) },
  });
  if (elements.length === 0 && rest.length === 1) {
    const t = ref(rest[0]!, { base: hint.base + "Item", module: hint.module });
    return arr(`Vec<${renderUnboxed(t)}>`);
  }
  if (rest.length === 0 && elements.every((e) => !AST.isOptional(e))) {
    const ts = elements.map((e, i) =>
      ref(e, { base: hint.base + "Item" + i, module: hint.module }),
    );
    return arr(`(${ts.map(renderUnboxed).join(", ")}${ts.length === 1 ? "," : ""})`);
  }
  if (rest.length === 1 && elements.every((e) => !AST.isOptional(e))) {
    // non-empty arrays and friends
    const t = ref(rest[0]!, { base: hint.base + "Item", module: hint.module });
    return arr(`Vec<${renderUnboxed(t)}>`);
  }
  deviations.push(`${name ?? hint.base}: tuple shape modelled as Vec<serde_json::Value>`);
  return arr("Vec<serde_json::Value>");
}

function fieldHint(parent: Hint, wire: string): Hint {
  return { base: parent.base + pascal(wire), module: parent.module };
}

function shapeObjects(ast: A, name: string | undefined, hint: Hint): TRef {
  const props = any(ast).propertySignatures as Array<{ name: PropertyKey; type: A }>;
  const indexes = any(ast).indexSignatures as Array<{ parameter: A; type: A }>;
  if (props.length === 0 && indexes.length === 1 && !name) {
    const keyT = ref(indexes[0]!.parameter, { base: hint.base + "Key", module: hint.module });
    const valT = ref(indexes[0]!.type, { base: hint.base + "Value", module: hint.module });
    const keyTy = keyT.base === "String" || keyT.item ? renderUnboxed(keyT) : "String";
    return {
      base: `BTreeMap<${keyTy}, ${renderUnboxed(valT)}>`,
      nullable: "none",
      info: { kinds: new Set(["object"]) },
    };
  }
  const structName = name ?? freshName(hint.base);
  const self: Hint = { base: structName, module: hint.module };
  const result: TRef = {
    base: structName,
    nullable: "none",
    info: { kinds: new Set(["object"]), litFields: new Map() },
    item: structName,
  };
  if (!name) anonRefs.set(ast, result); // recursion guard for anonymous structs
  const fields: Field[] = [];
  const usedRust = new Set<string>();
  for (const ps of props) {
    const wire = String(ps.name);
    const f = fieldOf(wire, ps.type, self);
    let rust = snake(wire.replace(/^_+/, "") || wire);
    while (usedRust.has(rust)) rust += "_";
    usedRust.add(rust);
    f.rust = rust;
    fields.push(f);
    if (
      !f.keyOptional &&
      f.defaultJson === undefined &&
      f.ty.nullable === "none" &&
      !f.ty.lenient
    ) {
      const lits = f.ty.info.literals;
      if (lits && lits.length > 0 && f.ty.info.kinds.size === 1)
        result.info.litFields!.set(wire, lits);
    }
  }
  let rest: TRef | undefined;
  if (indexes.length > 0) {
    if (indexes.length > 1)
      deviations.push(`${structName}: several index signatures, only the first is modelled`);
    rest = ref(indexes[0]!.type, { base: structName + "Rest", module: hint.module });
  }
  if (name && props.length === 0 && indexes.length === 1) {
    // named record: alias to a map
    const keyT = ref(indexes[0]!.parameter, { base: structName + "Key", module: hint.module });
    const keyTy = keyT.base === "String" || keyT.item ? renderUnboxed(keyT) : "String";
    return {
      base: `BTreeMap<${keyTy}, ${renderUnboxed(rest!)}>`,
      nullable: "none",
      info: { kinds: new Set(["object"]) },
    };
  }
  addItem({
    kind: "struct",
    name: structName,
    module: hint.module,
    doc: describe(ast),
    fields,
    rest,
  });
  itemInfo.set(structName, result.info);
  return result;
}

function fieldOf(wire: string, type: A, parent: Hint): Field {
  const hint = fieldHint(parent, wire);
  const fin = finalNode(type);
  const keyOptional = AST.isOptional(fin);
  const doc = keyDescription(type);
  // withDecodingDefault & friends: the wire key is optional but the decoded one is not.
  if (type.encoding && keyOptional && !AST.isOptional(type) && fin._tag !== "Unknown") {
    let inner = fin;
    if (inner._tag === "Union") {
      const kept = any(inner).types.filter((t: A) => t._tag !== "Undefined");
      if (kept.length === 1) inner = kept[0];
    }
    const ty = ref(AST.replaceContext(inner, undefined), hint);
    const dflt = computeDefault(wire, type);
    if (dflt.ok) {
      return {
        wire,
        rust: "",
        ty,
        keyOptional: false,
        defaultJson: dflt.value,
        omitOnEncode: dflt.omit,
        doc,
      };
    }
    deviations.push(
      `${parent.base}.${wire}: decoding default could not be computed; modelled as optional`,
    );
    return {
      wire,
      rust: "",
      ty: { ...ty, nullable: ty.nullable === "none" ? "undef" : ty.nullable },
      keyOptional: true,
      doc,
    };
  }
  const ty = ref(type, hint);
  // `OmittedWhenNull`: null is never on the wire (absent ⇔ null), so one `Option` level
  const omittedWhenNull = fin._tag === "Unknown" && keyOptional && !AST.isOptional(type);
  return { wire, rust: "", ty, keyOptional, doc, omittedWhenNull };
}

function computeDefault(wire: string, type: A): { ok: boolean; value?: unknown; omit?: boolean } {
  try {
    const s = Schema.make(new AST.Objects([new AST.PropertySignature(wire, type)], [])) as any;
    const codec = Schema.toCodecJson(s) as any;
    const decoded = Schema.decodeUnknownSync(codec)({});
    const encoded = Schema.encodeSync(codec)(decoded) as Record<string, unknown>;
    if (!(wire in encoded)) return { ok: true, value: null, omit: true };
    return { ok: true, value: encoded[wire] };
  } catch {
    return { ok: false };
  }
}

function mergeInfo(infos: Info[]): Info {
  const kinds = new Set<JsonKind>();
  for (const i of infos) for (const k of i.kinds) kinds.add(k);
  const out: Info = { kinds };
  if (infos.every((i) => i.literals)) out.literals = infos.flatMap((i) => i.literals!);
  if (kinds.size === 1 && kinds.has("object") && infos.every((i) => i.litFields)) {
    // keys present in every member, with the union of the values
    const keys = [...infos[0]!.litFields!.keys()].filter((k) =>
      infos.every((i) => i.litFields!.has(k)),
    );
    out.litFields = new Map(keys.map((k) => [k, infos.flatMap((i) => i.litFields!.get(k)!)]));
  }
  return out;
}

function flattenUnion(ast: A): A[] {
  const out: A[] = [];
  for (const t of any(ast).types as A[]) {
    if (t._tag === "Union" && !t.encoding && !lookupName(t)) out.push(...flattenUnion(t));
    else out.push(t);
  }
  return out;
}

const sharedUnions = new Map<string, TRef>();
function shapeUnion(ast: A, name: string | undefined, hint: Hint): TRef {
  const all = flattenUnion(ast);
  let nullable: Nullability = "none";
  const members: A[] = [];
  for (const m of all) {
    const fin = m.encoding ? null : m;
    if (fin && fin._tag === "Null") nullable = "null";
    else if (fin && (fin._tag === "Undefined" || fin._tag === "Void"))
      nullable = nullable === "null" ? "null" : "undef";
    else if (fin && fin._tag === "Never") continue;
    else members.push(m);
  }
  if (members.length === 0) return prim("()", ["null"]);
  if (members.length === 1) {
    const t = ref(members[0]!, hint);
    // `serde_json::Value` already holds null (Unknown matches null before Undefined does)
    if (nullable === "none" || t.base === "serde_json::Value") return t;
    if (t.nullable === "null" || t.baseIsOption)
      return { ...t, nullable: t.nullable === "none" ? nullable : t.nullable };
    return { ...t, nullable: t.nullable === "null" ? "null" : nullable, item: undefined };
  }
  // all string literals → a plain enum
  const memberRefs: TRef[] = [];
  const literalOnly = members.every(
    (m) => !m.encoding && m._tag === "Literal" && typeof any(m).literal === "string",
  );
  if (literalOnly) {
    const enumName = name ?? freshName(hint.base);
    const used = new Set<string>();
    const variants = members.map((m) => {
      const wire = any(m).literal as string;
      let rust = pascal(wire) || "Empty";
      while (used.has(rust)) rust += "_";
      used.add(rust);
      return { rust, wire };
    });
    addItem({ kind: "strEnum", name: enumName, module: hint.module, doc: describe(ast), variants });
    const info: Info = { kinds: new Set(["string"]), literals: variants.map((v) => v.wire) };
    itemInfo.set(enumName, info);
    return { base: enumName, nullable, info, item: enumName };
  }
  const memberNames = members.map((m) => lookupName(m));
  const allNamed = memberNames.every((n) => n !== undefined);
  // anonymous unions of the same named members (`Schema.Union([E1, E2])` in many RPCs) share one type
  const sharedKey = !name && allNamed ? memberNames.join("|") : undefined;
  if (sharedKey && sharedUnions.has(sharedKey))
    return { ...sharedUnions.get(sharedKey)!, nullable };
  const unionName =
    name ?? freshName(members.length <= 3 && allNamed ? memberNames.join("Or") : hint.base);
  const result: TRef = {
    base: unionName,
    nullable,
    info: { kinds: new Set(["any"]) },
    item: unionName,
  };
  if (sharedKey) sharedUnions.set(sharedKey, result);
  const out: Member[] = [];
  const usedVariants = new Set<string>();
  const variantName = (base: string) => {
    let v = base || "Variant";
    if (!/^[A-Za-z]/.test(v)) v = "V" + v;
    while (usedVariants.has(v)) v += "_";
    usedVariants.add(v);
    return v;
  };
  const astTag = astTagKey(members.filter((m) => !(!m.encoding && m._tag === "Literal")));
  members.forEach((m, i) => {
    if (!m.encoding && m._tag === "Literal") {
      const lit = any(m).literal;
      const value = typeof lit === "bigint" ? String(lit) : lit;
      const v =
        typeof value === "string"
          ? pascal(value) || "Empty"
          : typeof value === "boolean"
            ? value
              ? "True"
              : "False"
            : "N" + String(value).replace("-", "Neg").replace(".", "_");
      out.push({
        variant: variantName(v),
        literal: value,
        info: { kinds: new Set([jsonKindOf(value)]), literals: [value] },
      });
      return;
    }
    const tagHint = (astTag ? astLitFields(m)?.get(astTag)?.[0] : undefined) ?? memberTagHint(m);
    const t = ref(m, {
      base: unionName + (tagHint ? pascal(tagHint) : "Variant" + (i + 1)),
      module: hint.module,
    });
    memberRefs.push(t);
    let v = t.item && !t.base.startsWith("Box<") ? t.item : undefined;
    if (!v) v = tagHint ? pascal(tagHint) : kindVariant(t);
    if (
      t.item &&
      v.startsWith(unionName) &&
      v.length > unionName.length &&
      /^[A-Z]/.test(v.slice(unionName.length))
    ) {
      v = v.slice(unionName.length);
    }
    out.push({
      variant: variantName(v),
      ty: t,
      info:
        t.nullable !== "none" ? { ...t.info, kinds: new Set([...t.info.kinds, "null"]) } : t.info,
    });
  });
  // discriminator
  let tagKey: string | undefined;
  if (
    out.every(
      (m) => m.ty && m.info.kinds.size === 1 && m.info.kinds.has("object") && m.info.litFields,
    )
  ) {
    const candidates = new Set<string>(out[0]!.info.litFields!.keys());
    for (const m of out)
      for (const k of [...candidates]) if (!m.info.litFields!.has(k)) candidates.delete(k);
    const ordered = [...candidates].sort(
      (a, b) => keyPriority(a) - keyPriority(b) || a.localeCompare(b),
    );
    for (const k of ordered) {
      const seen = new Set<string>();
      let ok = true;
      for (const m of out) {
        for (const v of m.info.litFields!.get(k)!) {
          if (typeof v !== "string" || seen.has(v)) ok = false;
          seen.add(String(v));
        }
      }
      if (ok) {
        tagKey = k;
        break;
      }
    }
  }
  addItem({
    kind: "union",
    name: unionName,
    module: hint.module,
    doc: describe(ast),
    members: out,
    tagKey,
  });
  result.info = mergeInfo(out.map((m) => m.info));
  itemInfo.set(unionName, result.info);
  return result;
}
function keyPriority(k: string): number {
  return k === "_tag" ? 0 : k === "type" ? 1 : k === "kind" ? 2 : 3;
}
function kindVariant(t: TRef): string {
  const k = [...t.info.kinds][0];
  switch (k) {
    case "string":
      return "String";
    case "number":
      return "Number";
    case "bool":
      return "Bool";
    case "array":
      return "Array";
    case "object":
      return "Object";
    default:
      return "Value";
  }
}
/** Required string-literal properties of a member, read from its AST (before items exist). */
function astLitFields(m: A): Map<string, string[]> | undefined {
  let n = m;
  if (n._tag !== "Objects") n = finalNode(n);
  if (n._tag !== "Objects") return undefined;
  const out = new Map<string, string[]>();
  for (const p of any(n).propertySignatures as Array<{ name: PropertyKey; type: A }>) {
    const t = p.type;
    if (AST.isOptional(t) || t.encoding) continue;
    if (t._tag === "Literal" && typeof any(t).literal === "string")
      out.set(String(p.name), [any(t).literal]);
    else if (
      t._tag === "Union" &&
      any(t).types.every((x: any) => x._tag === "Literal" && typeof x.literal === "string")
    ) {
      out.set(
        String(p.name),
        any(t).types.map((x: any) => x.literal),
      );
    }
  }
  return out;
}
function astTagKey(members: A[]): string | undefined {
  const lits = members.map(astLitFields);
  if (lits.length < 2 || lits.some((l) => !l)) return undefined;
  const keys = [...lits[0]!.keys()].filter((k) => lits.every((l) => l!.has(k)));
  keys.sort((a, b) => keyPriority(a) - keyPriority(b) || a.localeCompare(b));
  return keys.find((k) => {
    const seen = new Set<string>();
    for (const l of lits)
      for (const v of l!.get(k)!) {
        if (seen.has(v)) return false;
        seen.add(v);
      }
    return true;
  });
}
function memberTagHint(m: A): string | undefined {
  let n = m;
  while (n.encoding) n = n.encoding[n.encoding.length - 1]!.to;
  if (m._tag === "Objects") n = m;
  if (n._tag !== "Objects") return undefined;
  for (const key of ["_tag", "type", "kind"]) {
    const ps = any(n).propertySignatures.find((p: any) => p.name === key);
    if (ps && ps.type._tag === "Literal" && typeof ps.type.literal === "string")
      return ps.type.literal;
  }
  return undefined;
}

// ---------------------------------------------------------------------------------------------
// Walk every export
// ---------------------------------------------------------------------------------------------

const exportRefs = new Map<string, TRef>();
for (const [name] of allExports) {
  if (canonicalOfExport.get(name) !== name) continue;
  const schema = (Contracts as any)[name] as Schema.Top;
  exportRefs.set(name, namedRef(schema.ast, name));
}

// ---------------------------------------------------------------------------------------------
// RPC table
// ---------------------------------------------------------------------------------------------

const scopeSource = Fs.readFileSync(
  Path.join(CODE_ROOT, "apps/server/src/auth/RpcAuthorization.ts"),
  "utf8",
);
const scopeTable = new Map<string, string>();
for (const m of scopeSource.matchAll(
  /\[(ORCHESTRATION_WS_METHODS|WS_METHODS)\.(\w+)\]:\s*(\w+),/g,
)) {
  const methods = (Contracts as any)[m[1]!] as Record<string, string>;
  const tag = methods[m[2]!];
  const scope = (Contracts as any)[m[3]!];
  if (typeof tag !== "string" || typeof scope !== "string")
    throw new Error(`cannot resolve scope entry ${m[0]}`);
  scopeTable.set(tag, scope);
}

interface RpcEntry {
  tag: string;
  rust: string;
  stream: boolean;
  scope: string;
  payload: TRef;
  success: TRef;
  error: TRef;
  schemas: { payload: Schema.Top; success: Schema.Top; error: Schema.Top };
}
const rpcs: RpcEntry[] = [];
{
  const usedRust = new Set<string>();
  for (const [tag, rpc] of (Contracts.WsRpcGroup as any).requests as Map<string, any>) {
    let rust = pascal(tag);
    while (usedRust.has(rust)) rust += "_";
    usedRust.add(rust);
    const streamSchemas = RpcSchema.getStreamSchemas(rpc.successSchema);
    const stream = streamSchemas._tag === "Some";
    const successSchema: Schema.Top = stream
      ? (streamSchemas as any).value.success
      : rpc.successSchema;
    const errorParts: Schema.Top[] = [rpc.errorSchema];
    if (stream) errorParts.push((streamSchemas as any).value.error);
    const errors = errorParts.filter((s) => s.ast._tag !== "Never");
    const errorSchema: Schema.Top =
      errors.length === 0 ? Schema.Never : errors.length === 1 ? errors[0]! : Schema.Union(errors);
    const scope = scopeTable.get(tag);
    if (!scope) throw new Error(`no scope for RPC ${tag}`);
    const mk = (s: Schema.Top, suffix: string) =>
      ref(s.ast, { base: rust + suffix, module: "rpc" });
    rpcs.push({
      tag,
      rust,
      stream,
      scope,
      payload: mk(rpc.payloadSchema, "Payload"),
      success: mk(successSchema, stream ? "Item" : "Success"),
      error: mk(errorSchema, "Error"),
      schemas: { payload: rpc.payloadSchema, success: successSchema, error: errorSchema },
    });
  }
}
if (rpcs.length !== scopeTable.size)
  throw new Error(`scope table has ${scopeTable.size} entries for ${rpcs.length} RPCs`);

// ---------------------------------------------------------------------------------------------
// HTTP table
// ---------------------------------------------------------------------------------------------

interface HttpEntry {
  group: string;
  name: string;
  rust: string;
  method: string;
  path: string;
  authenticated: boolean;
  payloadEncoding?: string;
  params?: TRef;
  headers?: TRef;
  payload?: TRef;
  success: Array<{ status: number; ty: TRef; schema: Schema.Top }>;
  errors: Array<{ status: number; ty: TRef; schema: Schema.Top }>;
  error: TRef;
  schemas: { params?: Schema.Top; payload?: Schema.Top; errorUnion: Schema.Top };
}
const https: HttpEntry[] = [];
function unwrapHttpSchema(s: any): Schema.Top {
  return s && s.schema && Schema.isSchema(s.schema) && !s.fields ? s.schema : s;
}
HttpApi.reflect(Contracts.EnvironmentHttpApi as any, {
  onGroup() {},
  onEndpoint(o: any) {
    const e = o.endpoint;
    const group = o.group.identifier as string;
    const rust = pascal(group) + pascal(e.identifier);
    const module = "environmentHttp";
    const mk = (s: any, suffix: string) =>
      ref(unwrapHttpSchema(s).ast, { base: rust + suffix, module });
    const payloadEntries = [...e.payload.entries()] as Array<
      [string, { encoding: any; schemas: any[] }]
    >;
    let payload: TRef | undefined;
    let payloadSchema: Schema.Top | undefined;
    let payloadEncoding: string | undefined;
    if (payloadEntries.length > 0) {
      const [, p] = payloadEntries[0]!;
      payloadSchema =
        p.schemas.length === 1
          ? unwrapHttpSchema(p.schemas[0])
          : (Schema.Union(p.schemas.map(unwrapHttpSchema)) as any);
      payloadEncoding = e.method === "GET" ? "Query" : p.encoding._tag;
      payload = ref(payloadSchema!.ast, {
        base: rust + (e.method === "GET" ? "Query" : "Payload"),
        module,
      });
    }
    const success = [...o.successes.entries()].flatMap(([status, list]: any) =>
      list.map((s: any) => ({ status, ty: mk(s, "Success"), schema: unwrapHttpSchema(s) })),
    );
    const errors = [...o.errors.entries()].flatMap(([status, list]: any) =>
      list.map((s: any) => ({ status, ty: mk(s, "Error"), schema: unwrapHttpSchema(s) })),
    );
    const errorSchemas = errors.map((x: any) => x.schema);
    const errorUnion: Schema.Top =
      errorSchemas.length === 0
        ? Schema.Never
        : errorSchemas.length === 1
          ? errorSchemas[0]
          : (Schema.Union(errorSchemas) as any);
    https.push({
      group,
      name: e.identifier,
      rust,
      method: e.method,
      path: e.path,
      authenticated: e.middlewares.size > 0,
      payloadEncoding,
      params: e.params ? mk(e.params, "Params") : undefined,
      headers: e.headers ? mk(e.headers, "Headers") : undefined,
      payload,
      success,
      errors,
      error: ref(errorUnion.ast, { base: rust + "HttpError", module }),
      schemas: {
        params: e.params ? unwrapHttpSchema(e.params) : undefined,
        payload: payloadSchema,
        errorUnion,
      },
    });
  },
});

// ---------------------------------------------------------------------------------------------
// Emit Rust
// ---------------------------------------------------------------------------------------------

const HEADER = "// @generated by code/scripts/gen-rust-contracts.ts. Do not edit by hand.\n";

function deriveFor(item: Item): string {
  switch (item.kind) {
    case "struct":
      return "#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]";
    case "strEnum":
      return "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]";
    case "union":
      return "#[derive(Debug, Clone, PartialEq)]";
    default:
      return "";
  }
}

function emitStruct(item: Extract<Item, { kind: "struct" }>, defaults: string[]): string {
  let s = docLines(item.doc) + deriveFor(item) + "\n";
  if (item.fields.length === 0 && !item.rest) return s + `pub struct ${item.name} {}\n`;
  s += `pub struct ${item.name} {\n`;
  for (const f of item.fields) {
    const attrs: string[] = [];
    const rawName = f.rust.startsWith("r#") ? f.rust.slice(2) : f.rust;
    if (rawName !== f.wire) attrs.push(`rename = ${rustStr(f.wire)}`);
    let ty: string;
    if (f.defaultJson !== undefined) {
      ty = render(f.ty);
      const fn = `default_${snake(item.name).replace(/^r#/, "")}_${rawName}`;
      defaults.push(
        `pub(crate) fn ${fn}() -> ${ty} {\n    crate::prim::json_default(${rawStr(JSON.stringify(f.defaultJson))})\n}\n`,
      );
      defaultFns.push({ module: item.module, fn });
      attrs.push(`default = ${rustStr(fn)}`);
      if (f.ty.nullable === "none" && !f.ty.baseIsOption && f.ty.base !== "serde_json::Value") {
        // the wire key is `Schema.optional`: a `null` decodes as missing, hence the default
        const de = `de_${fn.slice("default_".length)}`;
        defaults.push(
          `pub(crate) fn ${de}<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<${ty}, D::Error> {\n    crate::prim::null_as_default(deserializer, ${fn})\n}\n`,
        );
        attrs.push(`deserialize_with = ${rustStr(de)}`);
      }
      if (f.omitOnEncode) attrs.push("skip_serializing");
    } else if (f.ty.lenient) {
      const double = f.keyOptional && f.ty.nullable === "null" && !f.omittedWhenNull;
      ty = double
        ? `Option<${render(f.ty)}>`
        : render(f.ty.nullable === "none" ? { ...f.ty, nullable: "undef" } : f.ty);
      attrs.push("default");
      if (f.keyOptional) attrs.push(`skip_serializing_if = "Option::is_none"`);
      attrs.push(
        `deserialize_with = ${rustStr(double ? "crate::prim::lenient_double_option" : "crate::prim::lenient_option")}`,
      );
    } else if (f.keyOptional) {
      if (f.ty.nullable === "null") {
        ty = `Option<${render(f.ty)}>`;
        attrs.push(
          `default`,
          `skip_serializing_if = "Option::is_none"`,
          `with = "crate::prim::double_option"`,
        );
      } else {
        ty = f.ty.nullable === "undef" ? render(f.ty) : `Option<${render(f.ty)}>`;
        attrs.push(`default`, `skip_serializing_if = "Option::is_none"`);
        // an unknown value may be `null`: keep it (`Some(Value::Null)`), absent is `None`
        if (f.ty.base === "serde_json::Value")
          attrs.push(`deserialize_with = "crate::prim::value_present"`);
      }
    } else {
      ty = render(f.ty);
      // `NullOr(X)`: the key is required (serde would read a missing `Option` field as `None`,
      // which would also make untagged unions pick a different member than TS)
      if (f.ty.nullable !== "none" && !f.ty.baseIsOption)
        attrs.push(`deserialize_with = "crate::prim::nullable"`);
    }
    s += docLines(f.doc, "    ");
    if (attrs.length) s += `    #[serde(${attrs.join(", ")})]\n`;
    s += `    pub ${f.rust}: ${ty},\n`;
  }
  if (item.rest) {
    s += `    /// Keys matched by the index signature.\n    #[serde(flatten)]\n    pub rest: BTreeMap<String, ${render(item.rest)}>,\n`;
  }
  return s + "}\n";
}

function emitStrEnum(item: Extract<Item, { kind: "strEnum" }>): string {
  let s = docLines(item.doc) + deriveFor(item) + "\n" + `pub enum ${item.name} {\n`;
  for (const v of item.variants) s += `    #[serde(rename = ${rustStr(v.wire)})]\n    ${v.rust},\n`;
  s += "}\n";
  s += `impl ${item.name} {\n    /// The wire string.\n    pub const fn as_str(self) -> &'static str {\n        match self {\n`;
  for (const v of item.variants) s += `            Self::${v.rust} => ${rustStr(v.wire)},\n`;
  s += `        }\n    }\n    /// Every member, in declaration order.\n    pub const ALL: &'static [Self] = &[${item.variants.map((v) => `Self::${v.rust}`).join(", ")}];\n}\n`;
  return s;
}

function literalExpr(v: string | number | boolean): string {
  return typeof v === "string"
    ? rustStr(v)
    : typeof v === "number"
      ? Number.isInteger(v)
        ? `${v}`
        : `${v}`
      : `${v}`;
}
function literalPattern(key: string | undefined, values: Array<string | number | boolean>): string {
  const target = key === undefined ? "v" : `v.get(${rustStr(key)})`;
  const strs = values.filter((x) => typeof x === "string") as string[];
  const nums = values.filter((x) => typeof x === "number") as number[];
  const bools = values.filter((x) => typeof x === "boolean") as boolean[];
  const parts: string[] = [];
  const get =
    key === undefined
      ? (m: string) => `${target}.${m}()`
      : (m: string) => `${target}.and_then(serde_json::Value::${m})`;
  if (strs.length) parts.push(`matches!(${get("as_str")}, Some(${strs.map(rustStr).join(" | ")}))`);
  for (const n of nums)
    parts.push(`${get("as_f64")} == Some(${Number.isInteger(n) ? n + ".0" : n})`);
  for (const b of bools) parts.push(`${get("as_bool")} == Some(${b})`);
  return parts.length === 1 ? parts[0]! : `(${parts.join(" || ")})`;
}
function kindCheck(info: Info): string | undefined {
  if (info.kinds.has("any") || info.kinds.size === 0) return undefined;
  const checks = [...info.kinds].map((k) => {
    switch (k) {
      case "string":
        return "v.is_string()";
      case "number":
        return "v.is_number()";
      case "bool":
        return "v.is_boolean()";
      case "object":
        return "v.is_object()";
      case "array":
        return "v.is_array()";
      case "null":
        return "v.is_null()";
    }
  });
  return checks.length === 1 ? checks[0] : `(${checks.join(" || ")})`;
}

function emitUnion(item: Extract<Item, { kind: "union" }>): string {
  let s = docLines(item.doc) + deriveFor(item) + "\n" + `pub enum ${item.name} {\n`;
  for (const m of item.members) {
    if (m.ty)
      s += `    ${m.variant}(${renderUnboxed(m.ty) === m.ty.base ? m.ty.base : render(m.ty)}),\n`;
    else s += `    /// \`${JSON.stringify(m.literal)}\`\n    ${m.variant},\n`;
  }
  s += "}\n";
  // Serialize
  s += `impl Serialize for ${item.name} {\n    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {\n        match self {\n`;
  for (const m of item.members) {
    if (m.ty) s += `            Self::${m.variant}(value) => value.serialize(serializer),\n`;
    else {
      const v = m.literal!;
      const call =
        typeof v === "string"
          ? `serializer.serialize_str(${rustStr(v)})`
          : typeof v === "boolean"
            ? `serializer.serialize_bool(${v})`
            : Number.isInteger(v)
              ? `serializer.serialize_i64(${v})`
              : `serializer.serialize_f64(${v})`;
      s += `            Self::${m.variant} => ${call},\n`;
    }
  }
  s += `        }\n    }\n}\n`;
  // Deserialize
  s += `impl<'de> Deserialize<'de> for ${item.name} {\n    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {\n        let v = serde_json::Value::deserialize(deserializer)?;\n`;
  if (item.tagKey) {
    s += `        let tag = v.get(${rustStr(item.tagKey)}).and_then(serde_json::Value::as_str).map(str::to_owned);\n        match tag.as_deref() {\n`;
    for (const m of item.members) {
      const values = m.info.litFields!.get(item.tagKey)!.map((x) => rustStr(String(x)));
      s += `            Some(${values.join(" | ")}) => crate::prim::from_value(v).map(Self::${m.variant}),\n`;
    }
    s += `            other => Err(crate::prim::unknown_tag(${rustStr(item.name)}, ${rustStr(item.tagKey)}, other)),\n        }\n        .map_err(serde::de::Error::custom)\n    }\n}\n`;
    return s;
  }
  for (const m of item.members) {
    if (!m.ty) {
      s += `        if ${literalPattern(undefined, [m.literal!])} {\n            return Ok(Self::${m.variant});\n        }\n`;
      continue;
    }
    const conds: string[] = [];
    const k = kindCheck(m.info);
    if (k) conds.push(k);
    if (m.info.literals && !m.info.kinds.has("null"))
      conds.push(literalPattern(undefined, m.info.literals));
    if (m.info.litFields)
      for (const [key, values] of m.info.litFields) conds.push(literalPattern(key, values));
    const cond = conds.length ? conds.join(" && ") : "true";
    s += `        if ${cond} {\n            if let Ok(value) = crate::prim::from_value(v.clone()) {\n                return Ok(Self::${m.variant}(value));\n            }\n        }\n`;
  }
  s += `        Err(serde::de::Error::custom(crate::prim::no_member(${rustStr(item.name)}, &v)))\n    }\n}\n`;
  return s;
}

function emitItem(item: Item, defaults: string[]): string {
  switch (item.kind) {
    case "struct":
      return emitStruct(item, defaults);
    case "strEnum":
      return emitStrEnum(item);
    case "union":
      return emitUnion(item);
    case "newtype":
      return (
        docLines(item.doc) +
        `crate::prim::${item.inner === "String" ? "string_newtype" : "int_newtype"}!(${item.name});\n`
      );
    case "alias":
      return docLines(item.doc) + `pub type ${item.name} = ${item.target};\n`;
  }
}

// export aliases for names that were not canonical
const aliasExports: Array<{ name: string; module: string; target: string }> = [];
for (const [name, schema] of allExports) {
  if (canonicalOfExport.get(name) === name) continue;
  const canonical = canonicalOfExport.get(name);
  const target = canonical
    ? canonical
    : render(
        shapeOf(schema.ast, undefined, { base: name, module: exportModule.get(name) ?? "misc" }),
      );
  if (takenNames.has(name) && items.has(name)) continue;
  takenNames.add(name);
  aliasExports.push({ name, module: exportModule.get(name) ?? "misc", target });
}

const defaultFns: Array<{ module: string; fn: string }> = [];
const modules = new Map<string, string[]>();
const moduleDefaults = new Map<string, string[]>();
for (const name of itemOrder) {
  const item = items.get(name)!;
  const mod = snake(item.module).replace(/^r#/, "");
  if (!modules.has(mod)) modules.set(mod, []);
  if (!moduleDefaults.has(mod)) moduleDefaults.set(mod, []);
  modules.get(mod)!.push(emitItem(item, moduleDefaults.get(mod)!));
}
for (const a of aliasExports) {
  const mod = snake(a.module).replace(/^r#/, "");
  if (!modules.has(mod)) modules.set(mod, []);
  modules
    .get(mod)!
    .push(
      `/// Same schema as \`${a.target}\` in the TypeScript contracts.\npub type ${a.name} = ${a.target};\n`,
    );
}

Fs.rmSync(GEN_DIR, { recursive: true, force: true });
Fs.mkdirSync(GEN_DIR, { recursive: true });
const moduleNames = [...modules.keys()].sort();
const writeGen = (file: string, body: string) => Fs.writeFileSync(Path.join(GEN_DIR, file), body);

for (const mod of moduleNames) {
  let body =
    HEADER +
    `//! Types of \`packages/contracts/src/${[...moduleFiles, "rpc", "http"].find((f) => snake(f).replace(/^r#/, "") === mod) ?? mod}.ts\`.\n\n#[allow(unused_imports)]\nuse super::*;\n\n`;
  body += modules.get(mod)!.join("\n");
  const defs = moduleDefaults.get(mod) ?? [];
  if (defs.length)
    body +=
      "\n// Decoding defaults (`withDecodingDefault`): the value a missing key decodes to.\n\n" +
      defs.join("\n");
  writeGen(`${mod}.rs`, body);
}

// literal unit types
{
  let body =
    HEADER +
    "//! Single-literal unit types: they serialize to their literal and only deserialize from it.\n\n";
  const sorted = [...lits.values()].sort((a, b) => a.name.localeCompare(b.name));
  for (const l of sorted) {
    const mac =
      typeof l.value === "string"
        ? "lit_str"
        : typeof l.value === "boolean"
          ? "lit_bool"
          : Number.isInteger(l.value)
            ? "lit_int"
            : "lit_f64";
    body += `crate::prim::${mac}!(${l.name}, ${literalExpr(l.value)});\n`;
  }
  writeGen("lits.rs", body);
}

// rpc table
{
  const scopeEnum = namedRef((Contracts as any).AuthEnvironmentScope.ast, "AuthEnvironmentScope");
  const scopeItem = items.get(scopeEnum.base);
  if (!scopeItem || scopeItem.kind !== "strEnum")
    throw new Error("AuthEnvironmentScope must be a string enum");
  const scopeVariant = (s: string) =>
    `AuthEnvironmentScope::${scopeItem.variants.find((v) => v.wire === s)!.rust}`;
  let body =
    HEADER +
    `//! The ${rpcs.length} WebSocket RPC methods of \`WsRpcGroup\` (\`packages/contracts/src/rpc.ts\`) with the
//! scope each one requires (\`apps/server/src/auth/RpcAuthorization.ts\`).
//!
//! Use the marker types in [\`methods\`] with [\`RpcMethod\`] for type-checked handlers, [\`Rpc\`] to
//! dispatch on the wire tag, and [\`crate::zc_for_each_rpc\`] to generate per-method code.
//!
//! One scope depends on the payload and is not in this table: \`device.list\` needs
//! \`orchestration:operate\` when \`retryHostId\` or \`updateTool\` is set.

use super::*;

/// Unary (one \`Exit\`) or stream (\`Chunk\`s then \`Exit\` with a \`null\` value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RpcKind {
    Unary,
    Stream,
}

/// Static description of one RPC method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodSpec {
    pub rpc: Rpc,
    /// The wire \`tag\` of the \`Request\` envelope.
    pub tag: &'static str,
    pub kind: RpcKind,
    /// Scope the session must hold before the handler runs.
    pub scope: AuthEnvironmentScope,
    /// Rust type names of the payload, the success value (the item type for streams) and the error.
    pub payload: &'static str,
    pub success: &'static str,
    pub error: &'static str,
}

/// Compile-time description of one RPC method.
pub trait RpcMethod {
    const RPC: Rpc;
    const TAG: &'static str;
    const KIND: RpcKind;
    const SCOPE: AuthEnvironmentScope;
    /// The request payload.
    type Payload: Serialize + serde::de::DeserializeOwned + Send + 'static;
    /// The success value of a unary call, or one stream item.
    type Success: Serialize + serde::de::DeserializeOwned + Send + 'static;
    /// The typed failure (\`Exit\` cause \`Fail\`). \`Never\` when the method cannot fail.
    type Error: Serialize + serde::de::DeserializeOwned + Send + 'static;
}

/// Every RPC method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Rpc {
${rpcs.map((r) => `    /// \`${r.tag}\`\n    ${r.rust},`).join("\n")}
}

impl Rpc {
    /// Every method, in \`WsRpcGroup\` order.
    pub const ALL: [Rpc; ${rpcs.length}] = [${rpcs.map((r) => `Rpc::${r.rust}`).join(", ")}];

    /// The wire tag.
    pub const fn tag(self) -> &'static str {
        match self {
${rpcs.map((r) => `            Rpc::${r.rust} => ${rustStr(r.tag)},`).join("\n")}
        }
    }

    /// Looks a method up by its wire tag.
    pub fn from_tag(tag: &str) -> Option<Rpc> {
        Some(match tag {
${rpcs.map((r) => `            ${rustStr(r.tag)} => Rpc::${r.rust},`).join("\n")}
            _ => return None,
        })
    }

    /// The static description of this method.
    pub fn spec(self) -> &'static MethodSpec {
        &METHODS[self as usize]
    }
}

/// Every method, in \`WsRpcGroup\` order (\`METHODS[rpc as usize].rpc == rpc\`).
pub static METHODS: [MethodSpec; ${rpcs.length}] = [
${rpcs
  .map(
    (r) =>
      `    MethodSpec { rpc: Rpc::${r.rust}, tag: ${rustStr(r.tag)}, kind: RpcKind::${r.stream ? "Stream" : "Unary"}, scope: ${scopeVariant(r.scope)}, payload: ${rustStr(render(r.payload))}, success: ${rustStr(render(r.success))}, error: ${rustStr(render(r.error))} },`,
  )
  .join("\n")}
];

/// Marker types, one per method, implementing [\`RpcMethod\`].
pub mod methods {
${rpcs
  .map(
    (r) => `
    /// \`${r.tag}\` (${r.stream ? "stream" : "unary"}, scope \`${r.scope}\`).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub struct ${r.rust};`,
  )
  .join("\n")}
}
${rpcs
  .map(
    (r) => `
impl RpcMethod for methods::${r.rust} {
    const RPC: Rpc = Rpc::${r.rust};
    const TAG: &'static str = ${rustStr(r.tag)};
    const KIND: RpcKind = RpcKind::${r.stream ? "Stream" : "Unary"};
    const SCOPE: AuthEnvironmentScope = ${scopeVariant(r.scope)};
    type Payload = ${render(r.payload)};
    type Success = ${render(r.success)};
    type Error = ${render(r.error)};
}`,
  )
  .join("\n")}

/// Calls \`$mac!\` once with the list of every method:
/// \`(MarkerType, "wire.tag", Unary|Stream)\`, separated by commas.
///
/// \`\`\`ignore
/// macro_rules! count { ($(($t:ident, $tag:literal, $kind:ident)),* $(,)?) => { [$($tag),*].len() } }
/// assert_eq!(zc_contracts::zc_for_each_rpc!(count), 148);
/// \`\`\`
#[macro_export]
macro_rules! zc_for_each_rpc {
    ($mac:ident) => {
        $mac! {
${rpcs.map((r) => `            (${r.rust}, ${rustStr(r.tag)}, ${r.stream ? "Stream" : "Unary"}),`).join("\n")}
        }
    };
}
`;
  writeGen("rpc_methods.rs", body);
}

// http table
{
  let body =
    HEADER +
    `//! The ${https.length} typed endpoints of \`EnvironmentHttpApi\` (\`packages/contracts/src/environmentHttp.ts\`).
//!
//! Bodies are JSON encoded like RPC payloads, except \`POST /oauth/token\` (form-urlencoded) and the
//! GET query of \`threadSnapshot\` (query string). Errors are tagged JSON bodies with the listed status.

use super::*;

/// How the request body (or query) is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PayloadEncoding {
    None,
    Json,
    FormUrlEncoded,
    /// GET: the payload struct is the query string (every field encodes to a string).
    Query,
}

/// Static description of one endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndpointSpec {
    pub group: &'static str,
    pub name: &'static str,
    pub method: &'static str,
    /// Path pattern with \`:param\` segments.
    pub path: &'static str,
    /// Behind the \`EnvironmentAuthenticatedAuth\` middleware (session cookie or bearer token).
    pub authenticated: bool,
    pub payload_encoding: PayloadEncoding,
    /// Rust type names.
    pub params: Option<&'static str>,
    pub payload: Option<&'static str>,
    /// \`(status, Rust type)\` of each success body.
    pub success: &'static [(u16, &'static str)],
    /// \`(status, Rust type)\` of each error body.
    pub errors: &'static [(u16, &'static str)],
}

/// Compile-time description of one endpoint.
pub trait HttpEndpoint {
    const SPEC: &'static EndpointSpec;
    /// Path parameters (\`()\` when none).
    type Params: Serialize + serde::de::DeserializeOwned;
    /// JSON/form body or GET query (\`()\` when none).
    type Payload: Serialize + serde::de::DeserializeOwned;
    /// The 2xx body.
    type Success: Serialize + serde::de::DeserializeOwned;
    /// Union of every error body; [\`EndpointSpec::errors\`] gives each one's status.
    type Error: Serialize + serde::de::DeserializeOwned;
}

/// Every endpoint, in \`EnvironmentHttpApi\` order.
pub static ENDPOINTS: [EndpointSpec; ${https.length}] = [
${https
  .map(
    (h) =>
      `    EndpointSpec { group: ${rustStr(h.group)}, name: ${rustStr(h.name)}, method: ${rustStr(h.method)}, path: ${rustStr(h.path)}, authenticated: ${h.authenticated}, payload_encoding: PayloadEncoding::${h.payloadEncoding === "Json" ? "Json" : h.payloadEncoding === "FormUrlEncoded" ? "FormUrlEncoded" : h.payloadEncoding === "Query" ? "Query" : "None"}, params: ${h.params ? `Some(${rustStr(render(h.params))})` : "None"}, payload: ${h.payload ? `Some(${rustStr(render(h.payload))})` : "None"}, success: &[${h.success.map((x) => `(${x.status}, ${rustStr(render(x.ty))})`).join(", ")}], errors: &[${h.errors.map((x) => `(${x.status}, ${rustStr(render(x.ty))})`).join(", ")}] },`,
  )
  .join("\n")}
];

/// Marker types, one per endpoint, implementing [\`HttpEndpoint\`].
pub mod endpoints {
${https
  .map(
    (h) => `
    /// \`${h.method} ${h.path}\` (group \`${h.group}\`, endpoint \`${h.name}\`).
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub struct ${h.rust};`,
  )
  .join("\n")}
}
${https
  .map(
    (h, i) => `
impl HttpEndpoint for endpoints::${h.rust} {
    const SPEC: &'static EndpointSpec = &ENDPOINTS[${i}];
    type Params = ${h.params ? render(h.params) : "()"};
    type Payload = ${h.payload ? render(h.payload) : "()"};
    type Success = ${render(h.success[0]!.ty)};
    type Error = ${render(h.error)};
}`,
  )
  .join("\n")}
`;
  writeGen("http_endpoints.rs", body);
}

// registry (round-trip by schema id, for tests and the verification harness)
interface Target {
  id: string;
  rust: string;
  schema: Schema.Top;
}
const targets: Target[] = [];
for (const [name, schema] of allExports) {
  const canonical = canonicalOfExport.get(name);
  const ref = exportRefs.get(canonical ?? "");
  // a named union with a null member is the non-null enum; the value itself is `Option<..>`
  const rustName = ref ? render(ref) : aliasExports.find((a) => a.name === name) ? name : undefined;
  if (!rustName) continue;
  targets.push({ id: name, rust: rustName, schema });
}
for (const r of rpcs) {
  targets.push({ id: `rpc:${r.tag}:payload`, rust: render(r.payload), schema: r.schemas.payload });
  targets.push({ id: `rpc:${r.tag}:success`, rust: render(r.success), schema: r.schemas.success });
  targets.push({ id: `rpc:${r.tag}:error`, rust: render(r.error), schema: r.schemas.error });
}
for (const h of https) {
  if (h.params)
    targets.push({
      id: `http:${h.group}.${h.name}:params`,
      rust: render(h.params),
      schema: h.schemas.params!,
    });
  if (h.payload)
    targets.push({
      id: `http:${h.group}.${h.name}:payload`,
      rust: render(h.payload),
      schema: h.schemas.payload!,
    });
  h.success.forEach((s, i) =>
    targets.push({
      id: `http:${h.group}.${h.name}:success:${i}`,
      rust: render(s.ty),
      schema: s.schema,
    }),
  );
  targets.push({
    id: `http:${h.group}.${h.name}:error`,
    rust: render(h.error),
    schema: h.schemas.errorUnion,
  });
}
{
  let body =
    HEADER +
    `//! Round-trip by schema id: decode a wire value into the Rust type of a schema and encode it back.
//! Ids are export names of \`packages/contracts\`, \`rpc:<tag>:payload|success|error\` and
//! \`http:<group>.<endpoint>:params|payload|success:<n>|error\` — the same ids
//! \`code/scripts/contracts-oracle.ts\` understands.

use super::*;

fn rt<T: Serialize + serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<serde_json::Value, String> {
    let decoded: T = serde_json::from_value(value).map_err(|e| format!("decode: {e}"))?;
    serde_json::to_value(&decoded).map_err(|e| format!("encode: {e}"))
}

/// Decodes \`value\` as the schema \`id\` and re-encodes it. \`None\` when the id is unknown.
pub fn roundtrip(id: &str, value: serde_json::Value) -> Option<Result<serde_json::Value, String>> {
    Some(match id {
${targets.map((t) => `        ${rustStr(t.id)} => rt::<${t.rust}>(value),`).join("\n")}
        _ => return None,
    })
}

/// Every schema id \`roundtrip\` knows.
pub const IDS: &[&str] = &[
${targets.map((t) => `    ${rustStr(t.id)},`).join("\n")}
];

/// Ids whose type is \`Never\` (errors of methods that cannot fail): they have no values.
pub const NO_VALUE_IDS: &[&str] = &[
${targets
  .filter((t) => t.rust === "Never")
  .map((t) => `    ${rustStr(t.id)},`)
  .join("\n")}
];

/// Calls every decoding-default function (they parse JSON literals; this proves they are valid).
pub fn check_defaults() {
${defaultFns.map((d) => `    let _ = super::${snake(d.module).replace(/^r#/, "")}::${d.fn}();`).join("\n")}
}
`;
  writeGen("registry.rs", body);
}

// mod.rs
{
  let body =
    HEADER +
    `//! Generated serde types for \`packages/contracts\` (encoded/wire side), the RPC table and the
//! HTTP endpoint table. See docs/zenith-code/contracts.md.
#![allow(
    clippy::all,
    clippy::pedantic,
    missing_docs,
    non_camel_case_types,
    unused_imports,
    rustdoc::all
)]

pub(crate) use crate::prim::*;
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use std::collections::BTreeMap;

`;
  for (const mod of moduleNames) body += `mod ${mod};\npub use ${mod}::*;\n`;
  body += `mod lits;\npub use lits::*;\n`;
  body += `pub mod rpc_methods;\npub use rpc_methods::{methods, MethodSpec, Rpc, RpcKind, RpcMethod, METHODS};\n`;
  body += `pub mod http_endpoints;\npub use http_endpoints::{endpoints, EndpointSpec, HttpEndpoint, PayloadEncoding, ENDPOINTS};\n`;
  body += `#[cfg(any(test, feature = "registry"))]\npub mod registry;\n`;
  writeGen("mod.rs", body);
}

// rustfmt the output so `cargo fmt --check` stays clean (the workspace has no rustfmt.toml)
try {
  const files = Fs.readdirSync(GEN_DIR)
    .filter((f) => f.endsWith(".rs"))
    .map((f) => Path.join(GEN_DIR, f));
  execFileSync("rustfmt", ["--edition", "2021", ...files], { stdio: "inherit" });
} catch (e) {
  console.warn(`rustfmt failed or is missing; generated code left unformatted (${String(e)})`);
}

// ---------------------------------------------------------------------------------------------
// Fixtures: TS-encoded samples (arbitrary values with fixed seeds + curated edge cases)
// ---------------------------------------------------------------------------------------------

// JSON.stringify escapes lone surrogates as `\udXXX` (paired ones stay raw). Rust strings cannot
// hold them, so such values are skipped (documented deviation).
const LONE_SURROGATE = /\\u[dD][89a-fA-F][0-9a-fA-F]{2}/;
/**
 * Drops object keys whose value is `undefined` from a sampled (type-side) value. TS encodes a
 * present-but-undefined `Schema.optional` key as `null` and decodes it back as undefined; Rust
 * models absent and undefined alike (`None`, not serialized). Documented deviation.
 */
function stripUndefined(v: unknown, seen = new Set<unknown>()): unknown {
  if (v === null || typeof v !== "object" || v instanceof Uint8Array || seen.has(v)) return v;
  seen.add(v);
  if (Array.isArray(v)) {
    v.forEach((x, i) => {
      try {
        if (typeof x === "string" && x !== "" && x.trim() === "") v[i] = "x";
        else stripUndefined(x, seen);
      } catch {
        /* frozen */
      }
    });
    return v;
  }
  for (const k of Object.keys(v)) {
    const o = v as Record<string, unknown>;
    try {
      if (o[k] === undefined) delete o[k];
      // `TrimmedNonEmptyString` accepts " " on the type side but encodes it to "", which no
      // longer decodes: such samples are TS-invalid, and they are frequent. Patch them.
      else if (typeof o[k] === "string" && o[k] !== "" && (o[k] as string).trim() === "")
        o[k] = "x";
      else stripUndefined(o[k], seen);
    } catch {
      /* frozen: leave it */
    }
  }
  return v;
}
function encodeWith(schema: Schema.Top, value: unknown): unknown {
  return Schema.encodeSync(Schema.toCodecJson(schema) as any)(value);
}
function sampleTarget(t: Target, count: number, seed: number): unknown[] {
  const out: unknown[] = [];
  if (t.schema.ast._tag === "Never") return out;
  let values: ReadonlyArray<unknown> = [];
  try {
    values = Effect.runSync(
      Arbitrary.sampleEffect(Arbitrary.schema(t.schema as any), {
        count: count * 8,
        seed,
        size: 4,
      }),
    );
  } catch {
    return out;
  }
  const seen = new Set<string>();
  for (const v of values) {
    if (out.length >= count) break;
    try {
      const enc = encodeWith(t.schema, stripUndefined(v));
      const text = JSON.stringify(enc);
      if (text === undefined || LONE_SURROGATE.test(text) || seen.has(text)) continue;
      // must decode back in TS (arbitrary values of Unknown fields may not be JSON)
      Schema.decodeUnknownSync(Schema.toCodecJson(t.schema) as any)(JSON.parse(text));
      seen.add(text);
      out.push(JSON.parse(text));
    } catch {
      /* skip values that are not JSON-encodable */
    }
  }
  return out;
}

/**
 * Fallback for schemas whose whole-value samples never encode (an `Unknown` field got a non-JSON
 * value, a refined string the sampler cannot satisfy…): build the encoded value field by field
 * from samples of each property schema, then let TS validate the result.
 */
function composeEncoded(
  ast: A,
  seed: number,
  depth = 0,
): { ok: true; value: unknown } | { ok: false } {
  const whole = sampleTarget(
    { id: "", rust: "", schema: Schema.make(AST.replaceContext(ast, undefined)) as any },
    1,
    seed,
  );
  if (whole.length > 0) return { ok: true, value: whole[0] };
  if (depth > 8) return { ok: false };
  const typeSide =
    ast.encoding && ["Objects", "Arrays", "Union"].includes(ast._tag) ? strip(ast) : ast;
  const node =
    typeSide._tag === "Declaration" && typeSide.encoding ? finalNode(typeSide) : typeSide;
  switch (node._tag) {
    case "Objects": {
      if (any(node).indexSignatures.length > 0) return { ok: true, value: {} };
      const out: Record<string, unknown> = {};
      let i = 0;
      for (const ps of any(node).propertySignatures as Array<{ name: PropertyKey; type: A }>) {
        i++;
        const optional = AST.isOptional(finalNode(ps.type));
        if (optional && (seed + i) % 2 === 0) continue;
        const r = composeEncoded(ps.type, seed * 31 + i, depth + 1);
        if (r.ok) out[String(ps.name)] = r.value;
        else if (!optional) return { ok: false };
      }
      return { ok: true, value: out };
    }
    case "Arrays": {
      if (any(node).elements.length > 0) return { ok: false };
      const r = composeEncoded(any(node).rest[0], seed * 17 + 1, depth + 1);
      return { ok: true, value: r.ok ? [r.value] : [] };
    }
    case "Union": {
      const members = any(node).types as A[];
      for (let k = 0; k < members.length; k++) {
        const r = composeEncoded(members[(seed + k) % members.length]!, seed * 13 + k, depth + 1);
        if (r.ok) return r;
      }
      return { ok: false };
    }
    case "Suspend":
      return composeEncoded(any(node).thunk(), seed, depth + 1);
  }
  return { ok: false };
}
function composeTarget(t: Target, count: number, seed: number): unknown[] {
  const out: unknown[] = [];
  const seen = new Set<string>();
  const codec = Schema.toCodecJson(t.schema) as any;
  for (let attempt = 0; attempt < count * 6 && out.length < count; attempt++) {
    const r = composeEncoded(t.schema.ast, seed + attempt * 7919);
    if (!r.ok) continue;
    try {
      const value = Schema.encodeSync(codec)(
        stripUndefined(Schema.decodeUnknownSync(codec)(r.value)),
      );
      const text = JSON.stringify(value);
      if (LONE_SURROGATE.test(text) || seen.has(text)) continue;
      seen.add(text);
      out.push(JSON.parse(text));
    } catch {
      /* the composition broke a cross-field rule: try again */
    }
  }
  return out;
}

Fs.mkdirSync(FIXTURES, { recursive: true });
const unsampled: string[] = [];
{
  const lines: string[] = [];
  targets.forEach((t, i) => {
    const count = t.id.startsWith("rpc:") || t.id.startsWith("http:") ? 4 : 3;
    let samples = sampleTarget(t, count, 1000 + i);
    if (samples.length === 0 && t.schema.ast._tag !== "Never")
      samples = composeTarget(t, count, 5000 + i);
    if (samples.length === 0 && t.schema.ast._tag !== "Never") unsampled.push(t.id);
    for (const value of samples) lines.push(JSON.stringify({ schema: t.id, value }));
  });
  Fs.writeFileSync(Path.join(FIXTURES, "samples.jsonl"), lines.join("\n") + "\n");
}

// curated: wire values written by hand, decoded and re-encoded by TS (so they are canonical)
// `value` is a wire value; `fromExport` names a TYPE-side constant of the contracts to encode.
const curatedInput: Array<{ schema: string; value?: unknown; fromExport?: string; note?: string }> =
  JSON.parse(Fs.readFileSync(Path.join(HERE, "contracts-curated.json"), "utf8"));
{
  const lines: string[] = [];
  for (const c of curatedInput) {
    const t = targets.find((x) => x.id === c.schema);
    if (!t) throw new Error(`curated fixture: unknown schema ${c.schema}`);
    const codec = Schema.toCodecJson(t.schema) as any;
    let decoded: unknown;
    try {
      decoded =
        c.fromExport !== undefined
          ? (Contracts as any)[c.fromExport]
          : Schema.decodeUnknownSync(codec)(c.value);
      if (decoded === undefined && c.fromExport !== undefined)
        throw new Error(`no export ${c.fromExport}`);
    } catch (e) {
      throw new Error(`curated fixture for ${c.schema} does not decode in TS: ${String(e)}`);
    }
    const value = Schema.encodeSync(codec)(decoded);
    lines.push(JSON.stringify({ schema: c.schema, value }));
  }
  Fs.writeFileSync(Path.join(FIXTURES, "curated.jsonl"), lines.join("\n") + "\n");
}
Fs.writeFileSync(Path.join(FIXTURES, ".gitignore"), "harvested/\n");

// ---------------------------------------------------------------------------------------------
// Harvest real payloads from a COPY of state.sqlite (personal data: git-ignored output)
// ---------------------------------------------------------------------------------------------

if (HARVEST_DB) {
  const dir = Path.join(FIXTURES, "harvested");
  Fs.mkdirSync(dir, { recursive: true });
  const query = (sql: string): any[] => {
    const out = execFileSync("sqlite3", ["-readonly", "-json", HARVEST_DB, sql], {
      maxBuffer: 1 << 30,
    }).toString();
    return out.trim() ? JSON.parse(out) : [];
  };
  const decodeRow = Schema.decodeUnknownSync(Contracts.OrchestrationEvent as any);
  const encodeEvent = Schema.encodeSync(Schema.toCodecJson(Contracts.OrchestrationEvent) as any);
  const rows = query(
    `SELECT sequence, event_id AS eventId, event_type AS type, aggregate_kind AS aggregateKind, stream_id AS aggregateId, occurred_at AS occurredAt, command_id AS commandId, causation_event_id AS causationEventId, correlation_id AS correlationId, payload_json AS payload, metadata_json AS metadata FROM orchestration_events ORDER BY sequence`,
  );
  const lines: string[] = [];
  const rawLines: string[] = [];
  let failed = 0;
  for (const row of rows) {
    try {
      const raw = { ...row, payload: JSON.parse(row.payload), metadata: JSON.parse(row.metadata) };
      const event = decodeRow(raw);
      const value = encodeEvent(event);
      const text = JSON.stringify(value);
      if (LONE_SURROGATE.test(text)) continue;
      lines.push(JSON.stringify({ schema: "OrchestrationEvent", value }));
      // the row as stored (columns + payload_json + metadata_json): Rust must read it as well,
      // and re-encode it to the same value (the DB holds the wire shape)
      rawLines.push(JSON.stringify({ schema: "OrchestrationEvent", value: raw }));
    } catch {
      failed++;
    }
  }
  Fs.writeFileSync(Path.join(dir, "orchestration-events.jsonl"), lines.join("\n") + "\n");
  Fs.writeFileSync(Path.join(dir, "orchestration-event-rows.jsonl"), rawLines.join("\n") + "\n");
  console.log(
    `harvested ${lines.length} orchestration events (${failed} rows did not decode in TS)`,
  );

  // provider runtime events from the provider logs (CANON lines)
  const logDir = Path.join(Path.dirname(HARVEST_DB), "logs", "provider");
  const altLogDir = Path.join(process.env.HOME ?? "", ".zenith/code/userdata/logs/provider");
  const useDir = Fs.existsSync(logDir) ? logDir : Fs.existsSync(altLogDir) ? altLogDir : undefined;
  if (useDir) {
    const decodeRuntime = Schema.decodeUnknownSync(
      Schema.toCodecJson(Contracts.ProviderRuntimeEvent) as any,
    );
    const encodeRuntime = Schema.encodeSync(
      Schema.toCodecJson(Contracts.ProviderRuntimeEvent) as any,
    );
    const out: string[] = [];
    let bad = 0;
    for (const file of Fs.readdirSync(useDir).sort()) {
      for (const line of Fs.readFileSync(Path.join(useDir, file), "utf8").split("\n")) {
        const at = line.indexOf("] CANON: ");
        if (at < 0) continue;
        try {
          const value = encodeRuntime(decodeRuntime(JSON.parse(line.slice(at + 9))));
          const text = JSON.stringify(value);
          if (LONE_SURROGATE.test(text)) continue;
          out.push(JSON.stringify({ schema: "ProviderRuntimeEvent", value }));
        } catch {
          bad++;
        }
      }
    }
    Fs.writeFileSync(Path.join(dir, "provider-runtime-events.jsonl"), out.join("\n") + "\n");
    console.log(
      `harvested ${out.length} provider runtime events (${bad} lines did not decode in TS)`,
    );
  }
}

// ---------------------------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------------------------

const counts = { struct: 0, newtype: 0, alias: 0, strEnum: 0, union: 0 };
for (const item of items.values()) counts[item.kind]++;
const summary = {
  exports: allExports.length,
  items: items.size,
  ...counts,
  literalTypes: lits.size,
  exportAliases: aliasExports.length,
  rpcs: rpcs.length,
  streams: rpcs.filter((r) => r.stream).length,
  endpoints: https.length,
  schemaIds: targets.length,
  unsampled: unsampled.length,
};
Fs.writeFileSync(
  Path.join(FIXTURES, "generation-report.json"),
  JSON.stringify({ summary, unsampled, deviations: [...new Set(deviations)].sort() }, null, 2) +
    "\n",
);
console.log(JSON.stringify(summary));
