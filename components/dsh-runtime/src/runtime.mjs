import { randomUUID } from 'node:crypto';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { isAbsolute, resolve } from 'node:path';
import { AcpRpc } from './rpc.mjs';
import { TimelineStore, NATIVE_SEQ, TURN_INPUTS } from './store.mjs';
import { join } from 'node:path';
import { reconcileNativeLog, loadNativeCatalog, NativeLogScanCache } from './reconcile.mjs';
import { secretRedactor, streamRedactor } from './history-reader.mjs';
import { DeliveryQueue } from './delivery.mjs';
import { nativeTurnId } from './nativeProjection.mjs';
import { acpResultBlocks, parseArguments, todoPlan, toolResultItem, toolStartItem } from './toolItems.mjs';

const assistantKind = event => event.method === 'item/completed' && typeof event.params?.item?.id === 'string'
  ? event.params.item.id.startsWith('agent_message_chunk:') ? 'text' : event.params.item.id.startsWith('agent_thought_chunk:') ? 'reasoning' : null
  : null;

// cutex runs fork / revert / delete through this component's one-shot session-ops.mjs (r265).
const structureOps = existsSync(fileURLToPath(new URL('./session-ops.mjs', import.meta.url)));
export const capabilities = Object.freeze({ resume: true, interrupt: true, steer: false, fork: structureOps, revert: structureOps, delete: structureOps, threadSettingsUpdate: true });

/** Only explicit one-shot decisions can choose the advertised one-shot options. */
export function permissionOutcome(params, decision) {
  const kind = decision === 'accept' ? 'allow_once' : decision === 'decline' ? 'reject_once' : undefined;
  const choice = kind && params.options?.find(option => option.kind === kind);
  return choice ? { outcome: { outcome: 'selected', optionId: choice.optionId } } : { outcome: { outcome: 'cancelled' } };
}

export function cutexMcp(command, env) {
  if (!isAbsolute(command)) throw new Error('cutex-mcp command must be absolute');
  for (const key of ['CUTEX_AGENT_ID', 'CUTEX_RUNTIME_GENERATION', 'CUTEX_AGENT_BUS_URL', 'CUTEX_AGENT_BUS_TOKEN']) {
    if (!env[key]) throw new Error(`cutex-mcp requires ${key}`);
  }
  const allowed = new Set(['CUTEX_AGENT_ID', 'CUTEX_RUNTIME_GENERATION', 'CUTEX_AGENT_BUS_URL', 'CUTEX_AGENT_BUS_TOKEN', 'CUTEX_MCP_THREAD_ID', 'CUTEX_MCP_APPROVAL_WAIT_SECS', 'HOME', 'USERPROFILE', 'CUTEX_TEST_PRIVATE_HOME']);
  return { name: 'cutex', command, args: [], env: Object.entries(env).filter(([name]) => allowed.has(name)).map(([name, value]) => ({ name, value })) };
}

/**
 * What the owner reads for one DSH ask: the command (shell tools) or the tool
 * and its arguments, the cwd, and the model's own justification.
 */
export function approvalText(tool, cwd) {
  const args = tool.arguments && typeof tool.arguments === 'object' ? tool.arguments : {};
  const { sandbox_permissions: mode, justification, ...rest } = args;
  const shell = typeof rest.command === 'string' ? rest.command : Array.isArray(rest.command) && rest.command.every(part => typeof part === 'string') ? rest.command.join(' ') : undefined;
  const command = shell ?? `${tool.tool} ${JSON.stringify(rest)}`;
  const wider = typeof mode === 'string' ? ` (asks for ${mode})` : '';
  const reason = typeof justification === 'string' && justification.trim() ? `${justification.trim()}${wider}` : `DSH asks to run ${tool.tool} once${wider}.`;
  return { command, cwd: typeof rest.cwd === 'string' ? rest.cwd : cwd, reason };
}

/** DSH's own default model (pinned rc.3 catalog: deepseek-flash, deepseek-v4-pro). */
export const DEFAULT_MODEL = 'deepseek-flash';
/** The pre-r265 cutex default: retired upstream and served by Flash; agents created with it keep working. */
const LEGACY_ALIASES = { 'deepseek-v4-flash': 'deepseek-flash' };

const normalizeChoice = value => String(value).toLowerCase().replace(/[^a-z0-9]/g, '');
const optionChoices = option => (option?.options ?? []).flatMap(entry => entry.options ?? [entry]);
/** The one DSH choice for a cutex model id or effort name (opaque value, route id or display name). */
function chooseOption(options, category, wanted) {
  const option = (options ?? []).find(option => option.category === category && option.type === 'select');
  const routeId = value => { try { const route = JSON.parse(value); return Array.isArray(route) ? route[1] : undefined; } catch { return undefined; } };
  const find = name => optionChoices(option).filter(choice => choice.value === name || routeId(choice.value) === name || normalizeChoice(choice.name) === normalizeChoice(name));
  let matches = find(wanted);
  if (!matches.length && category === 'model' && LEGACY_ALIASES[wanted]) matches = find(LEGACY_ALIASES[wanted]);
  if (!option || matches.length !== 1 || typeof matches[0].value !== 'string') throw Object.assign(new Error(`requested ${category} is unavailable or ambiguous`), { code: -32602 });
  return { option, value: matches[0].value };
}

