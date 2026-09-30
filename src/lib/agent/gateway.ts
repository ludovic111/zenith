import "server-only";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { config } from "../config";
import { tr } from "../i18n";
import { codeStatus } from "../code/manager";
import { ask, followUp } from "./ask";
import { underLimit } from "./auth";
import { serial, writeJson } from "./files";
import { selfOrigin } from "./workspace";
import { LIFE, mention } from "./target";
import { waitForTurn } from "./talk";

/**
 * The gateway: talk to your agent from your phone, through a Telegram bot of your own
 * (TELEGRAM_BOT_TOKEN, from @BotFather). Only the chats listed in
 * `agent.gateway.telegram.chats` are heard; anyone else is told their chat id and nothing
 * more. A message starts a conversation with your agent (or continues the one from the
 * last two hours; /new starts over), and its answer comes back when the turn ends. When
 * the agent needs an approval, you get the link to give it in zenith.
 *
 * Your phone's words travel through Telegram, so they count as outside words: the agent
 * runs at most in "auto", where Claude's and Codex's reviewers stop risky actions.
 */

const API = "https://api.telegram.org";
const FILE = path.join(process.cwd(), ".data", "gateway.json");
const CONTINUE_MS = 2 * 3600e3;
const WATCH_MS = 30 * 60e3;
const MAX_TEXT = 4000;

type State = { offset: number; chats: Record<string, { threadId: string; environmentId: string; at: string }> };
type Update = { update_id: number; message?: { chat: { id: number }; text?: string; from?: { is_bot?: boolean } } };

const token = () => process.env.TELEGRAM_BOT_TOKEN?.trim() || "";
const settings = () => config().agent.gateway.telegram;
const allowed = (chat: number) => (settings()?.chats ?? []).some((c) => String(c) === String(chat));

async function state(): Promise<State> {
  try {
    const s = JSON.parse(await readFile(FILE, "utf8")) as State;
    return { offset: s.offset ?? 0, chats: s.chats ?? {} };
  } catch {
    return { offset: 0, chats: {} };
  }
}
const save = (fn: (s: State) => void) =>
  serial(FILE, async () => {
    const s = await state();
    fn(s);
    await writeJson(FILE, s);
  });

