/**
 * The TS oracle for WP-14 gate 3: how the TypeScript Codex driver launches `codex app-server`.
 *
 *   node apps/server/scripts/codex-launch-oracle.ts > ../crates/zenith-code/crates/zc-provider-codex/tests/fixtures/launch_oracle.json
 *
 * For each case it runs the real `makeCodexAdapter(...).startSession(...)` (and the status
 * probe's `withCodexAppServerClient`) against a ChildProcessSpawner that records the command
 * and refuses to start it, and prints {case, spawn: {command, args, cwd, env, extendEnv}}.
 * `crates/.../tests/launch_oracle.rs` computes the same spawns in Rust and compares.
 *
 * Inputs avoid `~` and the real environment (explicit `environment`), so the output is
 * deterministic and holds no personal data. Needs `node_modules` (see docs/zenith-code/codex.md).
 */
import * as NodeServices from "@effect/platform-node/NodeServices";
import { CodexSettings, ProviderInstanceId, ThreadId } from "@t3tools/contracts";
import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as PlatformError from "effect/PlatformError";
import * as Schema from "effect/Schema";
import * as ChildProcessSpawner from "effect/unstable/process/ChildProcessSpawner";

import { ServerConfig } from "../src/config.ts";
import * as McpProviderSession from "../src/mcp/McpProviderSession.ts";
import { makeCodexAdapter } from "../src/provider/Layers/CodexAdapter.ts";
import { withCodexAppServerClient } from "../src/provider/Layers/CodexProvider.ts";
import { resolveCodexLaunchArgs } from "../src/provider/Layers/codexLaunchArgs.ts";

const decodeSettings = Schema.decodeSync(CodexSettings);

interface Case {
  readonly name: string;
  readonly kind: "session" | "probe";
  readonly config: Record<string, unknown>;
  readonly instanceId?: string;
  readonly environment: Record<string, string>;
  readonly start: Record<string, unknown>;
  readonly mcp?: {
    readonly endpoint: string;
    readonly authorizationHeader: string;
    readonly capabilities: ReadonlyArray<string>;
    readonly agentDeviceEnvironment?: Record<string, string>;
  };
}

const baseEnvironment = { PATH: "/usr/bin:/bin", LANG: "en_US.UTF-8" };

const cases: ReadonlyArray<Case> = [
  {
    name: "defaults",
    kind: "session",
    config: {},
    environment: baseEnvironment,
    start: { threadId: "thread-a", cwd: "/work/project", runtimeMode: "full-access" },
  },
  {
    name: "binary, home and launch args from settings",
    kind: "session",
    config: {
      binaryPath: "/opt/example/bin/codex",
      homePath: "/home/example/.codex-work",
      launchArgs: "--strict-config --enable foo -c 'model=\"gpt 5\"'",
    },
    environment: baseEnvironment,
    start: { threadId: "thread-b", cwd: "/work/other", runtimeMode: "approval-required" },
  },
  {
    name: "launch args from T3CODE_CODEX_LAUNCH_ARGS win",
    kind: "session",
    config: { launchArgs: "--enable settings-feature" },
    environment: { ...baseEnvironment, T3CODE_CODEX_LAUNCH_ARGS: " --strict-config --enable env-feature " },
    start: { threadId: "thread-c", cwd: "/work/project", runtimeMode: "auto" },
  },
  {
    name: "blank T3CODE_CODEX_LAUNCH_ARGS falls back to settings",
    kind: "session",
    config: { launchArgs: " --strict-config " },
    environment: { ...baseEnvironment, T3CODE_CODEX_LAUNCH_ARGS: "   " },
    start: { threadId: "thread-d", cwd: "/work/project", runtimeMode: "auto-accept-edits" },
  },
  {
    name: "t3-code MCP session with device tools",
    kind: "session",
    config: { homePath: "/home/example/.codex", launchArgs: "--strict-config" },
    instanceId: "codex_personal",
    environment: baseEnvironment,
    start: {
      threadId: "thread-mcp",
      cwd: "/work/project",
      runtimeMode: "full-access",
      modelSelection: { instanceId: "codex_personal", model: "gpt-5.4", options: [{ id: "serviceTier", value: "fast" }] },
    },
    mcp: {
      endpoint: "http://127.0.0.1:41234/mcp",
      authorizationHeader: "Bearer example-token-123",
      capabilities: ["preview", "device"],
      agentDeviceEnvironment: { PATH: "/opt/example/agent-device/bin", AGENT_DEVICE_DAEMON: "http://127.0.0.1:41235" },
    },
  },
  {
    name: "MCP session without device environment",
    kind: "session",
    config: {},
    environment: baseEnvironment,
    start: { threadId: "thread-mcp-plain", cwd: "/work/project", runtimeMode: "full-access" },
    mcp: { endpoint: "http://127.0.0.1:9/mcp", authorizationHeader: "Bearer x", capabilities: ["preview"] },
  },
  {
    name: "status probe",
    kind: "probe",
    config: { binaryPath: "/opt/example/bin/codex", homePath: "/home/example/.codex-work", launchArgs: "--strict-config" },
    environment: baseEnvironment,
    start: { cwd: "/work/project" },
  },
  {
    name: "status probe without a home",
    kind: "probe",
    config: { launchArgs: "" },
    environment: { ...baseEnvironment, T3CODE_CODEX_LAUNCH_ARGS: "--enable env-feature" },
    start: { cwd: "/work/project" },
  },
];