/** ACP controller; the experimental daemon backend selects authoritative native-log mode. */
export class DshRuntime {
  constructor({ command, args, cwd, env, database, sessionsRoot, installRoot, model, reasoningEffort, secrets = [], approve = async () => null, emit = () => {}, liveness = {} }) {
    if (!isAbsolute(cwd) || !isAbsolute(database)) throw new Error('absolute cwd and database required');
    this.cwd = resolve(cwd);
    this.emit = emit;
    this.approve = approve;
    const redacted = [...secrets, ...Object.entries(env ?? {}).filter(([name]) => /TOKEN|SECRET|PASSWORD|API_KEY/i.test(name)).map(([, value]) => value)];
    this.redact = secretRedactor(redacted);
    // Streamed deltas: a secret split over chunks must not pass in pieces.
    this.streamFilter = streamRedactor(redacted);
    // ACP-path (no native log) stream filters of the open turn, by item id.
    this.acpFilters = new Map();
    this.requestedModel = model;
    this.requestedEffort = reasoningEffort;
    this.store = new TimelineStore(database);
    // Reconciliation retains the original native hash/cursor for prefix checks,
    // but secret-bearing raw bodies and their projections never enter SQLite.
    const commitNative = this.store.commitNative.bind(this.store);
    // The native seq being committed names the live attempt it settles.
    this.store.commitNative = record => {
      this.committingNativeSeq = record.nativeSeq;
      let committed;
      try { committed = commitNative({ ...record, event: this.redact(record.event), updates: this.redact(record.updates) }); }
      finally { this.committingNativeSeq = undefined; }
      this.noteUsage(record);
      return committed;
    };
    this.sessionsRoot = sessionsRoot;
    this.installRoot = installRoot;
    this.scans = Promise.resolve();
    this.nativeScanCache = new NativeLogScanCache();
    // Live streaming (display only; the native log stays the authority):
    // attempts open by the cutex plugin's frames, their live items, the live
    // id each committed native seq takes, and committed events held (in
    // order) while an attempt's end frame is still on its way.
    this.live = new Map();
    this.aliases = new Map();
    this.held = [];
    this.store.onNativeCommitted = events => {
      const seqs = new Set();
      for (const event of events) {
        // Batched commits (C2's tail reader) notify once per batch: each event carries its own native seq.
        const nativeSeq = event[NATIVE_SEQ] ?? this.committingNativeSeq;
        seqs.add(nativeSeq);
        if (event.params?.item) this.items.set(event.params.item.id, event.params.item);
        if (this.held.length || (this.live.size && assistantKind(event))) this.hold(event, nativeSeq);
        else this.publishNative(event, nativeSeq);
      }
      if (!this.held.length) for (const nativeSeq of seqs) this.aliases.delete(nativeSeq);
    };
    this.sessionId = this.store.binding()?.sessionId;
    const saved = this.store.binding();
    if (saved && resolve(saved.cwd) !== this.cwd) { this.store.close(); throw new Error('resume cwd differs from binding'); }
    this.items = new Map();
    // Raw DSH name and arguments of each tool call (ACP path; approvals read them).
    this.calls = new Map();
    this.deliveries = new DeliveryQueue();
    // Streamed (never one array of the whole timeline): long-lived agents keep large timelines.
    for (const event of this.store.iterate()) {
      this.deliveries.observe(event);
      if (event.method === 'dsh/tokenUsage') this.usageTotal = event.params.total;
      if (event.method === 'dsh/contextUsage/updated') this.contextWindow = event.params.size;
    }
    // Liveness, not a wall clock: a turn may run as long as DSH keeps
    // answering. While ACP is silent for `interval`, a cheap `session/list`
    // probe must answer within `probeTimeout`; `misses` failed probes in a
    // row mean DSH is wedged, so it is killed and restarted (resume).
    this.liveness = { interval: 30000, probeTimeout: 30000, misses: 2, restartDelays: [500, 2000, 5000, 15000, 30000], ...liveness };
    this.spawn = () => {
      const rpc = new AcpRpc(command, args, { cwd, env, update: p => this.update(p), permission: p => this.permission(p),
        ...(sessionsRoot ? { onStream: frame => this.stream(frame) } : {}),
        onExit: dead => { if (dead === this.rpc && !this.closed && !this.turnId) void this.recover(); } });
      return rpc;
    };
    this.rpc = this.spawn();
  }

