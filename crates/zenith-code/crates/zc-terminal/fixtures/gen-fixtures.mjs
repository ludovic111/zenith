// Generates the byte-exact fixtures of zc-terminal from the TypeScript originals:
//
//   - decoder.json    Node's StringDecoder("utf8"), which node-pty uses on the PTY socket
//                     (`socket.setEncoding("utf8")`): byte chunks in, strings out.
//   - sanitizer.json  `sanitizeTerminalHistoryChunk` of apps/server/src/terminal/Manager.ts,
//                     fed with what the decoder produced, chunk by chunk.
//   - history.json    `BoundedTerminalHistory` of the same file under append / clear sequences.
//
// The functions are not exported by Manager.ts (and importing it would pull the whole server),
// so their source is cut out of the file between fixed markers, written to a temporary .ts
// module and imported (Node strips the types). Run from anywhere, with Node >= 23:
//
//   node crates/zenith-code/crates/zc-terminal/fixtures/gen-fixtures.mjs
//
// Deterministic: a fixed-seed PRNG drives every random case.

import { StringDecoder } from "node:string_decoder";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "../../../../..");
const managerPath = path.join(repo, "code/apps/server/src/terminal/Manager.ts");
const source = fs.readFileSync(managerPath, "utf8");

const cut = (from, to) => {
  const start = source.indexOf(from);
  const end = source.indexOf(to, start);
  if (start < 0 || end < 0) throw new Error(`markers not found: ${from} .. ${to}`);
  return source.slice(start, end);
};
const constant = (name) => {
  const match = new RegExp(`const ${name} = [^;]+;`).exec(source);
  if (!match) throw new Error(`constant ${name} not found`);
  return match[0];
};

const moduleSource = [
  constant("DEFAULT_HISTORY_BYTE_LIMIT"),
  constant("MAX_HISTORY_CHUNK_LENGTH"),
  cut("interface TerminalHistoryChunk", "function isCsiFinalByte"),
  cut("function isCsiFinalByte", "function legacySafeThreadId"),
  "export { sanitizeTerminalHistoryChunk };",
].join("\n");
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "zc-terminal-fixtures-"));
const modulePath = path.join(tmp, "extracted.ts");
fs.writeFileSync(modulePath, moduleSource);
const { BoundedTerminalHistory, sanitizeTerminalHistoryChunk } = await import(
  pathToFileURL(modulePath).href
);
fs.rmSync(tmp, { recursive: true, force: true });

// --- helpers ---------------------------------------------------------------------------------

let seed = 0x5eed2026;
const random = () => {
  seed = (Math.imul(seed, 1_664_525) + 1_013_904_223) >>> 0;
  return seed / 0x1_0000_0000;
};
const pick = (list) => list[Math.floor(random() * list.length)];
const hex = (bytes) => Buffer.from(bytes).toString("hex");
const utf8 = (text) => Buffer.from(text, "utf8");

/** Splits `bytes` at the given offsets. */
const splitAt = (bytes, offsets) => {
  const chunks = [];
  let start = 0;
  for (const offset of [...offsets].sort((a, b) => a - b)) {
    chunks.push(bytes.subarray(start, offset));
    start = offset;
  }
  chunks.push(bytes.subarray(start));
  return chunks;
};

/** Runs byte chunks through the decoder and the sanitizer, like the manager does. */
const runPipeline = (chunks) => {
  const decoder = new StringDecoder("utf8");
  let pending = "";
  const steps = [];
  for (const chunk of chunks) {
    const data = decoder.write(chunk);
    // The manager only sees non-empty data events (a socket never emits "").
    if (data.length === 0) {
      steps.push({ data, visible: "", pending });
      continue;
    }
    const result = sanitizeTerminalHistoryChunk(pending, data);
    pending = result.pendingControlSequence;
    steps.push({ data, visible: result.visibleText, pending });
  }
  return steps;
};

const fnv1a64 = (text) => {
  let hash = 0xcbf29ce484222325n;
  for (const byte of utf8(text)) {
    hash ^= BigInt(byte);
    hash = (hash * 0x100000001b3n) & 0xffffffffffffffffn;
  }
  return hash.toString(16).padStart(16, "0");
};

