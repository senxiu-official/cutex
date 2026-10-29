// Shared test harness: real `kimi acp` (pinned install) driven by this adapter
// against the scripted offline mock, inside a private throwaway home. Never a
// real key, never the network, never the live cutex. Everything started is
// stopped on teardown.
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, existsSync, chmodSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, delimiter } from 'node:path';
import { execFileSync } from 'node:child_process';
import { once } from 'node:events';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { startMock } from './mock-llm.mjs';

/** The pinned Kimi Code npm tree (`runtime/` of the component, or a private install). */
export const install = process.env.KIMI_PRODUCT_INSTALL
  ?? (existsSync('/home/ema/cutex-dev/ext/kimi-code-install/node_modules/@moonshot-ai/kimi-code/dist/main.mjs') ? '/home/ema/cutex-dev/ext/kimi-code-install' : null);
export const kimiMain = install ? join(install, 'node_modules/@moonshot-ai/kimi-code/dist/main.mjs') : null;
export const delay = ms => new Promise(done => setTimeout(done, ms));

export async function poll(read, accepts, description, timeout = 30000) {
  const deadline = Date.now() + timeout;
  let last;
  while (Date.now() < deadline) {
    try { last = await read(); if (accepts(last)) return last; } catch (error) { last = error.message; }
    await delay(50);
  }
  throw new Error(`Timed out: ${description}; last=${JSON.stringify(last)?.slice(-2000)}`);
}

/** The `config.toml` cutex writes into an agent's private KIMI_CODE_HOME (mirror of src/runtime/kimi.rs). */
export function kimiConfig({ baseUrl, keyEnv = 'KIMI_CUTEX_API_KEY', models = [{ alias: 'mock/k', model: 'mock-kimi', capabilities: ['tool_use', 'image_in'] }], defaultModel = models[0].alias }) {
  const lines = [`default_model = ${JSON.stringify(defaultModel)}`, 'default_permission_mode = "manual"', 'telemetry = false', 'auto_session_title = false', '',
    '[watch]', 'enabled = false', '', '[tools]', 'disabled = ["CronCreate", "CronList", "CronDelete", "AskUserQuestion"]', '',
    '[providers.cutex]', 'type = "kimi"', `base_url = ${JSON.stringify(baseUrl)}`, `api_key_env = ${JSON.stringify(keyEnv)}`, ''];
  for (const model of models) {
    lines.push(`[models.${JSON.stringify(model.alias)}]`, 'provider = "cutex"', `model = ${JSON.stringify(model.model)}`, 'max_context_size = 262144',
      `capabilities = ${JSON.stringify(model.capabilities ?? ['tool_use'])}`);
    if (model.efforts) lines.push(`support_efforts = ${JSON.stringify(model.efforts)}`, `default_effort = ${JSON.stringify(model.defaultEffort ?? model.efforts[0])}`);
    lines.push('');
  }
  lines.push('[[permission.rules]]', 'decision = "allow"', 'pattern = "mcp__cutex__*"', '');
  return lines.join('\n');
}

/** The tools a read-only agent keeps and its PreToolUse hook (mirror of src/runtime/kimi.rs READ_ONLY_TOOLS / read_only_hook). */
export const READ_ONLY_TOOLS = ['Read', 'Grep', 'Glob', 'ReadMediaFile', 'SetTodoList', 'TodoList', 'TaskList', 'TaskOutput', 'WaitFor',
  'WebSearch', 'FetchURL', 'ExitPlanMode', 'GetGoal', 'select_tools', 'NotifyUser', 'mcp__cutex__.*'];
export const READ_ONLY_HOOK = ['[[hooks]]', 'event = "PreToolUse"', `matcher = ${JSON.stringify(`^(?!(${READ_ONLY_TOOLS.join('|')})$)`)}`,
  'command = "echo cutex: this agent is read-only and may not change files or run commands 1>&2 & exit 2"', 'timeout = 10', ''].join('\n');

const windows = process.platform === 'win32';
/**
 * The Kimi child's PATH: this Node first, then the system tools (POSIX: /usr/bin,
 * /bin; Windows: the host PATH, which must reach Git Bash). `path.delimiter`
 * separates entries on both.
 */
function childPath() {
  const node = resolve(process.execPath, '..');
  return windows ? [node, process.env.PATH ?? process.env.Path ?? ''].filter(Boolean).join(delimiter) : [node, '/usr/bin', '/bin'].join(delimiter);
}
/** Windows needs SystemRoot / ComSpec / PATHEXT for any child; Kimi's Bash tool needs Git Bash (KIMI_SHELL_PATH when not on PATH). */
function hostEnv() {
  if (!windows) return {};
  const env = {};
  for (const key of ['SystemRoot', 'WINDIR', 'ComSpec', 'PATHEXT', 'KIMI_SHELL_PATH']) if (process.env[key]) env[key] = process.env[key];
  return env;
}