  /**
   * Token usage of one committed native assistant message, as cute-codex
   * reports it (`thread/tokenUsage/updated`: thread-cumulative `total` and
   * this request's `last`). DeepSeek's input count excludes cache reads and
   * writes; cute-codex's input includes them (cached is a subset). The total
   * is a durable fact of the timeline (`dsh/tokenUsage`, not history), so a
   * restarted adapter continues the same counter.
   */
  noteUsage(record) {
    const event = record?.event;
    const usage = event?.type === 'assistant/message' ? event.data?.usage : undefined;
    const count = value => Number.isSafeInteger(value) && value >= 0 ? value : 0;
    if (!usage || typeof usage !== 'object') return;
    const cacheRead = count(usage.cacheReadTokens), cacheWrite = count(usage.cacheWriteTokens);
    const input = count(usage.inputTokens) + cacheRead + cacheWrite, output = count(usage.outputTokens);
    const last = { totalTokens: input + output, inputTokens: input, cachedInputTokens: cacheRead, cacheWriteInputTokens: cacheWrite,
      outputTokens: output, reasoningOutputTokens: Math.min(count(usage.reasoningTokens), output) };
    if (last.totalTokens === 0) return;
    const previous = this.usageTotal ?? {};
    const total = Object.fromEntries(Object.entries(last).map(([key, value]) => [key, count(previous[key]) + value]));
    const turn = event.data?.turn;
    const turnId = Number.isSafeInteger(turn) && turn > 0 ? nativeTurnId(record.sessionId, turn) : this.turnId ?? null;
    this.usageTotal = total;
    this.store.append({ method: 'dsh/tokenUsage', params: { threadId: record.sessionId, turnId, nativeSeq: record.nativeSeq, total, last } });
    if (!turnId) return;
    this.emit({ method: 'thread/tokenUsage/updated', params: { threadId: record.sessionId, turnId,
      tokenUsage: { total, last, ...(Number.isSafeInteger(this.contextWindow) && this.contextWindow > 0 ? { modelContextWindow: this.contextWindow } : {}) } } });
  }

  /** A committed event under the live id its attempt streamed with, so a live client never sees the message twice. */
  publishNative(event, nativeSeq) {
    const kind = assistantKind(event);
    const alias = kind && this.aliases.get(nativeSeq)?.[kind];
    this.publish(alias ? { ...event, params: { ...event.params, item: { ...event.params.item, id: alias } } } : event);
  }

  hold(event, nativeSeq) {
    this.held.push({ event, nativeSeq });
    // An end frame arrives within milliseconds; never hold history longer.
    this.heldTimer ??= setTimeout(() => this.releaseHeld(true), 1000);
    this.heldTimer.unref?.();
  }

  releaseHeld(force = false) {
    if (!force && this.live.size) return;
    clearTimeout(this.heldTimer);
    this.heldTimer = undefined;
    const held = this.held;
    this.held = [];
    // Abandoned attempts (no end frame): their held-back tails go out first.
    if (force) for (const attempt of this.live.values()) this.flushLive(attempt);
    for (const { event, nativeSeq } of held) this.publishNative(event, nativeSeq);
    for (const { nativeSeq } of held) this.aliases.delete(nativeSeq);
    if (force) this.live.clear();
  }

  /** One frame of the cutex plugin's live assistant stream. */
  stream(frame) {
    if (frame?.sessionId !== this.sessionId || typeof frame.attemptId !== 'string') return;
    if (frame.type === 'start') {
      const turnId = Number.isSafeInteger(frame.turn) && frame.turn > 0 ? nativeTurnId(this.sessionId, frame.turn) : this.turnId ?? null;
      this.live.set(frame.attemptId, { turnId, items: {} });
      return;
    }
    const attempt = this.live.get(frame.attemptId);
    if (!attempt) return;
    const base = { threadId: this.sessionId, turnId: attempt.turnId };
    if (frame.type === 'delta' && typeof frame.text === 'string') {
      const kind = frame.kind === 'reasoning' ? 'reasoning' : 'text';
      let item = attempt.items[kind];
      if (!item) {
        item = attempt.items[kind] = { id: `dsh-live:${frame.attemptId}:${kind}`, text: '', filter: this.streamFilter() };
        this.emit({ method: 'item/started', params: { ...base, item: kind === 'text' ? { id: item.id, type: 'agentMessage', text: '' } : { id: item.id, type: 'reasoning', summary: [], content: [] } } });
      }
      item.text += frame.text;
      this.emitLiveDelta(base, kind, item, item.filter.push(frame.text));
    } else if (frame.type === 'end') {
      this.live.delete(frame.attemptId);
      // The held-back tail goes out before the item ends, whatever the outcome.
      this.flushLive(attempt);
      if (frame.outcome?.kind === 'committed' && frame.outcome.eventType === 'assistant/message' && Number.isSafeInteger(frame.outcome.seq)) {
        this.aliases.set(frame.outcome.seq, { text: attempt.items.text?.id, reasoning: attempt.items.reasoning?.id });
      } else {
        // An abandoned or failed attempt commits no message: close its live items as streamed.
        for (const [kind, item] of Object.entries(attempt.items)) {
          this.emit({ method: 'item/completed', params: { ...base, item: kind === 'text' ? { id: item.id, type: 'agentMessage', text: item.text } : { id: item.id, type: 'reasoning', summary: [], content: [item.text] } } });
        }
      }
      this.releaseHeld();
    }
  }

  emitLiveDelta(base, kind, item, delta) {
    if (!delta) return;
    this.emit(kind === 'text' ? { method: 'item/agentMessage/delta', params: { ...base, itemId: item.id, delta } }
      : { method: 'item/reasoning/textDelta', params: { ...base, itemId: item.id, delta, contentIndex: 0 } });
  }

  /** Emit what each live item of an attempt still holds back (its stream filter). */
  flushLive(attempt) {
    const base = { threadId: this.sessionId, turnId: attempt.turnId };
    for (const [kind, item] of Object.entries(attempt.items)) this.emitLiveDelta(base, kind, item, item.filter?.flush());
  }

