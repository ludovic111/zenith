/**
 * Test data: a deterministic synthetic git repository, and a *safe copy* of a live
 * `~/.zenith/code` for validation runs.
 */
import * as NodeChildProcess from "node:child_process";
import * as NodeFS from "node:fs";
import * as NodePath from "node:path";

/**
 * A small git repository whose commit hashes are the same on every machine and every run
 * (fixed author, committer and dates), so recordings made on it replay byte-for-byte.
 */
export const makeSyntheticRepo = (dir: string): string => {
  NodeFS.mkdirSync(NodePath.join(dir, "src"), { recursive: true });
  NodeFS.writeFileSync(
    NodePath.join(dir, "README.md"),
    "# Synthetic project\n\nUsed by the zenith code compat harness.\n",
  );
  NodeFS.writeFileSync(NodePath.join(dir, "src/index.ts"), "export const needle = 1;\n");
  NodeFS.writeFileSync(
    NodePath.join(dir, "src/util.ts"),
    "export const add = (a: number, b: number) => a + b;\n",
  );
  const env = {
    ...process.env,
    GIT_AUTHOR_NAME: "Compat Harness",
    GIT_AUTHOR_EMAIL: "compat@example.invalid",
    GIT_COMMITTER_NAME: "Compat Harness",
    GIT_COMMITTER_EMAIL: "compat@example.invalid",
    GIT_AUTHOR_DATE: "2026-01-01T00:00:00Z",
    GIT_COMMITTER_DATE: "2026-01-01T00:00:00Z",
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_CONFIG_GLOBAL: "/dev/null",
  };
  const git = (...args: Array<string>) =>
    NodeChildProcess.execFileSync("git", args, { cwd: dir, env, stdio: "pipe" });
  git("init", "--quiet", "--initial-branch=main");
  git("add", "-A");
  git("commit", "--quiet", "-m", "Initial commit");
  return NodeFS.realpathSync(dir);
};

/**
 * settings.json for synthetic homes: every provider driver disabled and no update checks, so no
 * provider CLI is probed. Provider status then depends on nothing installed or logged in on the
 * machine (a logged-in `claude` answers from the keychain even with an empty HOME), which keeps
 * recordings deterministic and free of the owner's accounts and skills.
 */
export const SYNTHETIC_SETTINGS = {
  enableProviderUpdateChecks: false,
  providers: {
    codex: { enabled: false },
    claudeAgent: { enabled: false },
    cursor: { enabled: false },
    grok: { enabled: false },
    opencode: { enabled: false },
    antigravity: { enabled: false },
  },
};

/** Files of a live base dir that a validation copy needs. Logs, secrets and caches stay behind. */
// No -shm: SQLite rebuilds the wal-index from the WAL when the copy is first opened.
const LIVE_COPY_FILES = [
  "userdata/state.sqlite",
  "userdata/state.sqlite-wal",
  "userdata/settings.json",
  "userdata/keybindings.json",
  "userdata/environment-id",
  "userdata/model-manifest.json",
];

/**
 * Copies a live base dir (read-only on the source) into `dest`, then makes the copy safe to
 * start a server on:
 *  - no `secrets/` (provider API keys, signing keys) and no logs are copied;
 *  - settings: `continueThreadsAfterServerUpdate` and `defaultAutoPull` forced off, globally and
 *    per project, so startup neither resumes provider turns nor pulls the real repositories;
 *  - database: `projection_projects.auto_pull = 0`.
 * Thread content stays in the copy: recordings made on it must not be committed.
 */
export const copyLiveHome = (source: string, dest: string): void => {
  for (const relative of LIVE_COPY_FILES) {
    const from = NodePath.join(source, relative);
    if (!NodeFS.existsSync(from)) continue;
    const to = NodePath.join(dest, relative);
    NodeFS.mkdirSync(NodePath.dirname(to), { recursive: true });
    NodeFS.copyFileSync(from, to);
  }
  const themes = NodePath.join(source, "userdata/themes");
  if (NodeFS.existsSync(themes))
    NodeFS.cpSync(themes, NodePath.join(dest, "userdata/themes"), { recursive: true });

  const settingsFile = NodePath.join(dest, "userdata/settings.json");
  const settings = NodeFS.existsSync(settingsFile)
    ? (JSON.parse(NodeFS.readFileSync(settingsFile, "utf8")) as Record<string, unknown>)
    : {};
  settings.continueThreadsAfterServerUpdate = false;
  settings.defaultAutoPull = false;
  const overrides = (settings.projectSettingsOverrides ?? {}) as Record<
    string,
    Record<string, unknown>
  >;
  for (const override of Object.values(overrides)) {
    delete override.continueThreadsAfterServerUpdate;
    override.defaultAutoPull = false;
  }
  NodeFS.writeFileSync(settingsFile, `${JSON.stringify(settings, null, 2)}\n`);

  const db = NodePath.join(dest, "userdata/state.sqlite");
  if (NodeFS.existsSync(db)) {
    NodeChildProcess.execFileSync(
      "sqlite3",
      [db, "UPDATE projection_projects SET auto_pull = 0;"],
      { stdio: "pipe" },
    );
  }
};