// --- decoder ---------------------------------------------------------------------------------

const decoderCases = [];
{
  const samples = [
    utf8("café 名 🚀 ok"),
    Buffer.from([0xc2, 0x9b, 0x3f, 0x33, 0x31, 0x75]), // C1 CSI ?31u
    Buffer.from([0x9b, 0x41, 0xff, 0xfe, 0x41]), // lone continuation / invalid bytes
    Buffer.from([0xe5, 0x90, 0x41, 0xf0, 0x9f, 0x98, 0x41]), // truncated sequences then ASCII
    Buffer.from([0xc0, 0xaf, 0xed, 0xa0, 0x80, 0xf4, 0x90, 0x80, 0x80, 0x41]), // overlong, surrogate, > U+10FFFF
    Buffer.from([0xe0, 0x80, 0x41, 0xe0, 0xa0, 0x80, 0xf8, 0x88, 0x80, 0x80, 0x80]),
    Buffer.from([0x80, 0x80, 0x80, 0x80, 0x80, 0xc3, 0xa9]),
    Buffer.from([0xf0, 0x9f, 0x98, 0x80, 0xf0, 0x9f]),
  ];
  for (const [index, bytes] of samples.entries()) {
    // Every single split point, plus byte-at-a-time.
    for (let offset = 0; offset <= bytes.length; offset++) {
      const chunks = splitAt(bytes, [offset]).filter((chunk) => chunk.length > 0);
      decoderCases.push({
        name: `sample ${index} split at ${offset}`,
        chunks: chunks.map(hex),
        outputs: (() => {
          const decoder = new StringDecoder("utf8");
          return chunks.map((chunk) => decoder.write(chunk));
        })(),
      });
    }
    const decoder = new StringDecoder("utf8");
    const bytewise = [...bytes].map((byte) => Buffer.from([byte]));
    decoderCases.push({
      name: `sample ${index} byte by byte`,
      chunks: bytewise.map(hex),
      outputs: bytewise.map((chunk) => decoder.write(chunk)),
    });
  }
  // Random byte soup, randomly split.
  const alphabet = [
    [0x41],
    [0x0a],
    [0x1b],
    [0x80],
    [0xbf],
    [0xc2],
    [0xc3, 0xa9],
    [0xe5, 0x90, 0x8d],
    [0xf0, 0x9f, 0x9a, 0x80],
    [0xed],
    [0xf4],
    [0xff],
    [0xe0],
    [0xa0],
    [0x9b],
  ];
  for (let index = 0; index < 300; index++) {
    const bytes = Buffer.from(Array.from({ length: 1 + Math.floor(random() * 24) }, () => pick(alphabet)).flat());
    const offsets = Array.from({ length: Math.floor(random() * 5) }, () => Math.floor(random() * (bytes.length + 1)));
    const chunks = splitAt(bytes, offsets).filter((chunk) => chunk.length > 0);
    const decoder = new StringDecoder("utf8");
    decoderCases.push({
      name: `random ${index}`,
      chunks: chunks.map(hex),
      outputs: chunks.map((chunk) => decoder.write(chunk)),
    });
  }
}

// --- sanitizer -------------------------------------------------------------------------------