interface Recorded {
  command: string;
  args: ReadonlyArray<string>;
  cwd: string | undefined;
  env: Record<string, string | undefined> | undefined;
  extendEnv: boolean | undefined;
}

const run = Effect.gen(function* () {
  const results: Array<{ case: Case; spawn: Recorded | null }> = [];
  for (const testCase of cases) {
    let recorded: Recorded | null = null;
    const spawner = ChildProcessSpawner.make((command) => {
      if (command._tag === "StandardCommand") {
        recorded = {
          command: command.command,
          args: [...command.args],
          cwd: command.options.cwd,
          env: command.options.env,
          extendEnv: command.options.extendEnv,
        };
      }
      return Effect.fail(
        PlatformError.systemError({ _tag: "NotFound", module: "ChildProcess", method: "spawn", description: "oracle: not started" }),
      );
    });
    const config = decodeSettings(testCase.config);
    if (testCase.kind === "session") {
      McpProviderSession.clearAllMcpProviderSessions();
      if (testCase.mcp) {
        McpProviderSession.setMcpProviderSession({
          environmentId: "env" as never,
          threadId: ThreadId.make(String(testCase.start.threadId)),
          providerSessionId: "provider-session",
          providerInstanceId: ProviderInstanceId.make(testCase.instanceId ?? "codex"),
          endpoint: testCase.mcp.endpoint,
          authorizationHeader: testCase.mcp.authorizationHeader,
          capabilities: new Set(testCase.mcp.capabilities),
          ...(testCase.mcp.agentDeviceEnvironment ? { agentDeviceEnvironment: testCase.mcp.agentDeviceEnvironment } : {}),
        });
      }
      yield* Effect.gen(function* () {
        const adapter = yield* makeCodexAdapter(config, {
          ...(testCase.instanceId ? { instanceId: ProviderInstanceId.make(testCase.instanceId) } : {}),
          environment: testCase.environment,
        });
        yield* adapter.startSession(testCase.start as never).pipe(Effect.ignore);
      }).pipe(Effect.scoped, Effect.provideService(ChildProcessSpawner.ChildProcessSpawner, spawner));
    } else {
      yield* withCodexAppServerClient({
        binaryPath: config.binaryPath,
        homePath: config.homePath,
        // What checkCodexProviderStatus passes the probe.
        launchArgs: resolveCodexLaunchArgs(config.launchArgs, testCase.environment),
        cwd: String(testCase.start.cwd),
        environment: testCase.environment,
      }).pipe(Effect.scoped, Effect.ignore, Effect.provideService(ChildProcessSpawner.ChildProcessSpawner, spawner));
    }
    results.push({ case: testCase, spawn: recorded });
  }
  return results;
});

const results = await Effect.runPromise(
  run.pipe(
    Effect.provide(ServerConfig.layerTest("/work/project", { prefix: "codex-launch-oracle-" }).pipe(Layer.provideMerge(NodeServices.layer))),
    Effect.scoped,
  ),
);
process.stdout.write(`${JSON.stringify(results, null, 2)}\n`);
