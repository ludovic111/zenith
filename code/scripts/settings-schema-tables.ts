/**
 * Prints the decode tables `crates/zenith-code/crates/zc-settings/src/settings/schema.rs` keeps
 * for `ServerSettings` (and `ServerSettingsPatch`), extracted from the Effect AST:
 *
 *   node scripts/settings-schema-tables.ts [ServerSettings|ServerSettingsPatch]
 *
 * - String rules: every string node is decoded with `" a "`, `""` and `" "`; the behaviour gives
 *   `Trim` (TrimmedString), `TrimNonEmpty` (TrimmedNonEmptyString) or `TrimOr("x")` (the legacy
 *   binary paths, empty → executable name).
 * - Nested decoding defaults: each property signature is decoded from `{}`; a key that comes
 *   back holds its `withDecodingDefault` value (encoded). The generated Rust types apply the
 *   top-level ones only, so zc-settings fills the nested ones before the typed decode.
 *
 * Paths: `.`-separated keys, `*` every value of a record, `[]` every array element, `<key>`
 * record keys. Union members share their parent's path.
 */
import * as Contracts from "../packages/contracts/src/index.ts";
import * as Schema from "effect/Schema";
import * as AST from "effect/SchemaAST";

type Ast = any;

const name = process.argv[2] ?? "ServerSettings";
const root: Ast = (Contracts as any)[name].ast;

function stringRule(ast: Ast): string | undefined {
  if (!ast.encoding && !ast.checks) return undefined;
  let decode: (value: unknown) => unknown;
  try {
    decode = Schema.decodeUnknownSync(Schema.make(ast) as any);
  } catch {
    return undefined;
  }
  const probe = (value: unknown) => {
    try {
      return { ok: true, value: decode(value) };
    } catch {
      return { ok: false, value: undefined };
    }
  };
  const spaced = probe(" a ");
  const empty = probe("");
  const blank = probe(" ");
  if (spaced.ok && spaced.value === " a ") return undefined;
  if (!spaced.ok) return "Trim"; // pattern-checked trimmed strings (autoCompactWindow)
  if (spaced.value !== "a") return `UNKNOWN(${JSON.stringify(spaced.value)})`;
  if (!empty.ok) return "TrimNonEmpty";
  if (empty.value === "") return "Trim";
  if (typeof empty.value === "string" && blank.ok && blank.value === empty.value) {
    return `TrimOr(${JSON.stringify(empty.value)})`;
  }
  return `UNKNOWN(${JSON.stringify(empty.value)})`;
}

const rules = new Map<string, string>();
const defaults: Array<string> = [];
const seen = new Set<Ast>();

function join(path: string, segment: string) {
  return path === "" ? segment : `${path}.${segment}`;
}

function walk(ast: Ast, path: string, depth = 0) {
  if (depth > 40) return;
  const rule = ast._tag === "String" ? stringRule(ast) : undefined;
  if (rule && path !== "") rules.set(path, rule);
  switch (ast._tag) {
    case "Objects":
      for (const ps of ast.propertySignatures) {
        const key = String(ps.name);
        try {
          const one = Schema.make(new (AST as any).Objects([ps], []));
          const decoded = Schema.decodeUnknownSync(one as any)({}) as Record<string, unknown>;
          if (path !== "" && Object.hasOwn(decoded, key)) {
            const encoded = Schema.encodeSync(Schema.make(ps.type) as any)(decoded[key]);
            const line = `    (${JSON.stringify(path)}, ${JSON.stringify(key)}, r#"${JSON.stringify(encoded)}"#),`;
            if (!defaults.includes(line)) defaults.push(line);
          }
        } catch {
          // a required key: no default
        }
        walk(ps.type, join(path, key), depth + 1);
      }
      for (const is of ast.indexSignatures) {
        const keyRule = is.parameter._tag === "String" ? stringRule(is.parameter) : undefined;
        if (keyRule) rules.set(join(path, "<key>"), keyRule);
        walk(is.type, join(path, "*"), depth + 1);
      }
      break;
    case "Arrays":
      for (const rest of ast.rest) walk(rest, join(path, "[]"), depth + 1);
      break;
    case "Union":
      for (const member of ast.types) walk(member, path, depth + 1);
      break;
    case "Suspend":
      if (!seen.has(ast)) {
        seen.add(ast);
        walk(ast.thunk(), path, depth + 1);
      }
      break;
  }
}

walk(root, "");
const format = (rule: string) =>
  rule.startsWith("TrimOr(") ? `StringRule::TrimOr(${rule.slice(7, -1)})` : `StringRule::${rule}`;
console.log(`// ${name}: string rules`);
for (const [path, rule] of rules) console.log(`    (${JSON.stringify(path)}, ${format(rule)}),`);
console.log(`// ${name}: nested decoding defaults`);
for (const line of defaults) console.log(line);