  /** ACP path: record what each streamed item of the turn still holds back. */
  flushAcpDeltas(turnId) {
    for (const [id, filter] of this.acpFilters) {
      const delta = filter.flush();
      const item = this.items.get(id);
      if (delta && item) this.record(item.type === 'agentMessage' ? 'item/agentMessage/delta' : 'item/reasoning/textDelta',
        { threadId: this.sessionId, turnId, itemId: id, delta, ...(item.type === 'reasoning' ? { contentIndex: 0 } : {}) });
    }
    this.acpFilters.clear();
  }

  record(method, params) {
    const event = this.redact({ method, params });
    const seq = this.store.append(event);
    this.publish({ seq, ...event });
  }

  publish(event) {
    this.deliveries.observe(event);
    // Commit native facts immediately, but do not release a live terminal event
    // until both ACP and the native-log barrier have settled this prompt.
    if (event.method === 'turn/completed' && event.params.turn.id === this.turnId) this.pendingTerminal = event;
    else this.emit(event);
  }

  async initialize(mcpServers = []) {
    this.mcpServers = mcpServers;
    this.turnController?.abort();
    this.turnEpoch = Symbol('initialize');
    const resumed = Boolean(this.sessionId);
    // The latest persisted selection (launch, then every live change) wins
    // over the launch request: that only seeds a fresh session.
    const savedSelection = this.store.latest('dsh/launchSelection')?.params;
    // Native cold initialization can take almost 30s before the first reply.
    // Give that one handshake its own budget; ordinary RPCs keep their 30s
    // timeout and no request or prompt is retried after an uncertain write.
    const init = await this.rpc.request('initialize', { protocolVersion: 1, clientCapabilities: {}, clientInfo: { name: 'cutex-dsh-spike', version: '0.1.0' } }, 120000);
    if (init.protocolVersion !== 1) throw new Error('DSH did not negotiate ACP v1');
    // DSH admits inline images only when its configured model declares image input.
    this.imagePrompts = init.agentCapabilities?.promptCapabilities?.image === true;
    if (this.sessionId && !init.agentCapabilities?.sessionCapabilities?.resume) throw new Error('DSH cannot resume');
    const method = this.sessionId ? 'session/resume' : 'session/new';
    const identityMcp = typeof mcpServers === 'function';
    let session = await this.rpc.request(method, { ...(this.sessionId ? { sessionId: this.sessionId } : {}), cwd: this.cwd, mcpServers: identityMcp ? [] : mcpServers });
    this.sessionId = session.sessionId ?? this.sessionId;
    if (!this.sessionId) throw new Error('DSH did not return session identity');
    this.store.bind(this.sessionId, this.cwd);
    if (identityMcp) {
      await this.rpc.request('session/close', { sessionId: this.sessionId });
      session = await this.rpc.request('session/resume', { sessionId: this.sessionId, cwd: this.cwd, mcpServers: mcpServers(this.sessionId) });
    }
    await this.configureSelection(session.configOptions ?? [], { resumed, savedSelection });
    if (this.sessionsRoot) {
      // Catch up completely (an r264 cursor upgrade of a large log takes several bounded calls) before the first prompt.
      await this.syncNative({ drain: true });
      if (!this.poller) {
        this.poller = setInterval(() => { void this.syncNative().catch(error => { this.recoveryError = error; this.uncertain = true; }); }, 100);
        this.poller.unref();
      }
      return { init, sessionId: this.sessionId };
    }
    // A previously in-flight prompt cannot safely be repeated on restart.
    const history = this.store.replay();
    const started = history.filter(e => e.method === 'turn/started').at(-1);
    if (started && !history.some(e => e.method === 'turn/completed' && e.params.turn.id === started.params.turn.id)) {
      this.record('turn/completed', { threadId: this.sessionId, turn: { ...started.params.turn, status: 'interrupted', error: { message: 'Runtime restarted; partial history retained.' } } });
    }
    return { init, sessionId: this.sessionId };
  }

  async configureSelection(options, { savedSelection }) {
    this.configOptions = options;
    const model = savedSelection?.model ?? this.requestedModel;
    const effort = savedSelection ? savedSelection.reasoningEffort : this.requestedEffort;
    if (model === undefined && effort == null) return; // Legacy keyless spike keeps its fixture selection.
    // DSH restores a resumed session's last requested route; re-apply the
    // persisted selection when it differs (a live change not yet used).
    this.selection = await this.applySelection(model ?? DEFAULT_MODEL, effort ?? null);
    if (!savedSelection || savedSelection.modelValue !== this.selection.modelValue || savedSelection.effortValue !== this.selection.effortValue) {
      this.record('dsh/launchSelection', this.selection);
    }
  }