async function telegram<T>(method: string, body: Record<string, unknown>, timeoutMs = 15_000): Promise<T> {
  const res = await fetch(`${API}/bot${token()}/${method}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(timeoutMs),
  });
  const data = (await res.json().catch(() => ({}))) as { ok?: boolean; result?: T; description?: string };
  if (!data.ok) throw Object.assign(new Error(`telegram ${method}: ${data.description ?? res.status}`), { status: res.status });
  return data.result as T;
}

/** Sends text in pieces Telegram accepts. */
async function say(chat: number, text: string) {
  const clean = text.trim() || "…";
  for (let i = 0; i < clean.length; i += MAX_TEXT) await telegram("sendMessage", { chat_id: chat, text: clean.slice(i, i + MAX_TEXT) });
}

const link = (environmentId: string, threadId: string) => `${selfOrigin()}/code/${encodeURIComponent(environmentId)}/${encodeURIComponent(threadId)}`;

/** Waits for the turn started at `since` to end (or to need you), then sends its answer. */
async function relay(chat: number, threadId: string, environmentId: string, since: number) {
  let typing = 0;
  const end = await waitForTurn(threadId, since, WATCH_MS, () => {
    if (Date.now() - typing < 4500) return;
    typing = Date.now();
    telegram("sendChatAction", { chat_id: chat, action: "typing" }).catch(() => {});
  });
  const said = end.text ? `${end.text}\n\n` : "";
  if (end.state === "completed") return say(chat, end.text || tr("C'est fait.", "Done."));
  if (end.state === "needs-you") return say(chat, `${said}${tr("⏸ J'ai besoin de ton accord pour continuer, dans zenith sur ton Mac :", "⏸ I need your go-ahead to continue, in zenith on your Mac:")} ${link(environmentId, threadId)}`);
  if (end.state === "timeout") return say(chat, tr(`Toujours au travail. Suis-moi ici : ${link(environmentId, threadId)}`, `Still working. Follow along here: ${link(environmentId, threadId)}`));
  return say(chat, `${said}${tr("⚠︎ Je me suis arrêté en route", "⚠︎ I stopped on the way")}${end.error ? ` : ${end.error}` : ""}. ${link(environmentId, threadId)}`);
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

const told = new Map<number, number>();

async function handle(u: Update) {
  const m = u.message;
  if (!m?.text || m.from?.is_bot) return;
  const chat = m.chat.id;
  const text = m.text.trim();
  if (!allowed(chat)) {
    // Say once an hour who they are, so the owner can allow their own chat; nothing else.
    if (Date.now() - (told.get(chat) ?? 0) > 3600e3) {
      told.set(chat, Date.now());
      await say(chat, tr(`Ce chat n'est pas autorisé. Son id : ${chat} (à mettre dans agent.gateway.telegram.chats).`, `This chat isn't allowed. Its id: ${chat} (add it to agent.gateway.telegram.chats).`));
    }
    return;
  }
  if (/^\/(new|start)\b/.test(text)) {
    await save((s) => void delete s.chats[String(chat)]);
    if (/^\/start\b/.test(text)) await say(chat, tr("Bonjour ! Dis-moi ce que tu veux ; /new pour repartir de zéro.", "Hi! Tell me what you want; /new to start over."));
    else await say(chat, tr("Nouvelle conversation.", "New conversation."));
    return;
  }
  if (!underLimit("gateway", 30)) return say(chat, tr("Trop de demandes cette heure-ci.", "Too many requests this hour."));
  if (!codeStatus().running) return say(chat, tr("zenith code ne tourne pas sur le Mac : je ne peux rien lancer.", "zenith code isn't running on the Mac: I can't start anything."));

  const since = Date.now();
  const current = (await state()).chats[String(chat)];
  let thread: { threadId: string; environmentId: string } | null = null;
  if (current && since - Date.parse(current.at) < CONTINUE_MS) {
    try {
      await followUp(current.threadId, text);
      thread = current;
    } catch {
      thread = null; // gone or busy: start a new one
    }
  }
  if (!thread) {
    const r = await ask({ prompt: text, target: mention(text) ? undefined : settings()?.target ?? LIFE, source: "gateway" });
    thread = { threadId: r.threadId, environmentId: r.environmentId };
  }
  await save((s) => void (s.chats[String(chat)] = { ...thread!, at: new Date().toISOString() }));
  await relay(chat, thread.threadId, thread.environmentId, since);
}

const g = globalThis as { __zenithGateway?: boolean };

async function loop() {
  let s = await state();
  let backoff = 1000;
  for (;;) {
    if (!token()) return void (g.__zenithGateway = false);
    try {
      const updates = await telegram<Update[]>("getUpdates", { offset: s.offset, timeout: 50, allowed_updates: ["message"] }, 60_000);
      backoff = 1000;
      for (const u of updates) {
        s.offset = u.update_id + 1;
        await save((x) => void (x.offset = s.offset));
        handle(u).catch((e) => {
          console.error("[zenith] gateway:", e instanceof Error ? e.message : e);
          const chat = u.message?.chat.id;
          if (chat && allowed(chat)) say(chat, `${tr("Échec", "Failed")}: ${e instanceof Error ? e.message : String(e)}`).catch(() => {});
        });
      }
    } catch (e) {
      // 409: another zenith server (dev or the app) is already listening; wait our turn.
      const status = (e as { status?: number }).status;
      if (status === 401 || status === 404) {
        console.error("[zenith] gateway: TELEGRAM_BOT_TOKEN refused");
        return void (g.__zenithGateway = false);
      }
      await sleep(status === 409 ? 60_000 : backoff);
      backoff = Math.min(backoff * 2, 60_000);
      s = await state();
    }
  }
}

/** Starts listening when a token and at least one chat (or none yet, to learn its id) are set. */
export function startGateway() {
  if (g.__zenithGateway || !token() || !config().agent.enabled || !settings()) return;
  g.__zenithGateway = true;
  loop().catch((e) => {
    g.__zenithGateway = false;
    console.error("[zenith] gateway:", e);
  });
}

export const gatewayStatus = () => ({ configured: !!settings(), token: !!token(), running: !!g.__zenithGateway, chats: settings()?.chats.length ?? 0 });