const sequences = {
  // Device attributes (DA1 / DA2 / DA3) queries and replies.
  "DA1 query": "\u001b[c",
  "DA1 query 0": "\u001b[0c",
  "DA2 query": "\u001b[>c",
  "DA2 query 0": "\u001b[>0c",
  "DA1 reply": "\u001b[?1;2c",
  "DA2 reply": "\u001b[>0;276;0c",
  "DA3 query": "\u001b[=c",
  // Device status / cursor position.
  "DSR status": "\u001b[5n",
  "DSR CPR query": "\u001b[6n",
  "DSR private": "\u001b[?6n",
  "DSR reply": "\u001b[0n",
  "CPR reply": "\u001b[12;40R",
  "CPR private reply": "\u001b[?12;40R",
  "CPR with spaces": "\u001b[12; 40R",
  // DECRQM / DECRPM, and setters sharing the final byte.
  "DECRQM private": "\u001b[?2026$p",
  "DECRQM ansi": "\u001b[4$p",
  "DECRPM reply": "\u001b[?2026;2$y",
  "DECRPM ansi reply": "\u001b[4;1$y",
  "DECSTR setter": "\u001b[!p",
  "DECSCL setter": '\u001b[62;1"p',
  "DECSCL short": '\u001b["p',
  // XTVERSION and DECSCUSR.
  "XTVERSION": "\u001b[>q",
  "XTVERSION 0": "\u001b[>0q",
  "XTVERSION reply (DCS)": "\u001bP>|XTerm(390)\u001b\\",
  "DECSCUSR": "\u001b[4 q",
  "DECLL": "\u001b[1q",
  // Kitty keyboard protocol.
  "kitty query": "\u001b[?u",
  "kitty reply": "\u001b[?31u",
  "restore cursor": "\u001b[u",
  "kitty push": "\u001b[>1u",
  "kitty pop": "\u001b[<u",
  "kitty set": "\u001b[=1;1u",
  // DCS request / reply traffic.
  "DECRQSS": "\u001bP$qm\u001b\\",
  "DECRQSS space": "\u001bP$q m\u001b\\",
  "DECRQSS reply": "\u001bP1$r0m\u001b\\",
  "DECRQSS invalid reply": "\u001bP0$r\u001b\\",
  "DECRQSS BEL": "\u001bP$qm\u0007",
  "XTGETTCAP": "\u001bP+q544e\u001b\\",
  "XTGETTCAP reply": "\u001bP1+r544e=787465726d\u001b\\",
  "sixel": "\u001bPq#0;2;0;0;0~-\u001b\\",
  "DCS tmux passthrough": "\u001bPtmux;\u001b\u001b]0;x\u0007\u001b\\",
  "DECRQSS 8-bit": "\u0090$q m\u009c",
  "DECRQSS reply 8-bit": "\u00901$r0m\u009c",
  "XTGETTCAP 8-bit": "\u0090+q544e\u009c",
  "XTGETTCAP reply 8-bit": "\u00901+r544e=1b\u009c",
  // OSC 10 / 11 / 12 color queries and replies, and OSCs that stay.
  "OSC 10 query": "\u001b]10;?\u0007",
  "OSC 11 query ST": "\u001b]11;?\u001b\\",
  "OSC 12 query": "\u001b]12;?\u0007",
  "OSC 11 reply": "\u001b]11;rgb:ffff/ffff/ffff\u0007",
  "OSC 10 reply ST": "\u001b]10;rgb:0000/0000/0000\u001b\\",
  "OSC 11 query 8-bit": "\u009d11;?\u009c",
  "OSC 11 set": "\u001b]11;#ffffff\u0007",
  "OSC 0 title": "\u001b]0;title\u0007",
  "OSC 1": "\u001b]1;x\u0007",
  "OSC 4 query": "\u001b]4;1;?\u0007",
  "OSC 8 link": "\u001b]8;;https://example.com\u001b\\link\u001b]8;;\u001b\\",
  "OSC 110 reset": "\u001b]110\u0007",
  "OSC 112": "\u001b]112\u0007",
  "OSC 133": "\u001b]133;A\u0007",
  // CSI that stays.
  "SGR": "\u001b[31;1m",
  "SGR reset": "\u001b[0m",
  "home clear": "\u001b[H\u001b[2J",
  "hide cursor": "\u001b[?25l",
  "alt screen": "\u001b[?1049h",
  "CSI with ESC in body": "\u001b[1\u001b[2m",
  "C1 CSI SGR": "\u009b32m",
  "C1 CSI kitty": "\u009b?31u",
  "C1 CSI DSR": "\u009b6n",
  // ESC sequences.
  "charset": "\u001b(B",
  "charset G1": "\u001b)0",
  "DECALN": "\u001b#8",
  "UTF-8 select": "\u001b%G",
  "ESC intermediates then control": "\u001b(\n",
  "ESC then multibyte": "\u001bé",
  "ESC then emoji": "\u001b🚀",
  "DECSC": "\u001b7",
  "DECRC": "\u001b8",
  "DECKPAM": "\u001b=",
  "RIS": "\u001bc",
  "RI": "\u001bM",
  "ESC ESC": "\u001b\u001b[0m",
  "ESC X": "\u001bXsos\u001b\\",
  // APC / PM strings.
  "kitty graphics APC": "\u001b_Gf=24;AAAA\u001b\\",
  "PM": "\u001b^privacy\u001b\\",
  "APC 8-bit": "\u009fapc\u009c",
  "PM 8-bit": "\u009epm\u0007",
};