  /** Select model and effort through DSH's config options; returns the persisted selection shape. */
  async applySelection(model, effort) {
    if (typeof model !== 'string' || !model.trim()) throw Object.assign(new Error('invalid model'), { code: -32602 });
    if (effort != null && !['off', 'low', 'high', 'max'].includes(effort)) throw Object.assign(new Error('unsupported reasoning effort'), { code: -32602 });
    const select = async (category, wanted) => {
      const { option, value } = chooseOption(this.configOptions, category, wanted);
      if (option.currentValue !== value) {
        const result = await this.rpc.request('session/set_config_option', { sessionId: this.sessionId, configId: option.id, value });
        this.configOptions = result.configOptions ?? [];
        if (this.configOptions.find(entry => entry.id === option.id)?.currentValue !== value) throw new Error('DSH did not confirm selected config option');
      }
      return value;
    };
    const modelValue = await select('model', model);
    const effortValue = effort == null ? undefined : await select('thought_level', effort);
    return { model, reasoningEffort: effort ?? null, modelValue, ...(effortValue === undefined ? {} : { effortValue }) };
  }

  /**
   * A live model / effort change (`thread/settings/update`): applies from the
   * next turn (DSH pins a running turn's route), persists across restarts.
   */
  async updateSelection({ model, effort }) {
    if (!this.available) throw Object.assign(new Error('runtime unavailable'), { code: -32009 });
    const current = this.selection ?? { model: DEFAULT_MODEL, reasoningEffort: null };
    const next = await this.applySelection(model ?? current.model, effort === undefined ? current.reasoningEffort : effort);
    this.selection = next;
    this.record('dsh/launchSelection', next);
    return next;
  }

  /** The models DSH offers this session (its `model` config option), for `model/list`. */
  modelList() {
    const options = this.configOptions ?? [];
    const modelOption = options.find(option => option.category === 'model' && option.type === 'select');
    const effortOption = options.find(option => option.category === 'thought_level' && option.type === 'select');
    // DSH describes reasoning for the current model only; DeepSeek's models share it.
    const efforts = optionChoices(effortOption).filter(choice => choice.value !== '').map(choice => ({ reasoningEffort: String(choice.name ?? choice.value).toLowerCase(), description: choice.description ?? choice.name ?? '' }));
    const defaultEffort = efforts.some(effort => effort.reasoningEffort === 'high') ? 'high' : efforts[0]?.reasoningEffort ?? null;
    return optionChoices(modelOption).map(choice => {
      let id = choice.name;
      try { const route = JSON.parse(choice.value); if (Array.isArray(route) && typeof route[1] === 'string') id = route[1]; } catch {}
      const current = choice.value === modelOption.currentValue;
      return { id, model: id, displayName: choice.name ?? id, description: choice.description ?? '', hidden: false,
        // DSH reports image support for its configured model only; other models are listed as text.
        inputModalities: current && this.imagePrompts ? ['text', 'image'] : ['text'],
        supportedReasoningEfforts: efforts, defaultReasoningEffort: defaultEffort, isDefault: choice.value === modelOption.currentValue };
    });
  }

  /** Whether a new prompt may start now. */
  get available() { return Boolean(this.sessionId) && !this.uncertain && !this.closed && !this.rpc.dead && !this.recovering; }

  /**
   * Accept one cutex inter-agent message. It enters the next prompt; an idle
   * session starts that prompt now unless the message is passive.
   */
  deliver(params) {
    if (!this.available) throw Object.assign(new Error('runtime unavailable; delivery retried after restart'), { code: -32009 });
    const result = this.deliveries.submit(params);
    this.pumpDeliveries();
    return { submissionId: result.submissionId };
  }

  /**
   * An asks input (delegation, delegation result) with exclusive admission:
   * its own turn now, or a busy refusal that queues nothing (cutex retries).
   */
  submitInput(params) {
    if (!this.available) throw Object.assign(new Error('runtime unavailable'), { code: -32009 });
    // Waiting passive notes join this turn; a waiting wake would start its own.
    const busy = this.turnId ? 'active_turn' : this.deliveries.wantsTurn() ? 'pending_obligation' : null;
    const { entry, duplicate } = this.deliveries.submitInput(params, busy);
    if (!duplicate) {
      const task = this.prompt('');
      entry.turnId = this.turnId;
      task.catch(error => { this.lastDeliveryError = error; });
    }
    return this.inputStatus(params.messageId, duplicate);
  }

  /** The asks view of one input: its turn and how that turn went. */
  inputStatus(messageId, duplicate = false) {
    const entry = this.deliveries.entries.get(messageId);
    const persisted = this.deliveries.persisted.get(messageId);
    if (!entry?.input && !persisted) throw Object.assign(new Error('input not found'), { code: -32004 });
    const turnId = persisted?.turnId ?? entry?.turnId ?? null;
    let state = entry?.state === 'queued' ? 'queued' : entry?.state === 'failed' && !persisted ? 'failed' : 'processing';
    if (turnId && turnId !== this.turnId) {
      const terminal = this.store.turnEvents(turnId).filter(event => event.method === 'turn/completed').at(-1);
      if (terminal) state = terminal.params.turn.status === 'completed' ? 'completed' : terminal.params.turn.status === 'interrupted' ? 'interrupted' : 'failed';
    }
    return { messageId, turnId, state, contextPersisted: Boolean(persisted), duplicate };
  }

  /** Start a prompt for queued deliveries when the session is idle. */
  pumpDeliveries() {
    if (this.turnId || !this.available || !this.deliveries.wantsTurn()) return;
    const task = this.prompt('');
    task.catch(error => { this.lastDeliveryError = error; });
  }

