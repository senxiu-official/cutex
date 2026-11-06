// cutex browser script kernel (design 059 S2). One process per agent session, started by the
// Management process on the agent's own host. It runs the agent's JavaScript and nothing else:
// every browser step is a request to its parent over stdin/stdout (newline-delimited JSON), which
// checks identity, turn, approval and leases per step. No token, network or daemon access here.
"use strict";
const vm = require("node:vm");
const readline = require("node:readline");

const MAX_OUTPUT_BYTES = 60 * 1024;
const MAX_LOG_LINES = 200;

let activeRun = null;
let seq = 0;
const pending = new Map();
const out = line => process.stdout.write(JSON.stringify(line) + "\n");

class BrowserError extends Error {
  constructor(error) {
    super(typeof error?.message === "string" ? error.message : "Browser step failed");
    this.name = "BrowserError";
    this.code = typeof error?.code === "string" ? error.code : "browser_unavailable";
    if (typeof error?.requestId === "string") this.requestId = error.requestId;
    if (error?.hint) this.hint = error.hint;
    if (error?.details) this.details = error.details;
  }
}

function call(command, args) {
  const run = activeRun;
  if (!run) return Promise.reject(new BrowserError({ code: "script_not_running", message: "Browser steps run only while a browser_script call is running." }));
  const id = ++seq;
  out({ t: "call", run: run.id, seq: id, command, args });
  return new Promise((resolve, reject) => pending.set(id, { run: run.id, resolve, reject }));
}

const describe = value => {
  if (typeof value === "string") return value;
  if (value === undefined) return "undefined";
  try {
    const seen = new WeakSet();
    return JSON.stringify(value, (_key, item) => {
      if (typeof item === "bigint") return item.toString();
      if (item && typeof item === "object") { if (seen.has(item)) return "[Circular]"; seen.add(item); }
      return item;
    }, 1);
  } catch { return String(value); }
};

function emitText(text) {
  const run = activeRun;
  if (!run) return;
  const room = MAX_OUTPUT_BYTES - run.bytes;
  if (room <= 0) { run.truncated = true; return; }
  let chunk = text;
  if (Buffer.byteLength(chunk) > room) { chunk = Buffer.from(chunk).subarray(0, room).toString(); run.truncated = true; }
  run.bytes += Buffer.byteLength(chunk);
  run.output.push({ type: "text", text: chunk });
}

