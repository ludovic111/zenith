// @effect-diagnostics nodeBuiltinImport:off globalDate:off globalTimers:off - a standalone Node oracle script.
/**
 * The TypeScript `/mcp` endpoint as an oracle for the Rust port (`zc-mcp`).
 *
 *   node apps/server/scripts/mcp-oracle.ts run <script.json> --out transcript.json
 *   node apps/server/scripts/mcp-oracle.ts tools --out tools.json
 *
 * `run` serves the real `McpHttpServer.layer` (bearer auth, the patched DELETE, the 202
 * rewrite, every toolkit) on a random loopback port, with the projection and engine replaced
 * by the script's fixtures and a scripted browser host connected to the real
 * `PreviewAutomationBroker`. It plays the script's HTTP steps in order and prints, per step,
 * the status, the MCP headers and the body, then the commands the engine received and the
 * requests the browser host received. Session ids, command uuids and broker connection ids
 * are replaced by placeholders so the transcript is stable.
 *
 * `tools` prints the `tools/list` result (the tool descriptors the Rust server serves) and the
 * success schemas of the two image tools.
 *
 * The Rust side is `cargo test -p zc-mcp --test http_golden` (the committed transcript) and
 * `cargo test -p zc-mcp --test http_golden -- --ignored` (re-runs this oracle live).
 */
import * as NodeFs from "node:fs";
import * as NodeHttp from "node:http";

import * as NodeHttpServer from "@effect/platform-node/NodeHttpServer";
import * as NodeServices from "@effect/platform-node/NodeServices";
import * as Context from "effect/Context";
import * as Effect from "effect/Effect";
import * as Layer from "effect/Layer";
import * as Option from "effect/Option";
import * as Stream from "effect/Stream";
import { Tool } from "effect/unstable/ai";
import { HttpRouter, HttpServer } from "effect/unstable/http";
import {
  EnvironmentId,
  ProviderInstanceId,
  ThreadId,
  type OrchestrationCommand,
} from "@t3tools/contracts";

import * as ServerConfig from "../src/config.ts";
import * as DeviceService from "../src/device/DeviceService.ts";
import * as ServerEnvironment from "../src/environment/ServerEnvironment.ts";
import * as McpHttpServer from "../src/mcp/McpHttpServer.ts";
import { DeviceScreenshotTool } from "../src/mcp/toolkits/device/tools.ts";
import { PreviewSnapshotTool } from "../src/mcp/toolkits/preview/tools.ts";
import * as McpSessionRegistry from "../src/mcp/McpSessionRegistry.ts";
import * as PreviewAutomationBroker from "../src/mcp/PreviewAutomationBroker.ts";
import { OrchestrationCommandInvariantError } from "../src/orchestration/Errors.ts";
import { OrchestrationEngineService } from "../src/orchestration/Services/OrchestrationEngine.ts";
import { ProjectionSnapshotQuery } from "../src/orchestration/Services/ProjectionSnapshotQuery.ts";

interface Step {
  readonly name: string;
  readonly method: string;
  /** Which credential: a key of `credentials`, `"none"` or `"bad"`. */
  readonly token: string;
  /** `"$session"` in a header value stands for the session the last initialize opened. */
  readonly headers?: Record<string, string>;
  readonly body?: string;
}

interface Script {
  readonly environmentId: string;
  readonly threadId: string;
  readonly thread: Record<string, unknown> | null;
  readonly project: Record<string, unknown> | null;
  /** Link commands for these numbers fail with an invariant error ("already linked"). */
  readonly rejectLink: ReadonlyArray<number>;
  /** Unlink commands for these numbers fail with an invariant error ("not linked"). */
  readonly rejectUnlink: ReadonlyArray<number>;
  /** Credential name → capabilities beyond pull-requests. */
  readonly credentials: Record<string, ReadonlyArray<"preview" | "device">>;
  /** Connects a browser host answering per operation; omit for no host. */
  readonly host?: {
    readonly clientId: string;
    readonly supportedOperations?: ReadonlyArray<string>;
    readonly responses: Record<
      string,
      { readonly ok: boolean; readonly result?: unknown; readonly error?: unknown }
    >;
  };
  readonly steps: ReadonlyArray<Step>;
}