  /**
   * `images`: decoded ACP image blocks; `inputs`: the owner's original image
   * inputs in the same order (`{type: 'localImage', path}` | `{type: 'image',
   * sha256}`), recorded for this turn (`dsh/turnInputs`) so the committed user
   * message shows the owner's upload as cute-codex does.
   */
  async prompt(text, images = [], inputs = []) {
    if (!this.sessionId) throw new Error('initialize first');
    if (this.uncertain) throw new Error('Previous prompt outcome unknown; restart before another prompt');
    if (this.turnId) throw new Error('DSH has no same-turn steer; queue this message');
    if (this.closed || this.rpc.dead || this.recovering) throw new Error('Runtime unavailable');
    // Every accepted inter-agent message enters this prompt, before the owner's text.
    const claimed = this.deliveries.claim();
    text = [...claimed.map(entry => entry.frame), ...(text ? [text] : [])].join('\n\n');
    if (!text) throw new Error('empty prompt');
    this.turnId = this.sessionsRoot ? `${this.sessionId}:turn:${this.store.nextNativeTurn(this.sessionId)}` : randomUUID();
    this.turnEpoch = Symbol('turn');
    this.turnController = new AbortController();
    this.items.clear();
    this.calls.clear();
    this.acpFilters.clear();
    const turnId = this.turnId;
    // Before session/prompt: DSH commits the user message during the prompt.
    if (this.sessionsRoot && inputs.length) this.store.append({ method: TURN_INPUTS, params: { threadId: this.sessionId, turnId, images: inputs } });
    if (!this.sessionsRoot) {
      this.record('turn/started', { threadId: this.sessionId, turn: { id: turnId, status: 'inProgress', items: [] } });
      this.record('item/completed', { threadId: this.sessionId, turnId, item: { id: randomUUID(), type: 'userMessage', content: [{ type: 'text', text }] } });
    }
    const rpc = this.rpc;
    const watchdog = this.watch(rpc, turnId);
    try {
      // No wall clock: DSH answers when the turn ends, or the watchdog
      // proves it dead (then the request rejects with the child's exit).
      const result = await rpc.request('session/prompt', { sessionId: this.sessionId, prompt: [{ type: 'text', text }, ...images.map(image => ({ type: 'image', data: image.data, mimeType: image.mimeType }))] }, null);
      if (this.sessionsRoot) {
        // ACP promise settles after DSH's durable turn; scan that authority,
        // rather than synthesizing a second completion from transport state.
        for (let attempt = 0; attempt < 300; attempt++) {
          await this.syncNative({ drain: true });
          if (this.store.turnCompleted(turnId)) return result;
          await new Promise(resolve => setTimeout(resolve, 30));
        }
        // DSH answered, so the turn is over; its native end is late. Close
        // it live only (history keeps the native end once it lands).
        this.closeLive(turnId, result.stopReason === 'cancelled' ? 'interrupted' : result.stopReason === 'end_turn' ? 'completed' : 'failed', null);
        return result;
      }
      this.flushAcpDeltas(turnId);
      for (const item of this.items.values()) if (item.type === 'agentMessage' || item.type === 'reasoning') this.record('item/completed', { threadId: this.sessionId, turnId, item });
      const status = result.stopReason === 'cancelled' ? 'interrupted' : result.stopReason === 'end_turn' ? 'completed' : 'failed';
      this.record('turn/completed', { threadId: this.sessionId, turn: { id: turnId, status, items: [...this.items.values()], dshStopReason: result.stopReason } });
      return result;
    } catch (error) {
      const message = this.livenessError?.message ?? error.message;
      // Index what native committed before deciding which messages it took.
      if (this.sessionsRoot) {
        await this.syncNative().catch(() => {});
        // The native log is the authority: a turn it started and never ended
        // gets its native end when DSH restarts; until then close it live, so
        // cutex never waits on a turn nobody runs.
        if (!this.pendingTerminal) this.closeLive(turnId, 'failed', message);
      } else {
        // Spike mode has no native authority: a dead transport leaves the
        // outcome unknown (an answered error does not).
        if (rpc.dead) this.uncertain = true;
        this.flushAcpDeltas(turnId);
        this.record('turn/completed', { threadId: this.sessionId, turn: { id: turnId, status: 'failed', items: [...this.items.values()], error: { message } } });
      }
      throw error;
    } finally {
      clearInterval(watchdog);
      if (this.held.length || this.live.size) this.releaseHeld(true);
      this.livenessError = undefined;
      this.turnController.abort();
      this.turnId = undefined;
      // Native never took a message it did not commit; it goes back to the queue.
      this.deliveries.release(claimed);
      const terminal = this.pendingTerminal;
      this.pendingTerminal = undefined;
      if (terminal && !this.closed) this.emit(terminal);
      if (rpc.dead && rpc === this.rpc && !this.closed) void this.recover();
      // Messages that arrived during this turn start the next one.
      else setImmediate(() => this.pumpDeliveries());
    }
  }