const sanitizerCases = [];
const addCase = (name, text, offsetsList) => {
  const bytes = utf8(text);
  for (const offsets of offsetsList) {
    const chunks = splitAt(bytes, offsets).filter((chunk) => chunk.length > 0);
    sanitizerCases.push({
      name: `${name} [${offsets.join(",")}]`,
      chunks: chunks.map(hex),
      steps: runPipeline(chunks),
    });
  }
};

for (const [name, sequence] of Object.entries(sequences)) {
  const text = `a${sequence}z`;
  const length = utf8(text).length;
  // Whole, then every 2-way split (byte offsets, so UTF-8 can split too).
  const offsetsList = [[]];
  for (let offset = 1; offset < length; offset++) offsetsList.push([offset]);
  addCase(name, text, offsetsList);
  // Byte by byte.
  addCase(`${name} bytewise`, text, [Array.from({ length: length - 1 }, (_, i) => i + 1)]);
}

// Truncated sequences left pending at the end.
for (const [name, text] of Object.entries({
  "trailing ESC": "abc\u001b",
  "trailing CSI": "abc\u001b[12;",
  "trailing OSC": "abc\u001b]11;rgb:ff",
  "trailing DCS ESC": "abc\u001bP$qm\u001b",
  "trailing C1 CSI": "abc\u009b?3",
  "trailing C1 OSC": "abc\u009d11;?",
  "trailing ESC intermediates": "abc\u001b((",
})) {
  addCase(name, text, [[]]);
}

// The cases of Manager.test.ts, chunk for chunk.
const textCases = {
  "manager: query and reply": ["prompt ", "\u001b[32mok\u001b[0m ", "\u001b]11;rgb:ffff/ffff/ffff\u0007", "\u001b[1;1R", "done\n"],
  "manager: CSI and DCS with setters": [
    "prompt ",
    "\u001b[?2026$p\u001b[?2026;2$y\u001b[>q\u001b[?u\u001b[?31u",
    "\u001bP$q m\u001b\\\u001bP1$r0m\u001b\\",
    "\u001bP+q544e\u001b\\\u001bP1+r544e=1b\u001b\\",
    "\u0090$q m\u009c\u00901$r0m\u009c",
    "\u0090+q544e\u009c\u00901+r544e=1b\u009c",
    '\u001b[!p\u001b["p\u001b[4 q\u001b[u',
    "done\n",
  ],
  "manager: split queries": [
    "before ",
    "\u001b[?2026$",
    "pafter ",
    "\u001bP$q ",
    "m\u001b",
    "\\after ",
    "\u009b?3",
    "1uafter ",
    "\u0090+q544e",
    "\u009cafter\n",
  ],
  "manager: clear and style": [
    "before clear\n",
    "\u001b[H\u001b[2J",
    "prompt ",
    "\u001b]11;",
    "rgb:ffff/ffff/ffff\u0007\u001b[1;1",
    "R\u001b[36mdone\u001b[0m\n",
  ],
  "manager: ESC intermediate": ["before ", "\u001b(B", "after\n"],
  "manager: ESC intermediate split": ["before ", "\u001b(", "Bafter\n"],
};
for (const [name, texts] of Object.entries(textCases)) {
  const chunks = texts.map(utf8);
  sanitizerCases.push({ name, chunks: chunks.map(hex), steps: runPipeline(chunks) });
}

// Random mixtures of every sequence with text and multibyte characters, split at random
// byte offsets.
{
  const parts = [...Object.values(sequences), "plain ", "\n", "\r\n", "é", "名", "🚀", "\u001b", "\u001b[", "\u009b", "\u009c", "\u0007"];
  for (let index = 0; index < 400; index++) {
    const text = Array.from({ length: 1 + Math.floor(random() * 10) }, () => pick(parts)).join("");
    const length = utf8(text).length;
    const offsets = Array.from({ length: Math.floor(random() * 6) }, () => Math.floor(random() * (length + 1)));
    addCase(`random ${index}`, text, [offsets]);
  }
}