function writeOutput(text: string) {
  const outFlag = process.argv.indexOf("--out");
  if (outFlag === -1) {
    process.stdout.write(text);
  } else {
    NodeFs.writeFileSync(process.argv[outFlag + 1]!, text);
  }
}

const RECORDED_HEADERS = [
  "content-type",
  "mcp-session-id",
  "mcp-protocol-version",
  "www-authenticate",
  "allow",
  "cache-control",
];

const program = (script: Script, mode: "run" | "tools") =>
  Effect.gen(function* () {
    const commands: Array<OrchestrationCommand> = [];
    const hostRequests: Array<unknown> = [];
    const threadId = ThreadId.make(script.threadId);

    const dependencies = Layer.mergeAll(
      Layer.mock(ProjectionSnapshotQuery)({
        getThreadShellById: (id) =>
          Effect.succeed(
            id === threadId && script.thread !== null
              ? Option.some(script.thread as never)
              : Option.none(),
          ),
        getProjectShellById: () =>
          Effect.succeed(
            script.project === null ? Option.none() : Option.some(script.project as never),
          ),
      }),
      Layer.mock(OrchestrationEngineService)({
        readEvents: () => Stream.empty,
        streamDomainEvents: Stream.empty,
        latestSequence: Effect.succeed(0),
        dispatch: (command) =>
          Effect.gen(function* () {
            const rejected =
              (command.type === "thread.pull-request.link" &&
                script.rejectLink.includes(command.number)) ||
              (command.type === "thread.pull-request.unlink" &&
                script.rejectUnlink.includes(command.number));
            if (rejected) {
              return yield* new OrchestrationCommandInvariantError({
                commandType: command.type,
                detail: "scripted rejection",
              });
            }
            commands.push(command);
            return { sequence: commands.length };
          }),
      }),
      Layer.mock(DeviceService.DeviceService)({}),
      Layer.succeed(
        ServerEnvironment.ServerEnvironment,
        ServerEnvironment.ServerEnvironment.of({
          getEnvironmentId: Effect.succeed(EnvironmentId.make(script.environmentId)),
          getDescriptor: Effect.die("unused"),
        }),
      ),
    );

    const app = McpHttpServer.layer.pipe(
      Layer.provideMerge(PreviewAutomationBroker.layer),
      Layer.provideMerge(McpSessionRegistry.layer),
      Layer.provide(dependencies),
      Layer.provide(ServerConfig.layerTest(process.cwd(), { prefix: "t3-mcp-oracle-" })),
    );
    const context = yield* HttpRouter.serve(app, { disableListenLog: true, disableLogger: true }).pipe(
      Layer.provideMerge(
        NodeHttpServer.layer(NodeHttp.createServer, { port: 0, host: "127.0.0.1" }),
      ),
      Layer.build,
    );
    const server = Context.get(context, HttpServer.HttpServer);
    const address = server.address as { readonly port: number };
    const url = `http://127.0.0.1:${address.port}/mcp`;

    // Issuing revokes the thread's earlier credential (`issueActiveMcpCredential`), so a
    // credential is issued when a step first needs it and stays valid until the next one.
    let currentCredential: string | undefined;
    let currentHeader = "";
    const tokenFor = (name: string) =>
      Effect.gen(function* () {
        if (name === "none") return "";
        if (name === "bad") return "Bearer not-a-real-token";
        const capabilities = script.credentials[name];
        if (capabilities === undefined) throw new Error(`unknown credential ${name}`);
        if (currentCredential !== name) {
          const issued = yield* McpSessionRegistry.issueActiveMcpCredential({
            threadId,
            providerInstanceId: ProviderInstanceId.make("codex"),
            capabilities: new Set(capabilities),
          });
          currentCredential = name;
          currentHeader = issued!.config.authorizationHeader;
        }
        return currentHeader;
      });

    if (script.host) {
      const broker = Context.get(context, PreviewAutomationBroker.PreviewAutomationBroker);
      const host = script.host;
      const events = yield* broker.connect({
        clientId: host.clientId,
        environmentId: EnvironmentId.make(script.environmentId),
        ...(host.supportedOperations
          ? { supportedOperations: host.supportedOperations as never }
          : {}),
      });
      yield* Stream.runForEach(events, (event) => {
        if (event.type === "connected") return Effect.void;
        hostRequests.push({ ...event.request, requestId: "<request>" });
        const response = host.responses[event.request.operation] ?? {
          ok: false,
          error: { _tag: "PreviewAutomationExecutionError", message: "unscripted" },
        };
        return broker
          .respond({
            clientId: host.clientId,
            connectionId: event.connectionId,
            requestId: event.request.requestId,
            ...(response as { ok: boolean }),
          })
          .pipe(Effect.ignore);
      }).pipe(Effect.forkScoped);
      yield* Effect.sleep("20 millis");
    }

    let session: string | undefined;
    const transcript: Array<unknown> = [];
    const steps: ReadonlyArray<Step> =
      mode === "tools"
        ? [
            {
              name: "initialize",
              method: "POST",
              token: Object.keys(script.credentials)[0]!,
              headers: {
                "content-type": "application/json",
                accept: "application/json, text/event-stream",
              },
              body: '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"oracle","version":"1.0.0"}}}',
            },
            {
              name: "tools/list",
              method: "POST",
              token: Object.keys(script.credentials)[0]!,
              headers: {
                "content-type": "application/json",
                accept: "application/json, text/event-stream",
                "mcp-session-id": "$session",
                "mcp-protocol-version": "2025-06-18",
              },
              body: '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}',
            },
          ]
        : script.steps;
    for (const step of steps) {
      const headers: Record<string, string> = {};
      for (const [key, value] of Object.entries(step.headers ?? {})) {
        headers[key] = value === "$session" ? (session ?? "missing-session") : value;
      }
      const token = yield* tokenFor(step.token);
      if (token.length > 0) headers.authorization = token;
      const response = yield* Effect.promise(() =>
        fetch(url, {
          method: step.method,
          headers,
          ...(step.body === undefined ? {} : { body: step.body }),
        }),
      );
      const text = yield* Effect.promise(() => response.text());
      const recorded: Record<string, string> = {};
      for (const name of RECORDED_HEADERS) {
        const value = response.headers.get(name);
        if (value === null) continue;
        if (name === "mcp-session-id") {
          session = value;
          recorded[name] = "<session>";
        } else {
          recorded[name] = value;
        }
      }
      let body: unknown = text;
      try {
        body = text.length === 0 ? null : JSON.parse(text);
      } catch {
        // keep the text
      }
      // The descriptors are compared on their own (`tools`); a run keeps their names.
      const result = (body as { result?: { tools?: unknown } } | null)?.result;
      if (mode === "run" && result && Array.isArray(result.tools)) {
        result.tools = result.tools.map((tool: { name: string }) => tool.name);
      }
      transcript.push({ name: step.name, status: response.status, headers: recorded, body });
    }

    if (mode === "tools") {
      const list = transcript[1] as { body: { result: { tools: unknown } } };
      // The two hand-registered image tools publish no output schema; their results are
      // still encoded with these, which decides which keys survive and in what order.
      const successSchemas = {
        preview_snapshot: Tool.getJsonSchemaFromSchema(PreviewSnapshotTool.successSchema),
        device_screenshot: Tool.getJsonSchemaFromSchema(DeviceScreenshotTool.successSchema),
      };
      return JSON.stringify({ tools: list.body.result.tools, successSchemas }, null, 2) + "\n";
    }
    const normalizedCommands = commands.map((command) => ({
      ...command,
      commandId: String(command.commandId).replace(
        /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/,
        "<uuid>",
      ),
    }));
    return (
      JSON.stringify(
        { steps: transcript, commands: normalizedCommands, hostRequests },
        null,
        2,
      ) + "\n"
    );
  });

const [mode, scriptPath] = process.argv.slice(2);
if (mode !== "run" && mode !== "tools") {
  process.stderr.write("usage: mcp-oracle.ts run <script.json> | tools\n");
  process.exit(2);
}
const script: Script =
  mode === "run"
    ? JSON.parse(NodeFs.readFileSync(scriptPath!, "utf8"))
    : {
        environmentId: "environment-oracle",
        threadId: "thread-oracle",
        thread: null,
        project: null,
        rejectLink: [],
        rejectUnlink: [],
        credentials: { all: ["preview", "device"] },
        steps: [],
      };

const output = await Effect.runPromise(
  program(script, mode).pipe(Effect.scoped, Effect.provide(NodeServices.layer)),
);
writeOutput(output);
process.exit(0);
