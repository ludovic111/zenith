/**
 * The TypeScript workspace search as an oracle for zc-workspace (WP-24).
 *
 *   node search-oracle.ts <code dir> <request.json>
 *
 * Runs `code/apps/server/src/workspace/WorkspaceSearchIndex.ts` from source (the real
 * `@ff-labs/fff-node`, resolved from `code/apps/server/node_modules`) over `request.cwd` and
 * prints, as one JSON document, what `list`, `search` (after the same `normalizeSearchQuery`
 * as `WorkspaceEntries.search`) and `searchContents` return for each request. The Rust golden
 * test runs the same requests through `zc_workspace::WorkspaceSearchIndex` and compares.
 */
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

interface PathQuery {
  readonly query: string;
  readonly limit: number;
  readonly kind?: "file" | "directory";
  readonly imageOnly?: boolean;
}

interface ContentQuery {
  readonly query: string;
  readonly limit: number;
  readonly caseSensitive: boolean;
  readonly wholeWord: boolean;
  readonly useRegex: boolean;
}

interface Request {
  readonly cwd: string;
  readonly pathQueries: ReadonlyArray<PathQuery>;
  readonly contentQueries: ReadonlyArray<ContentQuery>;
}

const [codeDir, requestPath] = process.argv.slice(2);
if (!codeDir || !requestPath) {
  console.error("usage: search-oracle.ts <code dir> <request.json>");
  process.exit(2);
}
const request = JSON.parse(readFileSync(requestPath, "utf8")) as Request;
const serverDir = join(codeDir, "apps/server");
const requireFromServer = createRequire(join(serverDir, "package.json"));
const load = (specifier: string) => import(pathToFileURL(specifier).href);

const Effect = await load(requireFromServer.resolve("effect/Effect"));
const WorkspaceSearchIndex = await load(join(serverDir, "src/workspace/WorkspaceSearchIndex.ts"));
const { normalizeSearchQuery } = await load(join(codeDir, "packages/shared/src/searchRanking.ts"));

const program = Effect.gen(function* () {
  const paths = yield* WorkspaceSearchIndex.make(request.cwd, "paths");
  const content = yield* WorkspaceSearchIndex.make(request.cwd, "content");
  const list = yield* paths.list();
  const search = [];
  for (const query of request.pathQueries) {
    const normalized = normalizeSearchQuery(query.query, { trimLeadingPattern: /^[@./]+/ });
    search.push(yield* paths.search(normalized, query.limit, query.kind, query.imageOnly));
  }
  const contents = [];
  for (const query of request.contentQueries) {
    contents.push(yield* content.searchContents(query));
  }
  return { list, search, contents };
});

const result = await Effect.runPromise(Effect.scoped(program));
process.stdout.write(JSON.stringify(result));
