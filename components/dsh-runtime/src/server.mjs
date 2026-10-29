import { createServer } from 'node:http';
import { EventEmitter } from 'node:events';
import { createRequire } from 'node:module';
import { createHash, randomUUID, timingSafeEqual } from 'node:crypto';
import { readFileSync, writeFileSync, renameSync, unlinkSync, lstatSync, chmodSync, statSync } from 'node:fs';
import { isAbsolute, resolve, dirname, extname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { DshRuntime, capabilities, DEFAULT_MODEL } from './runtime.mjs';
import { HistoryReader, secretRedactor } from './history-reader.mjs';

// The experimental install supplies ws. Packaging owns the production dependency.
const localRequire = createRequire(import.meta.url);
let websocket;
try { websocket = localRequire('ws'); }
catch {
  const installRoot = process.env.DSH_INSTALL_ROOT ?? process.env.DSH_SPIKE_INSTALL_ROOT;
  const anchor = installRoot
    ? resolve(installRoot, 'package.json')
    : new URL('../../../.dsh-install/package.json', import.meta.url);
  websocket = createRequire(anchor)('ws');
}
const { WebSocketServer, WebSocket } = websocket;
const fault = (message, code = -32602) => Object.assign(new Error(message), { code });
const check = (condition, message, code) => { if (!condition) throw fault(message, code); };
const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
const MAX_FRAME = 8 * 1024 * 1024;
// thread/resume carries the newest turns up to this size (the frame limit with headroom).
const RESUME_TURN_BYTES = 6 * 1024 * 1024;

/** The same peer protocol over a byte stream; preserve split UTF-8 until newline. */
export function ndjsonTransport(socket) {
  const peer = new EventEmitter();
  let buffer = Buffer.alloc(0);
  Object.defineProperties(peer, {
    readyState: { get: () => socket.destroyed ? WebSocket.CLOSED : WebSocket.OPEN },
    bufferedAmount: { get: () => socket.writableLength },
  });
  peer.send = text => { if (Buffer.byteLength(text) > MAX_FRAME) socket.destroy(); else socket.write(text + '\n'); };
  peer.terminate = peer.close = () => socket.destroy();
  socket.on('error', error => peer.emit('error', error));
  socket.on('close', () => peer.emit('close'));
  socket.on('data', chunk => {
    buffer = Buffer.concat([buffer, chunk]);
    let newline;
    while ((newline = buffer.indexOf(10)) !== -1) {
      if (newline > MAX_FRAME) { socket.destroy(); return; }
      const line = buffer.subarray(0, newline);
      buffer = buffer.subarray(newline + 1);
      if (line.length) peer.emit('message', line, false);
      if (socket.destroyed) return;
    }
    if (buffer.length > MAX_FRAME) socket.destroy();
  });
  return peer;
}

function validateSocketPath(path) {
  check(process.platform !== 'win32', 'socketPath requires a Unix host');
  check(typeof path === 'string' && isAbsolute(path) && !path.includes('\0'), 'socketPath must be absolute');
  const parent = lstatSync(dirname(path));
  check(parent.isDirectory() && !parent.isSymbolicLink() && parent.uid === process.getuid() && (parent.mode & 0o777) === 0o700, 'socket parent must be owner-private mode 0700');
  try { lstatSync(path); throw fault('socketPath already exists; refusing to unlink'); }
  catch (error) { if (error.code !== 'ENOENT') throw error; }
}

const IMAGE_TYPES = { png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg', webp: 'image/webp', gif: 'image/gif' };
const MAX_IMAGE_BYTES = 20 * 1024 * 1024;
/** One cute-codex image input (`image` data URL or `localImage` path) as an ACP inline image. */
function readImage(item) {
  if (item.type === 'image') {
    const match = typeof item.url === 'string' && /^data:(image\/(?:png|jpeg|webp|gif));base64,([A-Za-z0-9+/]+={0,2})$/.exec(item.url);
    check(match, 'image input must be a base64 data URL of a PNG, JPEG, WebP or GIF image');
    check(match[2].length * 3 / 4 <= MAX_IMAGE_BYTES, 'image too large');
    return { mimeType: match[1], data: match[2] };
  }
  check(typeof item.path === 'string' && isAbsolute(item.path), 'localImage needs an absolute path');
  const mimeType = IMAGE_TYPES[extname(item.path).slice(1).toLowerCase()];
  check(mimeType, 'localImage must be a PNG, JPEG, WebP or GIF file');
  let bytes;
  try { check(statSync(item.path).size <= MAX_IMAGE_BYTES, 'image too large'); bytes = readFileSync(item.path); }
  catch (error) { if (error.code === -32602) throw error; throw fault('localImage is not readable'); }
  return { mimeType, data: bytes.toString('base64') };
}

/** One native DSH session per independent server; peer lifetime never owns runtime lifetime. */
export async function startServer(config) {
  if (config.socketPath !== undefined) validateSocketPath(config.socketPath);
  else check(typeof config.token === 'string' && config.token.length >= 24 && !/[\s\x00-\x1f\x7f]/.test(config.token), 'explicit bearer token (at least 24 characters) required');
  check(!config.readyFile || isAbsolute(config.readyFile), 'readyFile must be absolute');
  check(config.threadId === undefined || (typeof config.threadId === 'string' && config.threadId.length > 0), 'invalid facade threadId');
  check(config.runtimeId === undefined || (typeof config.runtimeId === 'string' && /^[a-zA-Z0-9._-]{1,256}$/.test(config.runtimeId)), 'invalid runtimeId');
  const runtimeId = config.runtimeId ?? randomUUID();
  const peers = new Set();
  const pending = new Map();
  const secretName = name => /TOKEN|SECRET|PASSWORD|API_KEY/i.test(name);
  // config.redact: earlier keys of the profile (cutex passes them in DSH_CUTEX_REDACT), never given to DSH.
  const secrets = [config.token, ...(Array.isArray(config.redact) ? config.redact : []), ...Object.entries(config.env ?? {}).filter(([key]) => secretName(key)).map(([, value]) => value),
    ...(config.mcpServers ?? []).flatMap(server => (server.env ?? []).filter(entry => secretName(entry.name)).map(entry => entry.value))]
    .filter(value => typeof value === 'string' && value.length).sort((a, b) => b.length - a.length);
  const redact = secretRedactor(secrets);
  const send = (peer, message) => {
    if (peer.ws.readyState !== WebSocket.OPEN) return;
    if (peer.ws.bufferedAmount > 8 * 1024 * 1024) { peer.ws.terminate(); return; }
    peer.ws.send(JSON.stringify(redact(wire(message))));
  };
  const broadcast = (method, params) => {
    for (const peer of peers) if (peer.ready && peer.subscribed && !peer.optOut.has(method)) send(peer, { method, params });
  };
  let runtime;
  const wireThreadId = () => config.threadId ?? runtime.sessionId;
  const wire = message => {
    const result = { ...message };
    // Rewrite protocol identity fields, never user/tool argument objects that
    // happen to contain a property named threadId or the native ID as text.
    for (const key of ['params', 'result']) if (object(message[key])) {
      result[key] = { ...message[key] };
      if (result[key].threadId === runtime.sessionId) result[key].threadId = wireThreadId();
      if (result[key].thread?.id === runtime.sessionId) result[key].thread = { ...result[key].thread, id: wireThreadId() };
    }
    return result;
  };
  let closing;
  let lastError = null;
  function settle(id, result) {
    const request = pending.get(id);
    if (!request) return;
    pending.delete(id);
    request.signal?.removeEventListener('abort', request.abort);
    request.resolve(result);
    broadcast('serverRequest/resolved', { threadId: runtime.sessionId, requestId: id });
    broadcast('thread/status/changed', { threadId: runtime.sessionId, status: status() });
  }
  // An approval waits for an answer or the end of its turn; with no owner
  // client connected it stays pending and goes to the next one that subscribes.
  function approve({ method, params, signal }) {
    const targets = new Set([...peers].filter(p => p.ready && p.interactive && p.subscribed));
    if (signal?.aborted) return Promise.resolve(null);
    const id = `dsh-approval-${randomUUID()}`;
    return new Promise(resolveApproval => {
      const abort = () => settle(id, null);
      pending.set(id, { targets, signal, abort, resolve: resolveApproval, message: { id, method, params } });
      signal?.addEventListener('abort', abort, { once: true });
      broadcast('thread/status/changed', { threadId: runtime.sessionId, status: status() });
      for (const peer of targets) send(peer, { id, method, params });
    });
  }
  const reader = config.readOnly ? new HistoryReader(config) : null;
  const advertised = reader ? { ...capabilities, resume: false, interrupt: false, readOnly: true, tokenUsage: false, threadSettingsUpdate: false } : { ...capabilities, tokenUsage: true, interAgentMessages: true, inputAdmissions: ['exclusive_turn'] };
  runtime = reader ? { sessionId: config.threadId, cwd: config.cwd, replay: () => reader.events(), close: async () => reader.close(), rpc: {} }
    : new DshRuntime({ ...config, secrets, model: config.model ?? (config.secretEnvMappings?.length ? DEFAULT_MODEL : undefined), sessionsRoot: config.sessionsRoot, installRoot: config.env?.DSH_INSTALL_ROOT ?? config.env?.DSH_SPIKE_INSTALL_ROOT, approve, emit: event => {
    // Raw ACP updates remain internal; only the projected app-server events go on wire.
    if (event.method === 'dsh/sessionUpdate') return;
    broadcast(event.method, event.params);
    if (event.method === 'turn/started' || event.method === 'turn/completed') {
      broadcast('thread/status/changed', { threadId: runtime.sessionId, status: event.method === 'turn/completed' ? { type: 'idle' } : status() });
    }
  } });
  // The files the agent's instructions come from (cutex's user AGENTS.md and
  // the project files DSH loads itself), as cute-codex reports them.
  const instructionSources = Array.isArray(config.instructionSources) && config.instructionSources.every(value => typeof value === 'string') ? config.instructionSources : null;
  const mcpServers = config.mcpServers ?? [];
  // The identity-aware runtime factory must run after session/new has supplied
  // the native identity and before the final MCP-enabled resume.
  const nativeMcp = sessionId => mcpServers.map(server => server.name !== 'cutex' ? server : {
    ...server, env: [...(server.env ?? []).filter(entry => entry.name !== 'CUTEX_MCP_THREAD_ID'),
      { name: 'CUTEX_MCP_THREAD_ID', value: config.threadId ?? sessionId }],
  });
  try {
    if (!reader) {
      await runtime.initialize(mcpServers.length ? nativeMcp : []);
      check(config.nativeSessionId == null || config.nativeSessionId === runtime.sessionId, 'native session identity differs from saved launch');
    }
  }
  catch (error) { await runtime.close(); throw error; }
  function status() {
    if (reader) return { type: 'notLoaded' };
    return runtime.turnId ? { type: 'active', activeFlags: pending.size ? ['waitingOnApproval'] : [] } : { type: 'idle' };
  }
  // Paged history from the live store (the timeline index), never a full replay.
  // Attached after initialize, when the wire thread id is known.
  let attached;
  const history = () => attached ??= HistoryReader.attach(runtime.store, { threadId: wireThreadId(), cwd: runtime.cwd, liveTurnId: () => runtime.turnId });
  /**
   * thread/read includeTurns: every turn within the reader's budget (refused
   * above it: use the paged methods). thread/resume: the newest turns within
   * that budget (older ones stay readable through thread/turns/list).
   */
  function turns({ bounded = false } = {}) {
    if (!bounded) {
      const result = history().snapshot({ includeTurns: true }).turns;
      const active = result.find(turn => turn.id === runtime.turnId);
      if (active) { active.status = 'inProgress'; active.error = null; }
      return { turns: result, truncated: false };
    }
    const result = [];
    let bytes = 0, cursor = null, truncated = false;
    page: for (;;) {
      const { data, nextCursor } = history().handle('thread/turns/list', { threadId: wireThreadId(), sortDirection: 'desc', itemsView: 'full', limit: 100, ...(cursor ? { cursor } : {}) });
      for (const entry of data) {
        const { itemsView: _view, ...turn } = entry;
        bytes += Buffer.byteLength(JSON.stringify(turn));
        if (bytes > RESUME_TURN_BYTES) { truncated = true; break page; }
        if (turn.id === runtime.turnId) { turn.status = 'inProgress'; turn.error = null; }
        result.push(turn);
      }
      if (!nextCursor) break;
      cursor = nextCursor;
    }
    return { turns: result.reverse(), truncated };
  }
  function thread(includeTurns = true, { bounded = false } = {}) {
    const occupancy = runtime.store?.latest('dsh/contextUsage/updated')?.params;
    const listed = includeTurns ? turns({ bounded }) : { turns: [], truncated: false };
    return { id: wireThreadId(), cwd: runtime.cwd, status: status(), turns: listed.turns, ...(listed.truncated ? { turnsTruncated: true } : {}),
      ephemeral: false, modelProvider: 'dsh', preview: '', createdAt: 0, updatedAt: 0, path: null,
      source: 'appServer', cliVersion: 'experimental-dsh', name: null,
      model: runtime.selection?.model ?? config.model ?? DEFAULT_MODEL, reasoningEffort: runtime.selection?.reasoningEffort ?? null,
      billing: 'unavailable', capabilities: advertised,
      dshContext: occupancy ? { used: occupancy.used, size: occupancy.size, observedAt: occupancy.observedAt ?? null, source: 'acp', billing: 'unavailable' } : null,
      ...(runtime.nativeLog ? { dshNativeLog: runtime.nativeLog } : {}) };
  }
  function subscribe(peer) {
    peer.subscribed = true;
    // Reconnecting Management clients can take over still-pending owner approvals.
    if (peer.interactive) for (const request of pending.values()) {
      request.targets.add(peer);
      send(peer, request.message);
    }
  }
  function dispatch(peer, method, params) {
    check(!closing, 'server shutting down', -32009);
    if (method === 'initialize') {
      check(!peer.initialized, 'already initialized', -32600);
      const caps = params.capabilities ?? {};
      check(object(caps), 'capabilities must be an object');
      check(caps.optOutNotificationMethods === undefined || (Array.isArray(caps.optOutNotificationMethods) && caps.optOutNotificationMethods.every(x => typeof x === 'string')), 'invalid notification opt-outs');
      peer.initialized = true;
      peer.interactive = caps.dshInteractiveApprovals === true;
      peer.optOut = new Set(caps.optOutNotificationMethods ?? []);
      return { userAgent: 'cutex-dsh-app-server/experimental', capabilities: advertised, runtimeId };
    }
    check(peer.initialized, 'initialize required', -32002);
    if (method === 'initialized') { peer.ready = true; return {}; }
    check(peer.ready, 'initialized required', -32002);
    if (reader) return reader.handle(method, params);
    if (['thread/start', 'thread/resume', 'turn/start'].includes(method)) {
      const selectedModel = runtime.selection?.model ?? config.model ?? DEFAULT_MODEL;
      const selectedEffort = runtime.selection?.reasoningEffort ?? config.reasoningEffort ?? null;
      check(params.model == null || params.model === selectedModel, 'the model changes through thread/settings/update');
      check((params.reasoningEffort ?? params.effort) == null || (params.reasoningEffort ?? params.effort) === selectedEffort, 'the reasoning effort changes through thread/settings/update');
    }
    if (method === 'thread/loaded/list') return { data: [wireThreadId()] };
    if (method === 'model/list') return { data: runtime.modelList(), nextCursor: null };
    if (method === 'dsh/server/shutdown') return {};
    if (method === 'thread/start') {
      check(params.cwd === undefined || resolve(params.cwd) === runtime.cwd, 'cwd differs from native session');
      subscribe(peer);
      return { thread: thread(), model: runtime.selection?.model ?? config.model ?? DEFAULT_MODEL, reasoningEffort: runtime.selection?.reasoningEffort ?? null, modelProvider: 'dsh', cwd: runtime.cwd, instructionSources };
    }
    check(params.threadId === wireThreadId(), 'unknown native DSH thread');
    if (method === 'thread/resume') {
      check(params.cwd === undefined || resolve(params.cwd) === runtime.cwd, 'cwd differs from native session');
      subscribe(peer);
      return { thread: thread(params.excludeTurns !== true, { bounded: true }), model: runtime.selection?.model ?? config.model ?? DEFAULT_MODEL, reasoningEffort: runtime.selection?.reasoningEffort ?? null, modelProvider: 'dsh', cwd: runtime.cwd, instructionSources };
    }
    if (method === 'thread/read') return { thread: thread(params.includeTurns === true) };
    if (method === 'thread/turns/list' || method === 'thread/items/list') return history().handle(method, params);
    if (method === 'thread/busy') return { busy: Boolean(runtime.turnId), turnId: runtime.turnId ?? null };
    if (method === 'dsh/runtime/status') return { runtimeId, threadId: runtime.sessionId, sessionId: runtime.sessionId,
      ...(config.threadId === undefined ? {} : { nativeSessionId: runtime.sessionId }),
      busy: Boolean(runtime.turnId), activeTurnId: runtime.turnId ?? null, pendingApprovals: pending.size,
      alive: !runtime.closed && !runtime.rpc.dead, lastError, capabilities: advertised, nativeLog: runtime.nativeLog ?? null };
    if (method === 'turn/start') {
      check(!runtime.turnId, 'DSH is busy; steer unsupported', -32009);
      check(!runtime.uncertain, 'runtime unavailable; restart before another prompt', -32009);
      check(runtime.available, runtime.recovering ? 'DSH is restarting; retry shortly' : 'runtime unavailable', -32009);
      check(Array.isArray(params.input) && params.input.length && params.input.every(i => object(i) && ((i.type === 'text' && typeof i.text === 'string') || i.type === 'image' || i.type === 'localImage')), 'only text and image input is supported');
      const images = params.input.filter(i => i.type !== 'text');
      check(!images.length || runtime.imagePrompts, 'this DSH model does not accept images', -32602);
      const decoded = images.map(readImage);
      // The owner's originals (a local file's path; a data URL only by digest, no bytes kept).
      const originals = images.map(i => i.type === 'localImage' ? { type: 'localImage', path: resolve(i.path) }
        : { type: 'image', sha256: createHash('sha256').update(i.url).digest('hex') });
      subscribe(peer);
      // prompt records turn/started synchronously before its first await.
      const task = runtime.prompt(params.input.filter(i => i.type === 'text').map(i => i.text).join('\n'), decoded, originals);
      const turn = { id: runtime.turnId, status: 'inProgress', items: [], error: null };
      void task.catch(error => { lastError = redact(error.message); });
      return { turn };
    }
    if (method === 'thread/inter_agent_message') {
      check(object(params), 'invalid inter-agent message');
      return runtime.deliver(params);
    }
    if (method === 'thread/inter_agent_message/status') return runtime.deliveries.status(wireThreadId(), params.messages);
    if (method === 'dsh/input/submit') return { threadId: wireThreadId(), ...runtime.submitInput(params) };
    if (method === 'dsh/input/status') {
      check(typeof params.messageId === 'string', 'messageId required');
      return { threadId: wireThreadId(), ...runtime.inputStatus(params.messageId) };
    }
    if (method === 'thread/settings/update') {
      const extra = Object.keys(params).filter(key => !['threadId', 'model', 'effort', 'reasoningEffort'].includes(key));
      check(!extra.length, `DSH can change model and effort only (not ${extra.join(', ')})`);
      const effort = params.effort !== undefined ? params.effort : params.reasoningEffort;
      check(params.model !== undefined || effort !== undefined, 'nothing to update');
      return runtime.updateSelection({ model: params.model ?? undefined, effort }).then(selection => {
        const threadSettings = { cwd: runtime.cwd, model: selection.model, effort: selection.reasoningEffort, modelProvider: 'dsh' };
        broadcast('thread/settings/updated', { threadId: runtime.sessionId, threadSettings });
        return { threadSettings };
      });
    }
    if (method === 'turn/interrupt') {
      check(runtime.turnId && params.turnId === runtime.turnId, 'turn is not active');
      runtime.cancel();
      return {};
    }
    throw fault('unsupported method', -32601);
  }
  // Both transports speak WebSocket, as the daemon's app-server client does
  // (over TCP with a bearer, or over the owner-private Unix socket, whose
  // 0700 parent and 0600 mode are its authentication).
  const unix = config.socketPath !== undefined;
  const http = createServer((_req, res) => { res.writeHead(404); res.end(); });
  const wss = new WebSocketServer({ noServer: true, maxPayload: MAX_FRAME, perMessageDeflate: false });
  http.on('upgrade', (req, socket, head) => {
    let valid = unix;
    if (!unix) {
      const expected = Buffer.from(`Bearer ${config.token}`);
      const supplied = Buffer.from(req.headers.authorization ?? '');
      valid = supplied.length === expected.length && timingSafeEqual(supplied, expected);
    }
    if (!valid || req.headers.origin !== undefined || !['/', '/rpc'].includes(req.url)) {
      socket.end(`HTTP/1.1 ${valid ? '403 Forbidden' : '401 Unauthorized'}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n`);
      return;
    }
    wss.handleUpgrade(req, socket, head, ws => wss.emit('connection', ws));
  });
  wss.on('connection', connected);
  function connected(ws) {
    const peer = { ws, initialized: false, ready: false, interactive: false, subscribed: false, optOut: new Set() };
    peers.add(peer);
    ws.on('error', () => {});
    ws.on('close', () => {
      peers.delete(peer);
      for (const request of pending.values()) request.targets.delete(peer);
    });
    ws.on('message', (raw, binary) => {
      if (binary) { ws.close(1003, 'text frames required'); return; }
      let message;
      try { message = JSON.parse(raw.toString()); }
      catch { send(peer, { id: null, error: { code: -32700, message: 'invalid JSON' } }); return; }
      if (!object(message) || (message.id !== undefined && typeof message.id !== 'string' && typeof message.id !== 'number')) {
        send(peer, { id: null, error: { code: -32600, message: 'invalid request' } }); return;
      }
      if (message.method === undefined) {
        const request = pending.get(message.id);
        if (request?.targets.has(peer) && peer.ready && peer.interactive && peer.subscribed) {
          const decision = message.error ? null : message.result?.decision;
          settle(message.id, ['accept', 'decline', 'cancel'].includes(decision) ? { decision } : null);
        }
        return;
      }
      const fail = error => {
        if (message.id !== undefined) send(peer, { id: message.id, error: { code: error.code ?? -32603, message: error.code ? error.message : 'internal adapter error', ...(error.code && error.data ? { data: error.data } : {}) } });
      };
      try {
        check(typeof message.method === 'string' && (message.params === undefined || object(message.params)), 'invalid request', -32600);
        const result = dispatch(peer, message.method, message.params ?? {});
        // Most methods answer synchronously (ordering with their notifications); a few wait on DSH.
        if (typeof result?.then === 'function') {
          result.then(value => { if (message.id !== undefined) send(peer, { id: message.id, result: value }); }, fail);
          return;
        }
        if (message.id !== undefined) send(peer, { id: message.id, result });
        if (message.method === 'dsh/server/shutdown') setImmediate(() => { void close(); });
      } catch (error) { fail(error); }
    });
  }
  async function close() {
    if (closing) return closing;
    closing = (async () => {
      for (const id of pending.keys()) settle(id, null);
      for (const peer of peers) peer.ws.terminate();
      http.closeAllConnections?.();
      await Promise.all([new Promise(done => http.close(done)), new Promise(done => wss.close(done)), runtime.close()]);
      if (config.readyFile) {
        try { if (JSON.parse(readFileSync(config.readyFile, 'utf8')).runtimeId === runtimeId) unlinkSync(config.readyFile); }
        catch (error) { if (error.code !== 'ENOENT') throw error; }
      }
    })();
    return closing;
  }
  try {
    let endpoint;
    if (unix) {
      // Recheck after ACP initialization; never remove a path created meanwhile.
      validateSocketPath(config.socketPath);
      await new Promise((done, reject) => { http.once('error', reject); http.listen(config.socketPath, done); });
      chmodSync(config.socketPath, 0o600);
      endpoint = `unix://${config.socketPath}`;
    } else {
      await new Promise((done, reject) => { http.once('error', reject); http.listen(0, '127.0.0.1', done); });
      endpoint = `ws://127.0.0.1:${http.address().port}`;
    }
    const ready = { pid: process.pid, endpoint, threadId: wireThreadId(), runtimeId,
      ...(config.threadId === undefined ? {} : { nativeSessionId: config.readOnly ? reader.nativeSessionId ?? null : runtime.sessionId }) };
    if (config.readyFile) {
      const temporary = `${config.readyFile}.${runtimeId}.tmp`;
      try { writeFileSync(temporary, JSON.stringify(ready), { mode: 0o600, flag: 'wx' }); renameSync(temporary, config.readyFile); }
      finally { try { unlinkSync(temporary); } catch (error) { if (error.code !== 'ENOENT') throw error; } }
    }
    return { ...ready, close };
  } catch (error) { await close(); throw error; }
}

/** No implicit home, credentials, config discovery, or inherited child environment. */
export function configFromEnv(env = process.env) {
  check(env.DSH_CUTEX_CONFIG && isAbsolute(env.DSH_CUTEX_CONFIG), 'DSH_CUTEX_CONFIG must name an absolute JSON file');
  const config = JSON.parse(readFileSync(env.DSH_CUTEX_CONFIG, 'utf8'));
  check(object(config), 'config must be an object');
  if (!config.readOnly) {
    check(typeof config.command === 'string' && isAbsolute(config.command), 'command must be absolute');
    check(Array.isArray(config.args) && config.args.every(arg => typeof arg === 'string'), 'args must be strings');
  }
  check((config.readOnly && config.env === undefined) || (object(config.env) && Object.values(config.env).every(value => typeof value === 'string')), 'env must explicitly contain string values');
  check(typeof config.cwd === 'string' && isAbsolute(config.cwd) && typeof config.database === 'string' && isAbsolute(config.database), 'absolute cwd and database required');
  check(typeof config.readyFile === 'string' && isAbsolute(config.readyFile), 'absolute readyFile required');
  const childEnv = { ...config.env };
  check(config.secretEnvMappings === undefined || Array.isArray(config.secretEnvMappings), 'secretEnvMappings must be an array');
  const targets = new Set();
  for (const mapping of config.secretEnvMappings ?? []) {
    check(object(mapping) && Object.keys(mapping).every(key => ['source', 'target'].includes(key))
      && typeof mapping.source === 'string' && /^[A-Za-z_][A-Za-z0-9_]*$/.test(mapping.source)
      && mapping.target === 'DEEPSEEK_API_KEY' && !targets.has(mapping.target), 'invalid secret environment mapping');
    check(typeof env[mapping.source] === 'string' && env[mapping.source].trim().length > 0, 'mapped secret environment variable is missing or blank');
    check(!Object.hasOwn(childEnv, mapping.target), 'mapped secret must not be stored in config.env');
    childEnv[mapping.target] = env[mapping.source];
    targets.add(mapping.target);
  }
  let redact = [];
  try { redact = JSON.parse(env.DSH_CUTEX_REDACT ?? '[]'); } catch {}
  redact = Array.isArray(redact) ? redact.filter(value => typeof value === 'string' && value.length) : [];
  return { ...config, env: childEnv, token: env.DSH_CUTEX_TOKEN ?? config.token, ...(redact.length ? { redact } : {}) };
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const server = await startServer(configFromEnv());
    const stop = () => { void server.close().catch(() => { process.exitCode = 1; }); };
    process.once('SIGINT', stop);
    process.once('SIGTERM', stop);
  } catch { console.error('DSH app-server startup failed; check explicit config and private runtime paths.'); process.exitCode = 1; }
}
