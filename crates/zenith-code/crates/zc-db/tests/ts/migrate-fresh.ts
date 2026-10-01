// Creates a fresh SQLite database with the TypeScript server's own migrations, the way
// `persistence/Layers/Sqlite.ts` does (same pragmas, same Migrator), so the Rust port's
// `sqlite_master` can be compared against it.
//
//   node crates/zenith-code/crates/zc-db/tests/ts/migrate-fresh.ts <db path>
//
// Needs `code/node_modules`, `code/apps/server/node_modules`, `code/packages/shared/node_modules`
// and `code/packages/contracts/node_modules` (symlinks to a checkout that ran `pnpm install` are
// fine). The `ts_schema_matches` test in `tests/gate.rs` runs it when `node` and those exist.
// `effect` is reached through the server package so it is the very module instance the
// migrations use (this file sits outside the pnpm workspace).
import * as Effect from "../../../../../../code/apps/server/node_modules/effect/dist/Effect.js";
import * as SqlClient from "../../../../../../code/apps/server/node_modules/effect/dist/unstable/sql/SqlClient.js";

import * as NodeSqliteClient from "../../../../../../code/packages/shared/src/nodeSqliteClient.ts";
import { runMigrations } from "../../../../../../code/apps/server/src/persistence/Migrations.ts";

const dbPath = process.argv[2];
if (!dbPath) {
  console.error("usage: migrate-fresh.ts <db path>");
  process.exit(2);
}

const program = Effect.gen(function* () {
  const sql = yield* SqlClient.SqlClient;
  yield* sql`PRAGMA busy_timeout = 5000;`;
  yield* sql`PRAGMA foreign_keys = ON;`;
  yield* sql`PRAGMA journal_mode = WAL;`;
  yield* sql.unsafe(`PRAGMA journal_size_limit = ${32 * 1024 * 1024};`);
  const ran = yield* runMigrations();
  console.log(JSON.stringify(ran.map(([id, name]) => `${id}_${name}`)));
});

await Effect.runPromise(
  Effect.scoped(program.pipe(Effect.provide(NodeSqliteClient.layer({ filename: dbPath })))),
);
