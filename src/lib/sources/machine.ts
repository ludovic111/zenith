import "server-only";
import { execFile } from "node:child_process";
import { statfs } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { promisify } from "node:util";
import { cached } from "../source";
import { PROJECTS, PROJECTS_ROOT } from "../projects";
import { tr } from "../i18n";

const run = promisify(execFile);
const sh = (cmd: string, args: string[], timeout = 8000) => run(cmd, args, { timeout, maxBuffer: 4 << 20 }).then((r) => r.stdout).catch(() => "");

export type Machine = {
  model: string;
  chip: string;
  macos: string;
  memoryGB: number;
  memoryFree: number | null;
  load: number;
  cores: number;
  uptime: number;
  disk: { freeGB: number; totalGB: number };
  battery: { percent: number; charging: boolean; source: string } | null;
};

/** This Mac: chip, macOS, memory, load, disk, battery. Everything is read locally. */
export const machine = () =>
  cached("machine", 60, async (): Promise<Machine> => {
    const [model, chip, macos, pressure, batt, fsInfo] = await Promise.all([
      sh("sysctl", ["-n", "hw.model"]),
      sh("sysctl", ["-n", "machdep.cpu.brand_string"]),
      sh("sw_vers", ["-productVersion"]),
      sh("memory_pressure", ["-Q"]),
      sh("pmset", ["-g", "batt"]),
      statfs("/System/Volumes/Data").catch(() => statfs("/")),
    ]);
    const b = batt.match(/(\d+)%;\s*([^;]+);/);
    return {
      model: model.trim(),
      chip: chip.trim(),
      macos: macos.trim(),
      memoryGB: Math.round(os.totalmem() / 2 ** 30),
      memoryFree: Number(pressure.match(/free percentage:\s*(\d+)%/)?.[1] ?? NaN) || null,
      load: os.loadavg()[0],
      cores: os.cpus().length,
      uptime: os.uptime(),
      disk: { freeGB: (fsInfo.bavail * fsInfo.bsize) / 1e9, totalGB: (fsInfo.blocks * fsInfo.bsize) / 1e9 },
      battery: b ? { percent: Number(b[1]), charging: /charging|charged|finishing/.test(b[2]) && !/discharging/.test(b[2]), source: batt.includes("AC Power") ? tr("secteur", "AC power") : tr("batterie", "battery") } : null,
    };
  });

export type Outdated = { name: string; current: string; latest: string; cask: boolean };

/** Outdated Homebrew packages (without running `brew update`, so no network). */
export const brewOutdated = () =>
  cached("brew:outdated", 6 * 3600, async (): Promise<Outdated[] | null> => {
    const out = await sh("brew", ["outdated", "--json=v2"], 60000);
    if (!out) return null;
    const d = JSON.parse(out) as {
      formulae: { name: string; installed_versions: string[]; current_version: string }[];
      casks: { name: string; installed_versions: string | string[]; current_version: string }[];
    };
    return [
      ...d.formulae.map((f) => ({ name: f.name, current: f.installed_versions.at(-1) ?? "", latest: f.current_version, cask: false })),
      ...d.casks.map((c) => ({ name: c.name, current: String(([] as string[]).concat(c.installed_versions).at(-1) ?? ""), latest: c.current_version, cask: true })),
    ];
  });

export type DevServer = { port: number; command: string; project: string | null; dir: string | null };

/** Processes that look like dev servers: common runtimes, plus binaries named after one of your projects. */
const esc = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
const DEV = new RegExp(
  `^(node|bun|deno|python\\d*(\\.\\d+)?|ruby|php|java|cargo|next-serv|vite|uvicorn|gunicorn|go|air|hugo|caddy${PROJECTS.map((p) => `|${esc(p.id)}.*`).join("")})$`,
  "i",
);

/** Dev servers listening on this Mac, and the project they come from. */
export const devServers = () =>
  cached("dev:servers", 30, async (): Promise<DevServer[]> => {
    const out = await sh("lsof", ["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"]);
    const found = new Map<string, { pid: string; command: string; port: number }>();
    let pid = "";
    let command = "";
    for (const line of out.split("\n")) {
      if (line.startsWith("p")) pid = line.slice(1);
      else if (line.startsWith("c")) command = line.slice(1);
      else if (line.startsWith("n")) {
        const port = Number(line.match(/:(\d+)$/)?.[1]);
        if (port && DEV.test(command) && !found.has(`${pid}:${port}`)) found.set(`${pid}:${port}`, { pid, command, port });
      }
    }
    const rows = await Promise.all(
      [...found.values()].map(async (s) => {
        const cwd = (await sh("lsof", ["-a", "-p", s.pid, "-d", "cwd", "-Fn"])).split("\n").find((l) => l.startsWith("n"))?.slice(1) ?? null;
        const rel = cwd && cwd.startsWith(PROJECTS_ROOT + path.sep) ? path.relative(PROJECTS_ROOT, cwd) : null;
        return { port: s.port, command: s.command, dir: cwd, project: rel ? rel.split(path.sep)[0] : null };
      }),
    );
    // A server often listens on both IPv4 and IPv6: one port per process is enough.
    return rows.filter((r, i) => rows.findIndex((x) => x.port === r.port) === i).sort((a, b) => a.port - b.port);
  });
