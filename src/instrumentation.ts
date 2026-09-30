export async function register() {
  if (process.env.NEXT_RUNTIME !== "nodejs") return;
  const { startMonitor } = await import("./lib/sources/uptime");
  startMonitor();

  // zenith code: the coding-agent workspace (code/), run as a child process on 127.0.0.1.
  const { startCodeServer } = await import("./lib/code/manager");
  startCodeServer();

  // The zenith agent: its team's folders, the token local agents act with, its routines and its gateway.
  const { config } = await import("./lib/config");
  if (config().agent.enabled) {
    const [{ ensureTeam }, { agentToken }, { startRoutines }, { startGateway }] = await Promise.all([
      import("./lib/agent/workspace"),
      import("./lib/agent/auth"),
      import("./lib/agent/routines"),
      import("./lib/agent/gateway"),
    ]);
    agentToken();
    ensureTeam().catch((e) => console.error("[zenith] agent folders:", e));
    startRoutines();
    startGateway();
  }

  // Context for agents: rewritten every 10 minutes into context/ and the Obsidian vault.
  const { writeContext } = await import("./lib/context");
  const run = () => writeContext().catch((e) => console.error("[zenith] context:", e));
  setTimeout(run, 20_000);
  setInterval(run, 10 * 60_000);
}
