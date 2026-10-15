// cutex workflow prelude (src/workflows/exec.rs). Installs the facade
// globals: agent, parallel, pipeline, phase, log, args, budget. Everything
// else that would observe the world is removed or throws.
(function () {
  "use strict";
  const send = globalThis.__send;
  const config = JSON.parse(globalThis.__config);
  delete globalThis.__send;
  delete globalThis.__config;
  const MAX_ITEMS = config.maxItems;
  const pending = new Map();
  let nextId = 1;
  let spent = config.spent || 0;
  let currentPhase = null;

  function post(message) {
    send(JSON.stringify(message));
  }

  function failure(error) {
    const e = new Error(error && error.message ? error.message : "agent failed");
    e.code = error && error.code ? error.code : "agent_failed";
    return e;
  }

  Object.defineProperty(globalThis, "__deliver", {
    value: function (text) {
      const message = JSON.parse(text);
      if (typeof message.spent === "number") spent = message.spent;
      if (message.t !== "result") return;
      const waiter = pending.get(message.id);
      if (!waiter) return;
      pending.delete(message.id);
      if (message.ok) waiter.resolve(message.value === undefined ? null : message.value);
      else waiter.reject(failure(message.error));
    },
  });
  Object.defineProperty(globalThis, "__pendingCount", { value: () => pending.size });
  function describe(e) {
    if (e && typeof e === "object") {
      const code = typeof e.code === "string" ? e.code : "script_error";
      const stack = typeof e.stack === "string" ? e.stack : "";
      return { code, message: String(e.message !== undefined ? e.message : e) + (stack ? "\n" + stack : "") };
    }
    return { code: "script_error", message: String(e) };
  }
  Object.defineProperty(globalThis, "__describe", { value: describe });

  function agent(prompt, opts) {
    if (typeof prompt !== "string") throw new TypeError("agent(prompt, opts): prompt must be a string");
    if (opts !== undefined && (opts === null || typeof opts !== "object" || Array.isArray(opts))) {
      throw new TypeError("agent(prompt, opts): opts must be an object");
    }
    const o = Object.assign({}, opts || {});
    if (o.phase === undefined && currentPhase !== null) o.phase = currentPhase;
    const id = nextId++;
    return new Promise(function (resolve, reject) {
      pending.set(id, { resolve, reject });
      post({ t: "agent", id, prompt, opts: o });
    });
  }

  function checkItems(name, items) {
    if (!Array.isArray(items)) throw new TypeError(name + ": expected an array");
    if (items.length > MAX_ITEMS) throw new RangeError(name + ": at most " + MAX_ITEMS + " items per call");
  }

  function warn(text) {
    post({ t: "log", text: String(text).slice(0, 2048), level: "warn" });
  }

  async function parallel(thunks) {
    checkItems("parallel", thunks);
    return Promise.all(
      thunks.map(async function (thunk, index) {
        try {
          return await thunk();
        } catch (e) {
          warn("parallel item " + index + " failed: " + (e && e.message ? e.message : String(e)));
          return null;
        }
      }),
    );
  }

  async function pipeline(items, ...stages) {
    checkItems("pipeline", items);
    return Promise.all(
      items.map(async function (item, index) {
        let prev = item;
        try {
          for (const stage of stages) prev = await stage(prev, item, index);
          return prev;
        } catch (e) {
          warn("pipeline item " + index + " failed: " + (e && e.message ? e.message : String(e)));
          return null;
        }
      }),
    );
  }

  function phase(title) {
    currentPhase = String(title);
    post({ t: "phase", title: currentPhase });
  }

  function log(message) {
    post({ t: "log", text: String(message).slice(0, 2048), level: "info" });
  }

  const total = typeof config.budgetTotal === "number" ? config.budgetTotal : null;
  const budget = Object.freeze({
    total,
    spent: () => spent,
    remaining: () => (total === null ? Infinity : Math.max(0, total - spent)),
  });

  const frozen = (value) => ({ value, writable: false, configurable: false, enumerable: false });
  Object.defineProperties(globalThis, {
    agent: frozen(agent),
    parallel: frozen(parallel),
    pipeline: frozen(pipeline),
    phase: frozen(phase),
    log: frozen(log),
    budget: frozen(budget),
    args: frozen(config.args === null ? undefined : config.args),
  });

  // Determinism (resume replays the script): no clock, no randomness.
  const RealDate = Date;
  const clockError = () =>
    new Error("Date.now(), Date() and new Date() without arguments are unavailable in workflows (they would break resume); pass timestamps via args");
  function WorkflowDate(...a) {
    if (!new.target) throw clockError();
    if (a.length === 0) throw clockError();
    return new RealDate(...a);
  }
  WorkflowDate.prototype = RealDate.prototype;
  WorkflowDate.UTC = RealDate.UTC;
  WorkflowDate.parse = RealDate.parse;
  WorkflowDate.now = function () {
    throw clockError();
  };
  Object.defineProperty(RealDate.prototype, "constructor", { value: WorkflowDate });
  globalThis.Date = WorkflowDate;
  Math.random = function () {
    throw new Error("Math.random() is unavailable in workflows (it would break resume); vary prompts or labels by index instead");
  };
  for (const name of ["setTimeout", "setInterval", "clearTimeout", "clearInterval", "queueMicrotask", "os", "std", "require", "process"]) {
    try {
      delete globalThis[name];
    } catch (_) {}
  }
})();
