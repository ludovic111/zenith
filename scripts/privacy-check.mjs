#!/usr/bin/env node
// Makes sure none of your data ends up in a commit.
//
//   npm run privacy                 scan every file git tracks (working tree)
//   npm run privacy -- --install    also run it before each `git push` (pre-push hook)
//
// It looks, in every tracked text file, for each identifying value of your config
// (names, emails, handles, domains, repositories, service ids, city, coordinates…),
// for the terms of perso/denylist.txt, and for common secret formats. One hit fails.
// perso/allowlist.txt lists words too common to block (a project called "Portfolio")
// or phrases allowed as is (your public repository, "you/zenith").

import { execFileSync } from "node:child_process";
import { chmodSync, existsSync, lstatSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";

const ROOT = path.resolve(import.meta.dirname, "..");
const PERSO = path.join(ROOT, "perso");
const git = (...a) => execFileSync("git", a, { cwd: ROOT, encoding: "utf8", maxBuffer: 1 << 30 });
const say = (s) => console.log(`\x1b[1;33m✦\x1b[0m ${s}`);

if (process.argv.includes("--install")) {
  const hooks = path.resolve(ROOT, git("rev-parse", "--git-common-dir").trim(), "hooks");
  mkdirSync(hooks, { recursive: true });
  const hook = path.join(hooks, "pre-push");
  writeFileSync(hook, `#!/bin/sh\n# zenith: never push personal data.\nexec node "$(git rev-parse --show-toplevel)/scripts/privacy-check.mjs"\n`);
  chmodSync(hook, 0o755);
  say(`pre-push hook installed (${path.relative(ROOT, hook)})`);
}

const configFile =
  process.env.ZENITH_CONFIG ?? [path.join(PERSO, "zenith.config.json"), path.join(ROOT, "zenith.config.json")].find((f) => existsSync(f));

/** Identifying values of the config. */
function personalTerms() {
  if (!configFile || !existsSync(configFile)) return [];
  const c = JSON.parse(readFileSync(configFile, "utf8"));
  const terms = new Set();
  const add = (v) => {
    if (typeof v !== "string") return;
    const s = v.trim();
    if (s.length >= 4) terms.add(s);
  };
  // Identifiers only: a value with spaces is often a plain phrase ("read only").
  const id = (v) => typeof v === "string" && !/\s/.test(v.trim()) && add(v);
  const host = (u) => {
    try {
      return new URL(u).hostname.replace(/^www\./, "");
    } catch {
      return null;
    }
  };
  const handle = (h) => add(String(h ?? "").replace(/^@/, ""));
  const COMMON = /^(github\.com|railway\.com|apps\.apple\.com|appstoreconnect\.apple\.com|play\.google\.com|supabase\.com|openrouter\.ai|app\.revenuecat\.com|x\.com|instagram\.com|youtube\.com|tiktok\.com)$/;
  add(c.owner?.name);
  for (const w of String(c.owner?.name ?? "").split(/\s+/)) add(w);
  add(c.owner?.firstName);
  add(c.owner?.sponsors);
  for (const e of c.owner?.emails ?? []) add(e.address);
  for (const s of c.owner?.socials ?? []) handle(s.handle);
  for (const a of c.owner?.accounts ?? []) id(a.value);
  add(c.location?.name);
  if (c.location) for (const n of [c.location.latitude, c.location.longitude]) add(String(n));
  add(c.transit?.stop);
  for (const s of c.water?.stations ?? []) {
    add(s.name);
    add(s.water);
  }
  for (const w of c.watch ?? []) add(w.term);
  for (const n of c.news ?? []) add(host(n.url));
  for (const p of c.projects ?? []) {
    add(p.name);
    add(p.repo);
    add(p.repo?.split("/")[0]);
    add(host(p.site));
    for (const pr of p.probes ?? []) add(host(pr.url));
    for (const l of p.links ?? []) if (host(l.url) && !COMMON.test(host(l.url))) add(host(l.url));
    if (p.railway) Object.values(p.railway).forEach(add);
    add(p.appStore?.id);
    add(p.revenuecat?.projectId);
    const ident = p.identity ?? {};
    for (const d of ident.domains ?? []) add(d);
    for (const e of ident.emails ?? []) add(e.address);
    for (const s of ident.socials ?? []) handle(s.handle);
    for (const f of [...(ident.ids ?? []), ...(ident.names ?? [])]) if (f.mono) id(f.value);
    for (const s of [...(ident.services ?? []), ...(ident.stores ?? [])]) id(s.value);
  }
  return [...terms];
}

const listFile = (f) =>
  existsSync(f)
    ? readFileSync(f, "utf8")
        .split("\n")
        .map((l) => l.trim())
        .filter((l) => l && !l.startsWith("#"))
    : [];

const esc = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
const allowLines = listFile(path.join(PERSO, "allowlist.txt"));
const allow = new Set(allowLines.map((s) => s.toLowerCase()));
const allowed = allowLines.map((l) => new RegExp(esc(l), "gi"));
const terms = [...new Set([...personalTerms(), ...listFile(path.join(PERSO, "denylist.txt"))])].filter((t) => !allow.has(t.toLowerCase()));
const termRes = terms.map((t) => ({ t, re: new RegExp(`(^|[^\\p{L}\\p{N}])${esc(t)}($|[^\\p{L}\\p{N}])`, "iu") }));
const SECRETS = [
  [/-----BEGIN [A-Z ]*PRIVATE KEY-----/, "private key"],
  [/\bsk_(live|test)_[A-Za-z0-9]{10,}/, "Stripe/RevenueCat key"],
  [/\bsk-ant-[A-Za-z0-9_-]{20,}/, "Anthropic key"],
  [/\bsk-(proj|or-v1)-[A-Za-z0-9_-]{20,}/, "OpenAI/OpenRouter key"],
  [/\bsb_secret_[A-Za-z0-9_-]{10,}/, "Supabase key"],
  [/\bgh[pousr]_[A-Za-z0-9]{30,}/, "GitHub token"],
  [/\bgithub_pat_[A-Za-z0-9_]{30,}/, "GitHub token"],
  [/\bAKIA[0-9A-Z]{16}\b/, "AWS key"],
  [/\bxox[abpr]-[A-Za-z0-9-]{10,}/, "Slack token"],
];
// Your home folder path (/Users/<you>/…) gives your login away.
const home = process.env.HOME ?? "";
if (home.length > 6) SECRETS.push([new RegExp(esc(home) + "(/|$)"), "path of your home folder"]);

const files = git("ls-files", "-z").split("\0").filter(Boolean);
const hits = [];
for (const f of files) {
  const abs = path.join(ROOT, f);
  if (!existsSync(abs) || !lstatSync(abs).isFile()) continue;
  const buf = readFileSync(abs);
  if (buf.includes(0)) continue; // binary
  if (buf.length > 5e6) {
    if (buf.length > 50e6) hits.push(`${f}  ${(buf.length / 1e6).toFixed(0)} MB file`);
    continue;
  }
  const text = allowed.reduce((t, re) => t.replace(re, " "), buf.toString("utf8"));
  const lines = text.split("\n");
  for (const { t, re } of termRes)
    if (re.test(text)) {
      const i = lines.findIndex((l) => re.test(l));
      hits.push(`${f}:${i + 1}  "${t}"  ${lines[i].trim().slice(0, 140)}`);
    }
  // Test fixtures carry fake keys on purpose.
  for (const [re, what] of /\.(test|spec)\.[cm]?[jt]sx?$/.test(f) ? [] : SECRETS)
    if (re.test(text)) {
      const i = lines.findIndex((l) => re.test(l));
      hits.push(`${f}:${i + 1}  ${what}`);
    }
}

say(`${terms.length} personal values${configFile ? ` from ${path.relative(ROOT, configFile)}` : " (no config found)"} and ${SECRETS.length} secret formats checked in ${files.length} tracked files`);
if (hits.length) {
  console.error(hits.map((h) => `  ${h}`).join("\n"));
  console.error(`\x1b[1;31m✗ ${hits.length} possible leak(s). Fix the code, or add a false positive to perso/allowlist.txt.\x1b[0m`);
  process.exit(1);
}
say("No personal data found.");