/** Private dirs + mock + the Kimi child environment, as cutex launches it. */
export async function privateKimi(t, { label = 'kimi', models, config: extraConfig = '' } = {}) {
  const root = mkdtempSync(join(tmpdir(), `kimi-${label}-`));
  chmodSync(root, 0o700);
  writeFileSync(join(root, '.cutex-test-private-home'), 'Kimi adapter test\n');
  const dirs = { root, home: join(root, 'home'), kimiHome: join(root, 'home', 'kimi'), tmp: join(root, 'tmp'), cwd: join(root, 'workspace'), state: join(root, 'state') };
  for (const dir of Object.values(dirs)) mkdirSync(dir, { recursive: true });
  execFileSync('git', ['init', '-q', dirs.cwd], { env: { ...hostEnv(), PATH: process.env.PATH ?? process.env.Path, GIT_CONFIG_GLOBAL: windows ? 'NUL' : '/dev/null', GIT_CONFIG_NOSYSTEM: '1' } });
  const mock = await startMock({ logFile: join(root, 'mock-requests.jsonl') });
  const baseUrl = `http://127.0.0.1:${mock.port}/v1`;
  writeFileSync(join(dirs.kimiHome, 'config.toml'), kimiConfig({ baseUrl, models }) + extraConfig);
  const env = {
    ...hostEnv(), PATH: childPath(), LANG: 'C.UTF-8', HOME: dirs.home, USERPROFILE: dirs.home,
    XDG_CONFIG_HOME: join(dirs.home, '.config'), XDG_CACHE_HOME: join(dirs.home, '.cache'), XDG_DATA_HOME: join(dirs.home, '.local/share'), XDG_STATE_HOME: join(dirs.home, '.local/state'),
    TMPDIR: dirs.tmp, TEMP: dirs.tmp, TMP: dirs.tmp, KIMI_CODE_HOME: dirs.kimiHome, KIMI_CODE_WATCH: '0', KIMI_DISABLE_TELEMETRY: '1', KIMI_DISABLE_CRON: '1',
    KIMI_CODE_NO_AUTO_UPDATE: '1', KIMI_CODE_REGION_MARKER: 'off',
    KIMI_CODE_PLUGIN_MARKETPLACE_URL: `http://127.0.0.1:${mock.port}/no-marketplace.json`,
    KIMI_WEB_SEARCH_BASE_URL: `http://127.0.0.1:${mock.port}/no-search`, KIMI_WEB_FETCH_BASE_URL: `http://127.0.0.1:${mock.port}/no-fetch`,
    KIMI_CUTEX_API_KEY: `sk-fake-offline-${label}-key`,
  };
  const closers = [];
  t.after(async () => {
    for (const close of closers.reverse()) { try { await close(); } catch {} }
    await mock.close();
    if (process.env.KIMI_TEST_RETAIN !== '1') rmSync(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
    else console.error(`retained ${root}`);
  });
  return { dirs, mock, env, baseUrl, onClose: close => closers.push(close) };
}

/** A WebSocket JSON-RPC client of the adapter (the daemon's role). */
export async function connect(endpoint, token, { interactive = true } = {}) {
  const require = createRequire(join(install, 'package.json'));
  const { WebSocket } = require('ws');
  const ws = new WebSocket(endpoint.replace(/^unix:\/\//, 'ws+unix://'), { headers: token ? { Authorization: `Bearer ${token}` } : {} });
  await once(ws, 'open');
  const notifications = [], requests = [], pending = new Map();
  let next = 0;
  ws.on('message', raw => {
    const message = JSON.parse(String(raw));
    if (message.id !== undefined && message.method) requests.push(message);
    else if (message.id !== undefined) { const p = pending.get(message.id); pending.delete(message.id); message.error ? p?.reject(Object.assign(new Error(message.error.message), message.error)) : p?.resolve(message.result); }
    else notifications.push(message);
  });
  const request = (method, params = {}) => new Promise((resolve, reject) => { const id = ++next; pending.set(id, { resolve, reject }); ws.send(JSON.stringify({ id, method, params })); });
  const respond = (id, result) => ws.send(JSON.stringify({ id, result }));
  await request('initialize', { clientInfo: { name: 'test', version: '1' }, capabilities: { dshInteractiveApprovals: interactive } });
  await request('initialized');
  return { ws, request, respond, notifications, requests, close: () => { ws.close(); } };
}

export const adapterPath = fileURLToPath(new URL('../src/server.mjs', import.meta.url));