class Tab {
  constructor(guestId, title) { this.guestId = guestId; this.title = title; }
  #args(extra, options) { return { title: options?.title ?? this.title ?? activeRun?.title, guestId: this.guestId, ...extra }; }
  read(options) { return call("read", this.#args({}, options)); }
  find(query, options) { return call("find", this.#args({ query }, options)); }
  act(action, options) { return call("act", this.#args({ action }, options)); }
  click(ref, options) { return this.act({ kind: "click", ref }, options); }
  fill(ref, text, options) { return this.act({ kind: "fill", ref, text }, options); }
  press(ref, key, options) { return this.act({ kind: "press", ref, key }, options); }
  select(ref, values, options) { return this.act({ kind: "select", ref, by: options?.by ?? "value", values: [values].flat() }, options); }
  check(ref, options) { return this.act({ kind: "check", ref }, options); }
  uncheck(ref, options) { return this.act({ kind: "uncheck", ref }, options); }
  hover(ref, options) { return this.act({ kind: "hover", ref }, options); }
  scroll(direction, options) { return this.act({ kind: "scroll", direction, ...(options?.amount ? { amount: options.amount } : {}), ...(options?.ref ? { ref: options.ref } : {}) }, options); }
  waitFor(condition, options) { return this.act({ kind: "wait", condition }, options); }
  back(options) { return this.act({ kind: "back" }, options); }
  forward(options) { return this.act({ kind: "forward" }, options); }
  reload(options) { return this.act({ kind: "reload" }, options); }
  navigate(url, options) { return call("navigate", this.#args({ url, ...(options?.snapshot ? { snapshot: true } : {}) }, options)); }
  visit(urls, options) { return call("visit", this.#args({ urls }, options)); }
  screenshot(options) { return call("screenshot", this.#args(options?.ref ? { ref: options.ref } : {}, options)); }
  release(options) { return call("release", this.#args({}, options)); }
}

const browser = Object.freeze({
  docs: topic => call("docs", topic === undefined ? {} : { topic }),
  status: requestId => call("status", { requestId }),
  list: options => call("list", { title: options?.title ?? activeRun?.title }),
  open: async (url, options) => {
    const result = await call("open", { title: options?.title ?? activeRun?.title, url, ...(options?.snapshot ? { snapshot: true } : {}) });
    const guestId = result?.tab?.guestId ?? result?.[0]?.tab?.guestId;
    return Object.assign(new Tab(guestId, options?.title), { opened: result });
  },
  claim: async (reason, options) => {
    const result = await call("claim", { title: options?.title ?? activeRun?.title, reason, ...(options?.urlHint ? { urlHint: options.urlHint } : {}) });
    return result?.claimed ? Object.assign(new Tab(result.guestId, options?.title), { claimed: result }) : result;
  },
  tab: (guestId, options) => new Tab(guestId, options?.title),
});

// Persistent per kernel: globalThis of this context and `state` survive between calls.
const context = vm.createContext({
  browser, BrowserError, state: {},
  emit: value => emitText(describe(value)),
  emitImage: image => { if (activeRun && image?.image) activeRun.output.push({ type: "image", handle: image.image }); },
  sleep: ms => new Promise(resolve => { const timer = setTimeout(resolve, Math.max(0, Math.min(Number(ms) || 0, 30_000))); timer.unref?.(); }),
  console: Object.freeze({
    log: (...items) => { if (activeRun && activeRun.logs++ < MAX_LOG_LINES) emitText(items.map(describe).join(" ") + "\n"); },
    error: (...items) => { if (activeRun && activeRun.logs++ < MAX_LOG_LINES) emitText(items.map(describe).join(" ") + "\n"); },
  }),
  JSON, Math, Date, Promise, Array, Object, String, Number, Boolean, RegExp, Map, Set, WeakMap, WeakSet, Error, TypeError, RangeError,
  URL, URLSearchParams, TextEncoder, TextDecoder, structuredClone, encodeURIComponent, decodeURIComponent, isNaN, isFinite, parseInt, parseFloat,
}, { name: "cutex-browser-script", codeGeneration: { strings: true, wasm: false } });

async function run(message) {
  const runState = { id: message.run, title: message.title, output: [], bytes: 0, logs: 0, truncated: false };
  activeRun = runState;
  let ok = true, value, error;
  try {
    const script = new vm.Script(`(async () => {\n${message.code}\n})()`, { filename: `cell-${message.run}.js` });
    value = await script.runInContext(context, { timeout: Math.max(1, message.syncBudgetMs | 0), breakOnSigint: false });
  } catch (caught) {
    ok = false;
    error = caught instanceof BrowserError
      ? { name: "BrowserError", code: caught.code, message: caught.message, ...(caught.requestId ? { requestId: caught.requestId } : {}), ...(caught.hint ? { hint: caught.hint } : {}) }
      : { name: caught?.name ?? "Error", message: String(caught?.message ?? caught).slice(0, 2000), stack: String(caught?.stack ?? "").split("\n").filter(line => !/^\s+at /.test(line) || /cell-\d+\.js/.test(line)).slice(0, 6).join("\n").slice(0, 2000) };
  }
  if (ok && value !== undefined) emitText(describe(value));
  activeRun = null;
  for (const [id, waiter] of pending) if (waiter.run === runState.id) { pending.delete(id); waiter.reject(new BrowserError({ code: "script_not_running" })); }
  out({ t: "done", run: runState.id, ok, error, output: runState.output, truncated: runState.truncated });
}

const input = readline.createInterface({ input: process.stdin });
input.on("line", line => {
  let message;
  try { message = JSON.parse(line); } catch { return; }
  if (message.t === "run" && !activeRun) { void run(message); return; }
  if (message.t === "run") { out({ t: "done", run: message.run, ok: false, error: { name: "Error", message: "kernel busy" }, output: [] }); return; }
  if (message.t === "reply") {
    const waiter = pending.get(message.seq);
    if (!waiter || waiter.run !== message.run) return;
    pending.delete(message.seq);
    if (message.error) waiter.reject(new BrowserError(message.error)); else waiter.resolve(message.result);
  }
});
input.on("close", () => process.exit(0));
out({ t: "ready", node: process.version });