  /** A live-only terminal for a turn whose native end is not (yet) visible. */
  closeLive(turnId, status, message) {
    if (this.store.turnCompleted(turnId)) return;
    this.pendingTerminal = { method: 'turn/completed', params: { threadId: this.sessionId, turn: { id: turnId, status, items: [],
      ...(message ? { error: { message } } : {}), dshSynthetic: true } } };
  }

  /** The liveness watchdog of one turn (see constructor). */
  watch(rpc, turnId) {
    const { interval, probeTimeout, misses: limit } = this.liveness;
    let misses = 0, probing = false;
    const timer = setInterval(async () => {
      if (probing || rpc.dead || this.turnId !== turnId) return;
      if (Date.now() - rpc.lastActivity < interval) { misses = 0; return; }
      probing = true;
      try { await rpc.request('session/list', { cwd: this.cwd }, probeTimeout); misses = 0; }
      catch (error) { if (error.timedOut) misses++; else misses = 0; }
      finally { probing = false; }
      if (misses >= limit && !rpc.dead && this.turnId === turnId) {
        this.livenessError = new Error(`DSH stopped responding (${limit} liveness probes unanswered); restarting it`);
        rpc.kill();
      }
    }, interval);
    timer.unref?.();
    return timer;
  }

  /**
   * Restart a dead DSH child and resume the same native session, so the
   * agent takes messages again. Bounded retries; the native log is the
   * authority for what the dead child committed.
   */
  recover() {
    if (this.recovering || this.closed || !this.sessionId) return this.recovering;
    this.recovering = (async () => {
      for (const delay of this.liveness.restartDelays) {
        await new Promise(resolve => setTimeout(resolve, delay).unref?.());
        if (this.closed) return;
        try {
          this.rpc = this.spawn();
          await this.initialize(this.mcpServers ?? []);
          // A new child is a new transport: nothing is replayed into it, so
          // the dead child's unknown outcome no longer blocks new input.
          this.uncertain = false;
          this.recoveryError = undefined;
          return;
        } catch (error) {
          this.recoveryError = error;
          if (!this.rpc.dead) { this.rpc.kill(); await this.rpc.exited; }
        }
      }
    })().finally(() => {
      this.recovering = undefined;
      if (!this.closed && !this.rpc.dead) this.pumpDeliveries();
    });
    return this.recovering;
  }

  update({ sessionId, update }) {
    if (sessionId !== this.sessionId) throw new Error('update from unbound session');
    const base = { threadId: sessionId, turnId: this.turnId ?? null };
    if (this.sessionsRoot && !['usage_update', 'config_option_update'].includes(update.sessionUpdate)) {
      void this.syncNative().catch(error => { this.recoveryError = error; this.uncertain = true; });
      return;
    }
    // Retain the exact semantic update as well, including autonomous post-cancel updates.
    this.record('dsh/sessionUpdate', { ...base, update });
    const kind = update.sessionUpdate;
    if (kind === 'agent_message_chunk' || kind === 'agent_thought_chunk') {
      if (update.content?.type !== 'text') return;
      const id = `${kind}:${update.messageId ?? randomUUID()}`;
      let item = this.items.get(id);
      if (!item) {
        item = kind === 'agent_message_chunk' ? { id, type: 'agentMessage', text: '' } : { id, type: 'reasoning', summary: [], content: [] };
        this.items.set(id, item);
        this.record('item/started', { ...base, item });
      }
      // One text per item (content[0] for reasoning, as its deltas say): a
      // secret split over chunks is whole again where redaction sees it.
      if (item.type === 'agentMessage') item.text += update.content.text;
      else item.content[0] = (item.content[0] ?? '') + update.content.text;
      if (!this.acpFilters.has(id)) this.acpFilters.set(id, this.streamFilter());
      const delta = this.acpFilters.get(id).push(update.content.text);
      if (delta) this.record(item.type === 'agentMessage' ? 'item/agentMessage/delta' : 'item/reasoning/textDelta', { ...base, itemId: id, delta, ...(item.type === 'reasoning' ? { contentIndex: 0 } : {}) });
    } else if (kind === 'tool_call' || kind === 'tool_call_update') {
      // Same items as the native projection (toolItems.mjs), from ACP's generic tool updates.
      const id = update.toolCallId;
      if (kind === 'tool_call' || !this.calls.has(id)) this.calls.set(id, { name: update.title ?? id, arguments: update.rawInput ?? {} });
      const call = this.calls.get(id);
      if (update.status === 'completed' || update.status === 'failed') {
        const item = toolResultItem(call.name, call.arguments, id, { isError: update.status === 'failed', blocks: acpResultBlocks(update.content) });
        if (call.name === 'todo_write' && update.status === 'completed') {
          const plan = todoPlan(call.arguments?.todos);
          if (plan) this.record('turn/plan/updated', { ...base, explanation: null, plan });
        }
        if (item) { this.items.set(id, item); this.record('item/completed', { ...base, item }); }
      } else {
        const item = this.items.get(id) ?? toolStartItem(call.name, call.arguments, id);
        if (item) { this.items.set(id, item); this.record(kind === 'tool_call' ? 'item/started' : 'item/updated', { ...base, item }); }
      }
    } else if (kind === 'usage_update') {
      // ACP reports occupancy, not billing totals. Keep it explicit until the
      // daemon contract can represent unknown billing without inventing zeroes.
      if (Number.isFinite(update.used) && update.used >= 0 && Number.isFinite(update.size) && update.size > 0) {
        this.contextWindow = update.size;
        this.record('dsh/contextUsage/updated', { ...base, used: update.used, size: update.size, observedAt: new Date().toISOString(), source: 'acp' });
      }
    } else if (kind === 'config_option_update') {
      if (Array.isArray(update.configOptions)) this.configOptions = update.configOptions;
      this.record('dsh/configOptions/updated', { threadId: sessionId, configOptions: update.configOptions });
    }
  }