// --- history ---------------------------------------------------------------------------------

const historyCases = [];
{
  const fragments = ["", "a", "\n", "\n\n", "\r", "\r\n", "café", "名", "🚀", "\u001b[31m", "\u001b[0m", "\u001b]8;;url\u0007", "line\n", "\uFEFF"];
  const record = (value) =>
    value.length <= 512 ? { value } : { bytes: utf8(value).length, fnv1a64: fnv1a64(value), head: value.slice(0, 32), tail: value.slice(-32) };
  for (const maxBytes of [0, 3, 8, 64, 1000, null]) {
    for (const maxLines of [0, 1, 3, 5, 5000]) {
      const initial = "before\ninitial\n";
      const history =
        maxBytes === null
          ? new BoundedTerminalHistory(maxLines, initial)
          : new BoundedTerminalHistory(maxLines, initial, maxBytes);
      const ops = [];
      const expected = [record(history.value())];
      for (let step = 0; step < 120; step++) {
        if (step % 37 === 36) {
          history.clear();
          ops.push({ clear: true });
        } else {
          const text = pick(fragments) + pick(fragments) + pick(fragments);
          history.append(text);
          ops.push({ append: [[text, 1]] });
        }
        expected.push(record(history.value()));
      }
      historyCases.push({ name: `fuzz lines=${maxLines} bytes=${maxBytes}`, maxLines, maxBytes, initial, ops, expected });
    }
  }
  // Long appends crossing the 16 Ki code unit chunk size, with multibyte characters at the
  // boundaries, and line trimming over compacted storage.
  const big = [
    { maxLines: 5000, maxBytes: 65539, ops: [[["a", 16383], ["😀", 1], ["b", 70000]], [["\r", 1], ["c", 70000]], [["d", 100]], [["\uFEFF", 1], ["名", 30000]]] },
    { maxLines: 3, maxBytes: null, ops: Array.from({ length: 40 }, (_, batch) => ({ batchLines: [batch, 300] })) },
    { maxLines: 5000, maxBytes: null, ops: Array.from({ length: 40 }, (_, batch) => ({ batchLines: [batch, 300] })) },
    { maxLines: 2, maxBytes: 100000, ops: [[["x", 20000], ["\n", 1], ["y", 40000]], [["\n", 1]], [["é", 30000]], [["\n", 1], ["z", 5]]] },
  ];
  for (const [index, spec] of big.entries()) {
    const history =
      spec.maxBytes === null
        ? new BoundedTerminalHistory(spec.maxLines, "")
        : new BoundedTerminalHistory(spec.maxLines, "", spec.maxBytes);
    const expected = [record(history.value())];
    // `batchLines: [batch, count]` stands for "batch:0\nbatch:1\n…" (count lines).
    const textOf = (op) =>
      Array.isArray(op)
        ? op.map(([text, count]) => text.repeat(count)).join("")
        : Array.from({ length: op.batchLines[1] }, (_, line) => `${op.batchLines[0]}:${line}\n`).join("");
    for (const op of spec.ops) {
      history.append(textOf(op));
      expected.push(record(history.value()));
    }
    historyCases.push({
      name: `long appends ${index}`,
      maxLines: spec.maxLines,
      maxBytes: spec.maxBytes,
      initial: "",
      ops: spec.ops.map((op) => (Array.isArray(op) ? { append: op } : op)),
      expected,
    });
  }
}

const write = (name, value) => {
  const file = path.join(here, name);
  fs.writeFileSync(file, `${JSON.stringify(value, null, 0)}\n`);
  console.log(`wrote ${path.relative(repo, file)} (${fs.statSync(file).size} bytes)`);
};
write("decoder.json", { generatedFrom: "node:string_decoder", cases: decoderCases });
write("sanitizer.json", { generatedFrom: "code/apps/server/src/terminal/Manager.ts", cases: sanitizerCases });
write("history.json", { generatedFrom: "code/apps/server/src/terminal/Manager.ts", cases: historyCases });
