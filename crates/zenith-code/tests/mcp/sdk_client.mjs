// The official MCP TypeScript SDK client (the one Claude Code builds on) against the Rust
// `/mcp`: connect with a registry-issued bearer credential, list the tools, link a pull
// request, list the thread's pull requests, close the session. Prints one JSON report.
//
//   MCP_SDK_DIR=<.../node_modules/@modelcontextprotocol/sdk> MCP_URL=http://127.0.0.1:<port>/mcp \
//   MCP_AUTHORIZATION="Bearer <token>" node sdk_client.mjs
import { pathToFileURL } from "node:url";
import { join } from "node:path";

const sdk = process.env.MCP_SDK_DIR;
const { Client } = await import(pathToFileURL(join(sdk, "dist/esm/client/index.js")).href);
const { StreamableHTTPClientTransport } = await import(
  pathToFileURL(join(sdk, "dist/esm/client/streamableHttp.js")).href
);

const transport = new StreamableHTTPClientTransport(new URL(process.env.MCP_URL), {
  requestInit: { headers: { Authorization: process.env.MCP_AUTHORIZATION } },
});
const client = new Client({ name: "zc-mcp-gate", version: "1.0.0" });
await client.connect(transport);

const report = {
  server: client.getServerVersion(),
  capabilities: client.getServerCapabilities(),
  sessionId: transport.sessionId ?? null,
};
const { tools } = await client.listTools();
report.tools = tools.map((tool) => tool.name);
report.linkDescription = tools.find((tool) => tool.name === "link_pull_request")?.description;
report.link = await client.callTool({
  name: "link_pull_request",
  arguments: { url: "https://github.com/acme/widgets/pull/42" },
});
report.list = await client.callTool({ name: "list_thread_pull_requests", arguments: {} });
report.preview = await client.callTool({ name: "preview_status", arguments: {} });
try {
  await client.callTool({ name: "link_pull_request", arguments: { number: 0 } });
  report.invalid = null;
} catch (error) {
  report.invalid = { code: error.code, message: error.message };
}
await transport.terminateSession();
report.terminated = transport.sessionId ?? null;
await client.close();
process.stdout.write(JSON.stringify(report));