  /**
   * One DSH approval ask. cutex MCP tools are pre-approved (as for cute-codex
   * and Claude: their own policy runs in cutex). Everything else goes to the
   * owner and waits, without a clock, until answered or the turn ends.
   */
  async permission(params) {
    if (params.sessionId !== this.sessionId || !this.turnId) return { outcome: { outcome: 'cancelled' } };
    const sessionId = this.sessionId, turnId = this.turnId, epoch = this.turnEpoch;
    const controller = new AbortController();
    const turnSignal = this.turnController?.signal;
    const abort = () => controller.abort();
    turnSignal?.addEventListener('abort', abort, { once: true });
    if (turnSignal?.aborted) controller.abort();
    const active = () => !controller.signal.aborted && !this.closed && this.sessionId === sessionId && this.turnId === turnId && this.turnEpoch === epoch;
    const unavailable = new Promise(resolve => {
      controller.signal.addEventListener('abort', () => resolve(null), { once: true });
      if (controller.signal.aborted) resolve(null);
    });
    const callId = params.toolCall?.toolCallId;
    let answer;
    try {
      // DSH commits the tool call before it asks; wait briefly for that fact.
      if (this.sessionsRoot) {
        for (let attempt = 0; attempt < 100 && active() && !this.toolCall(callId); attempt++) {
          await Promise.race([this.syncNative(), unavailable]);
          if (!active()) return permissionOutcome(params, null);
          if (!this.toolCall(callId)) await Promise.race([new Promise(resolve => setTimeout(resolve, 20)), unavailable]);
        }
      }
      if (!active()) return permissionOutcome(params, null);
      const tool = this.toolCall(callId) ?? { id: callId, tool: params.toolCall?.title ?? 'tool', arguments: params.toolCall?.rawInput ?? {} };
      if (typeof tool.tool === 'string' && tool.tool.startsWith('mcp__cutex__')) return permissionOutcome(params, 'accept');
      const request = { threadId: sessionId, turnId, itemId: tool.id ?? callId, approvalId: randomUUID(), ...approvalText(tool, this.cwd), availableDecisions: ['accept', 'decline', 'cancel'] };
      answer = await Promise.race([this.approve({ method: 'item/commandExecution/requestApproval', params: request, signal: controller.signal }), unavailable]);
      if (!active()) answer = null;
    }
    catch { answer = null; }
    finally { controller.abort(); turnSignal?.removeEventListener('abort', abort); }
    return permissionOutcome(params, answer?.decision);
  }

  /** `{id, tool, arguments}` of a known call: its committed native fact, else its ACP update. */
  toolCall(callId) {
    if (typeof callId !== 'string') return undefined;
    const native = this.sessionsRoot && this.sessionId ? this.store?.nativeToolCall(this.sessionId, callId) : undefined;
    if (native) return { id: callId, tool: native.data.name, arguments: parseArguments(native.data.arguments) };
    const call = this.calls?.get(callId);
    return call ? { id: callId, tool: call.name, arguments: call.arguments } : undefined;
  }

  cancel() { this.turnController?.abort(); this.rpc.send({ method: 'session/cancel', params: { sessionId: this.sessionId } }); }
  /**
   * One bounded native-log scan; with `drain`, scan again while the reader
   * reports `more` (budget hit). Content problems are statuses, never throws;
   * the latest status is `nativeLog` (thread/read.dshNativeLog).
   */
  syncNative({ drain = false } = {}) {
    if (!this.sessionsRoot || !this.sessionId) return Promise.resolve();
    const scan = this.scans.then(async () => {
      const { locateNativeLog } = await import('./reconcile.mjs');
      const logPath = await locateNativeLog(this.sessionsRoot, this.sessionId);
      if (!logPath) return;
      this.catalog ??= await loadNativeCatalog(join(this.installRoot, 'package.json'));
      for (;;) {
        const result = await reconcileNativeLog({ logPath, sessionId: this.sessionId, catalog: this.catalog, store: this.store, cache: this.nativeScanCache });
        if (result?.status) this.nativeLog = result.status;
        if (!drain || !result?.more || this.closed) break;
      }
    });
    this.scans = scan.catch(() => {});
    return scan;
  }
  replay(after = 0) { return this.store.replay(after); }
  async close() {
    if (this.closed) return;
    this.closed = true;
    clearInterval(this.poller);
    this.turnController?.abort();
    try { if (this.sessionId && !this.rpc.dead) await this.rpc.request('session/close', { sessionId: this.sessionId }); }
    finally {
      try { await this.rpc.close(); await this.scans; if (this.sessionsRoot) await this.syncNative(); }
      finally { this.store.close(); }
    }
  }
}
