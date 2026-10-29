import { randomUUID } from 'node:crypto';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { isAbsolute, resolve } from 'node:path';
import { AcpRpc } from './rpc.mjs';
import { TimelineStore, TURN_BINDING, TURN_INPUTS } from './store.mjs';
import { secretRedactor, streamRedactor, asSource, redactionSource } from './redaction.mjs';
import { DeliveryQueue } from './delivery.mjs';
import { TurnProjection, newTurnId } from './projection.mjs';
import { WireTail, MAIN_AGENT, usageOf, addUsage } from './wire.mjs';

// cutex runs fork / revert / delete through this component's one-shot session-ops.mjs.
const structureOps = existsSync(fileURLToPath(new URL('./session-ops.mjs', import.meta.url)));
export const capabilities = Object.freeze({ resume: true, interrupt: true, steer: false, fork: structureOps, revert: structureOps, delete: structureOps, threadSettingsUpdate: true });

/** Kimi's ACP modes for the agent's cutex permission settings (Kimi has no OS sandbox). */
export const MODES = new Set(['default', 'auto', 'yolo']);

/** The owner's decision as one of Kimi's offered options (approvals, plan reviews). */
export function permissionOutcome(params, decision) {
  const options = Array.isArray(params?.options) ? params.options : [];
  const pick = kinds => kinds.map(kind => options.find(option => option.kind === kind)).find(Boolean);
  const choice = decision === 'accept' ? pick(['allow_once']) : decision === 'acceptForSession' ? pick(['allow_always', 'allow_once'])
    : decision === 'decline' ? pick(['reject_once']) : undefined;
  return choice ? { outcome: { outcome: 'selected', optionId: choice.optionId } } : { outcome: { outcome: 'cancelled' } };
}

const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
const fault = (message, code = -32602, data) => Object.assign(new Error(message), { code, ...(data ? { data } : {}) });
const textOf = content => (Array.isArray(content) ? content : []).flatMap(entry => entry?.type === 'content' && entry.content?.type === 'text' ? [entry.content.text] : entry?.type === 'text' ? [entry.text] : []).join('\n');

/** The Codex / Claude timeline item of one delivered cutex input (`external_event`). */
export function externalItem(id, frameText) {
  let frame = {};
  try { frame = JSON.parse(frameText.replace(/^EXTERNAL_INPUT_JSON\n/, '')); } catch {}
  const type = frame.eventType === 'agent_message' ? 'message' : frame.eventType;
  return { id, type: 'functionCallOutput', name: 'external_event', namespace: 'external',
    output: JSON.stringify({ source: frame.source, type, text: frame.text }),
    ...(typeof frame.cutexMessageId === 'string' ? { cutexMessageId: frame.cutexMessageId } : {}) };
}

/** What the owner reads for one Kimi ask: the command, or the tool and what it does. */
export function approvalText(call, params, cwd) {
  const args = object(call?.rawInput) ? call.rawInput : object(call?.streamedArgs) ? call.streamedArgs : {};
  // Kimi 2.3.3 writes "Requesting approval to <action>", where the action is often a label ("Running: <command>"),
  // not a verb phrase: "Kimi asks to Running: …" would read wrong, so the reason names the action after a colon.
  const action = textOf(params?.toolCall?.content).replace(/^Requesting approval to /m, '').trim();
  const name = call?.name ?? params?.toolCall?.title ?? 'tool';
  const command = typeof args.command === 'string' ? args.command : `${name} ${JSON.stringify(args)}`;
  return { command, cwd: typeof args.cwd === 'string' ? args.cwd : cwd, reason: action ? `Kimi asks for approval: ${action}` : `Kimi asks to run ${name} once.` };
}

/** ACP controller of one Kimi Code session (`kimi acp`). */
export class KimiRuntime {
  constructor({ command, args, cwd, env, database, kimiHome, threadId, model, reasoningEffort, mode = 'default', approvalPolicy, sandboxReadOnly = false, imageModels,
    secrets = [], approve = async () => null, emit = () => {}, liveness = {}, stopGrace }) {
    if (!isAbsolute(cwd) || !isAbsolute(database)) throw new Error('absolute cwd and database required');
    if (!MODES.has(mode)) throw new Error('unsupported Kimi permission mode');
    this.cwd = resolve(cwd);
    this.kimiHome = kimiHome;
    this.threadId = threadId;
    this.emit = emit;
    this.approve = approve;
    this.mode = mode;
    this.approvalNever = approvalPolicy === 'never';
    // Read-only agents (sandbox read-only; `readOnly` is the history reader): Kimi's PreToolUse hook (config.toml, written by cutex)
    // refuses every tool outside the read set; whatever still asks is declined
    // here without an owner prompt (the owner cannot approve a write).
    this.sandboxReadOnly = sandboxReadOnly === true;
    this.imageModels = Array.isArray(imageModels) ? new Set(imageModels) : null;
    // `secrets`: a list, or the server's redaction source (its credential files
    // are re-read on change); the child's secret-named env values join it.
    const base = asSource(secrets);
    const envValues = redactionSource({ literals: Object.entries(env ?? {}).filter(([name]) => /TOKEN|SECRET|PASSWORD|API_KEY/i.test(name)).map(([, value]) => value) });
    const redacted = { literals: () => [...new Set([...base.literals(), ...envValues.literals()])].sort((a, b) => b.length - a.length), fingerprints: () => base.fingerprints() };
    this.redact = secretRedactor(redacted);
    // Live text / reasoning deltas: one chunk-safe filter per streamed item
    // (a secret split over two deltas never leaves in pieces).
    this.streamFilter = streamRedactor(redacted);
    this.streamFilters = new Map();
    this.requestedModel = model;
    this.requestedEffort = reasoningEffort;
    this.store = new TimelineStore(database);
    this.sessionId = this.store.binding()?.sessionId;
    const saved = this.store.binding();
    if (saved && resolve(saved.cwd) !== this.cwd) { this.store.close(); throw new Error('resume cwd differs from binding'); }
    this.deliveries = new DeliveryQueue();
    for (const event of this.store.iterate()) this.deliveries.observe(event);
    this.usageTotal = this.store.latest('kimi/tokenUsage')?.params.total;
    this.contextWindow = this.store.latest('kimi/contextUsage/updated')?.params.size;
    this.liveness = { interval: 30000, probeTimeout: 30000, misses: 2, restartDelays: [500, 2000, 5000, 15000, 30000], ...liveness };
    this.spawn = () => new AcpRpc(command, args, { cwd, env, stopGrace, update: p => this.update(p), permission: p => this.permission(p),
      onExit: dead => { if (dead === this.rpc && !this.closed && !this.turnId) void this.recover(); } });
    this.rpc = this.spawn();
  }

  record(method, params) {
    const event = this.redact({ method, params });
    const seq = this.store.append(event);
    this.publish({ seq, ...event });
  }
  publish(event) { this.deliveries.observe(event); this.emit(event); }
  live(method, params) { this.emit(this.redact({ method, params })); }

  async initialize(mcpServers = []) {
    this.mcpServers = mcpServers;
    this.turnController?.abort();
    const savedSelection = this.store.latest('kimi/launchSelection')?.params;
    const init = await this.rpc.request('initialize', { protocolVersion: 1,
      clientCapabilities: { fs: { readTextFile: false, writeTextFile: false }, terminal: false }, clientInfo: { name: 'cutex-kimi', version: '1' } }, 120000);
    if (init.protocolVersion !== 1) throw new Error('Kimi did not negotiate ACP v1');
    this.agentVersion = init.agentInfo?.version ?? null;
    this.imagePrompts = init.agentCapabilities?.promptCapabilities?.image === true;
    const resumed = Boolean(this.sessionId);
    if (resumed && !init.agentCapabilities?.sessionCapabilities?.resume) throw new Error('Kimi cannot resume');
    // Product launches know the facade thread id up front: the cutex MCP server gets it at session/new.
    const servers = typeof mcpServers === 'function' ? mcpServers(this.threadId ?? this.sessionId) : mcpServers;
    const session = resumed
      ? await this.rpc.request('session/resume', { sessionId: this.sessionId, cwd: this.cwd, mcpServers: servers }, 120000)
      : await this.rpc.request('session/new', { cwd: this.cwd, mcpServers: servers }, 120000);
    this.sessionId = session.sessionId ?? this.sessionId;
    if (!this.sessionId) throw new Error('Kimi did not return session identity');
    this.store.bind(this.sessionId, this.cwd);
    this.configOptions = session.configOptions ?? [];
    this.currentMode = session.modes?.currentModeId;
    // The agent's permission settings at every start (a resumed session keeps Kimi's last mode).
    if (this.currentMode !== this.mode) {
      await this.rpc.request('session/set_mode', { sessionId: this.sessionId, modeId: this.mode });
      this.currentMode = this.mode;
    }
    await this.configureSelection({ savedSelection });
    let offsets = {};
    try { offsets = JSON.parse(this.store.meta('wireOffsets') ?? '{}'); } catch {}
    this.wire = new WireTail({ kimiHome: this.kimiHome, sessionId: this.sessionId, offsets });
    this.syncWire();
    if (!this.poller) {
      this.poller = setInterval(() => { try { this.syncWire(); } catch {} }, 500);
      this.poller.unref();
    }
    // A turn the previous adapter never closed (crash) ends now; history keeps what it saw.
    const open = this.openTurn();
    if (open && open !== this.turnId) this.record('turn/completed', { threadId: this.sessionId, turn: { id: open, status: 'interrupted', items: [], error: { message: 'Runtime restarted; partial history retained.' } } });
    return { init, sessionId: this.sessionId };
  }

  openTurn() {
    const ids = this.store.turnIdPage({ limit: 1, direction: 'desc' });
    const last = ids[0];
    return last && !this.store.turnCompleted(last) ? last : null;
  }

  /** The model / thinking choices of this session (ACP config options). */
  option(category) { return (this.configOptions ?? []).find(option => option.category === category && option.type === 'select'); }
  async setOption(category, value) {
    const option = this.option(category);
    if (!option) throw fault(`this Kimi model has no ${category === 'thought_level' ? 'thinking effort' : category} choice`);
    const choices = (option.options ?? []).flatMap(entry => entry.options ?? [entry]);
    const match = choices.find(choice => choice.value === value) ?? choices.find(choice => String(choice.name).toLowerCase() === String(value).toLowerCase());
    if (!match) throw fault(`requested ${category === 'thought_level' ? 'reasoning effort' : category} is unavailable: ${value}`);
    if (option.currentValue === match.value) return match.value;
    const result = await this.rpc.request('session/set_config_option', { sessionId: this.sessionId, configId: option.id, value: match.value });
    if (Array.isArray(result?.configOptions)) this.configOptions = result.configOptions;
    return match.value;
  }

  async configureSelection({ savedSelection }) {
    const model = savedSelection?.model ?? this.requestedModel;
    const effort = savedSelection ? savedSelection.reasoningEffort : this.requestedEffort;
    this.selection = await this.applySelection(model ?? this.option('model')?.currentValue, effort ?? null);
    if (!savedSelection || savedSelection.model !== this.selection.model || savedSelection.reasoningEffort !== this.selection.reasoningEffort) {
      this.record('kimi/launchSelection', { threadId: this.sessionId, ...this.selection });
    }
  }

  async applySelection(model, effort) {
    if (typeof model !== 'string' || !model.trim()) throw fault('invalid model');
    const modelValue = await this.setOption('model', model);
    let effortValue = null;
    if (effort != null && effort !== '') effortValue = await this.setOption('thought_level', effort);
    else effortValue = this.option('thought_level')?.currentValue ?? null;
    return { model: modelValue, reasoningEffort: effortValue };
  }

  /** A live model / effort change: from the next turn on, persisted across restarts. */
  async updateSelection({ model, effort }) {
    if (!this.available) throw fault('runtime unavailable', -32009);
    if (this.turnId) throw fault('Kimi is busy; change the model when the turn has ended', -32009);
    const current = this.selection ?? {};
    // A new model starts on its own default effort unless one is named.
    const next = await this.applySelection(model ?? current.model, effort === undefined ? (model && model !== current.model ? null : current.reasoningEffort) : effort);
    this.selection = next;
    this.record('kimi/launchSelection', { threadId: this.sessionId, ...next });
    return next;
  }

  /** The models Kimi offers (its `model` config option), in the cute-codex `model/list` shape. */
  modelList() {
    const option = this.option('model');
    const thinking = this.option('thought_level');
    const efforts = (thinking?.options ?? []).map(choice => ({ reasoningEffort: choice.value, description: choice.name ?? choice.value }));
    return (option?.options ?? []).map(choice => {
      const current = choice.value === option.currentValue;
      const image = this.imageModels ? this.imageModels.has(choice.value) : this.imagePrompts;
      return { id: choice.value, model: choice.value, displayName: choice.name ?? choice.value, description: choice.description ?? '', hidden: false,
        inputModalities: image ? ['text', 'image'] : ['text'],
        // Kimi describes thinking for the current model only.
        supportedReasoningEfforts: current ? efforts : [], defaultReasoningEffort: current ? thinking?.currentValue ?? null : null, isDefault: current };
    });
  }

  get imagesAllowed() {
    if (!this.imagePrompts) return false;
    return this.imageModels ? this.imageModels.has(this.selection?.model) : true;
  }

  get available() { return Boolean(this.sessionId) && !this.closed && !this.rpc.dead && !this.recovering; }

  deliver(params) {
    if (!this.available) throw fault('runtime unavailable; delivery retried after restart', -32009);
    const result = this.deliveries.submit(params);
    this.pumpDeliveries();
    return { submissionId: result.submissionId };
  }

  submitInput(params) {
    if (!this.available) throw fault('runtime unavailable', -32009);
    const busy = this.turnId ? 'active_turn' : this.deliveries.wantsTurn() ? 'pending_obligation' : null;
    const { entry, duplicate } = this.deliveries.submitInput(params, busy);
    if (!duplicate) {
      const task = this.prompt('');
      entry.turnId = this.turnId;
      task.catch(error => { this.lastDeliveryError = error; });
    }
    return this.inputStatus(params.messageId, duplicate);
  }

  inputStatus(messageId, duplicate = false) {
    const entry = this.deliveries.entries.get(messageId);
    const persisted = this.deliveries.persisted.get(messageId);
    if (!entry?.input && !persisted) throw fault('input not found', -32004);
    const turnId = persisted?.turnId ?? entry?.turnId ?? null;
    let state = entry?.state === 'queued' ? 'queued' : entry?.state === 'failed' && !persisted ? 'failed' : 'processing';
    if (turnId && turnId !== this.turnId) {
      const terminal = this.store.turnEvents(turnId).filter(event => event.method === 'turn/completed').at(-1);
      if (terminal) state = terminal.params.turn.status === 'completed' ? 'completed' : terminal.params.turn.status === 'interrupted' ? 'interrupted' : 'failed';
    }
    return { messageId, turnId, state, contextPersisted: Boolean(persisted), duplicate };
  }

  pumpDeliveries() {
    if (this.turnId || !this.available || !this.deliveries.wantsTurn()) return;
    this.prompt('').catch(error => { this.lastDeliveryError = error; });
  }

  /**
   * One cutex turn = one ACP `session/prompt`. `images`: decoded ACP image
   * blocks; `inputs`: the owner's originals in the same order (a local
   * file's path, or a data URL's digest), shown in the user message.
   */
  async prompt(text, images = [], inputs = []) {
    if (!this.sessionId) throw new Error('initialize first');
    if (this.turnId) throw fault('Kimi is busy; steer unsupported', -32009);
    if (!this.available) throw fault('Runtime unavailable', -32009);
    const claimed = this.deliveries.claim();
    const prompt = [...claimed.map(entry => entry.frame), ...(text ? [text] : [])].join('\n\n');
    if (!prompt && !images.length) { this.deliveries.release(claimed); throw fault('empty prompt'); }
    const turnId = newTurnId();
    this.turnId = turnId;
    this.turnController = new AbortController();
    this.projection = new TurnProjection({ threadId: this.sessionId, turnId, cwd: this.cwd });
    this.turnKimi = undefined;
    this.turnEnded = undefined;
    const base = { threadId: this.sessionId, turnId };
    const userItems = [...claimed.map((entry, index) => externalItem(`${turnId}:external:${index}`, entry.frame))];
    if (text || images.length) {
      userItems.push({ id: `${turnId}:user`, type: 'userMessage', content: [...(text ? [{ type: 'text', text }] : []),
        ...inputs.map(input => input.type === 'localImage' ? { type: 'localImage', path: input.path } : { type: 'text', text: '[image]' })] });
    }
    this.record('turn/started', { threadId: this.sessionId, turn: { id: turnId, status: 'inProgress', items: [] } });
    if (inputs.length) this.store.append({ method: TURN_INPUTS, params: { ...base, images: inputs } });
    // The user items enter history once Kimi took the prompt (its first update, or its answer).
    let admitted = false;
    this.admit = () => {
      if (admitted) return;
      admitted = true;
      for (const item of userItems) this.record('item/completed', { ...base, item });
    };
    const rpc = this.rpc;
    const watchdog = this.watch(rpc, turnId);
    let status = 'failed', error = null;
    try {
      const result = await rpc.request('session/prompt', { sessionId: this.sessionId,
        prompt: [...(prompt ? [{ type: 'text', text: prompt }] : []), ...images.map(image => ({ type: 'image', data: image.data, mimeType: image.mimeType }))] }, null);
      this.admit();
      status = result?.stopReason === 'cancelled' ? 'interrupted' : result?.stopReason === 'end_turn' ? 'completed' : 'failed';
      if (result?.stopReason === 'refusal') error = { message: 'Kimi refused this turn (provider policy or a blocking hook).' };
      // Kimi answers `end_turn` for a failed turn too; its log says why.
      const ended = await this.awaitTurnEnd(rpc);
      if (ended?.reason === 'failed') { status = 'failed'; error = { message: this.redact(ended.error?.message ?? ended.error?.code ?? 'the Kimi turn failed') }; }
      if (ended?.reason === 'cancelled') status = 'interrupted';
      return result;
    } catch (failure) {
      const message = this.livenessError?.message ?? failure.message;
      // Kimi refused the prompt before running it (busy, auth): nothing was taken.
      if (rpc.dead || failure.code === undefined || admitted) this.admit();
      error = { message: this.redact(message) };
      status = this.turnController.signal.aborted && !rpc.dead ? 'interrupted' : 'failed';
      throw failure;
    } finally {
      clearInterval(watchdog);
      this.livenessError = undefined;
      for (const event of this.projection.finish({ interrupted: status === 'interrupted' })) this.emitProjected(event);
      this.syncWire();
      this.record('turn/completed', { threadId: this.sessionId, turn: { id: turnId, status, items: [], ...(error ? { error } : {}) } });
      this.turnController.abort();
      this.streamFilters.clear();
      this.turnId = undefined;
      this.projection = undefined;
      this.admit = undefined;
      this.deliveries.release(claimed);
      if (rpc.dead && rpc === this.rpc && !this.closed) void this.recover();
      else setImmediate(() => this.pumpDeliveries());
    }
  }

  /** Kimi writes `turn.ended` just after it answers; wait briefly for that fact. */
  async awaitTurnEnd(rpc) {
    for (let attempt = 0; attempt < 40 && !this.turnEnded; attempt++) {
      this.syncWire();
      if (this.turnEnded || rpc.dead) break;
      await new Promise(done => setTimeout(done, 25));
    }
    return this.turnEnded;
  }

  emitProjected(event) {
    const { method, params } = event;
    if (method === 'item/agentMessage/delta' || method === 'item/reasoning/textDelta') {
      let filter = this.streamFilters.get(params.itemId);
      if (!filter) { filter = this.streamFilter(); this.streamFilters.set(params.itemId, filter); }
      const delta = filter.push(params.delta);
      if (delta) this.live(method, { ...params, delta });
      return;
    }
    // A streamed item completes: what its filter still holds goes out first.
    if (method === 'item/completed' && this.streamFilters.has(params.item?.id)) this.flushStream(params);
    if (event.store) this.record(method, params);
    else this.live(method, params);
  }

  /** Emit the tail an item's stream filter held back (the item ended). */
  flushStream({ threadId, turnId, item }) {
    const filter = this.streamFilters.get(item.id);
    this.streamFilters.delete(item.id);
    const tail = filter?.flush();
    if (!tail) return;
    if (item.type === 'agentMessage') this.live('item/agentMessage/delta', { threadId, turnId, itemId: item.id, delta: tail });
    else this.live('item/reasoning/textDelta', { threadId, turnId, itemId: item.id, delta: tail, contentIndex: 0 });
  }

  /** New records of Kimi's session logs: Kimi turn numbers, failures, billing usage. */
  syncWire() {
    if (!this.wire) return;
    const changed = this.wire.poll((agent, record) => {
      if (agent === MAIN_AGENT && record.type === 'turn.prompt' && this.turnId && this.turnKimi === undefined) {
        this.turnKimi = record.turnId;
        this.store.append({ method: TURN_BINDING, params: { threadId: this.sessionId, turnId: this.turnId, kimiTurnId: record.turnId, promptId: record.promptId ?? null } });
      } else if (agent === MAIN_AGENT && record.type === 'turn.ended' && this.turnId && record.turnId === this.turnKimi) {
        this.turnEnded = record;
      } else if (record.type === 'usage.record') {
        this.noteUsage(agent, record);
      }
    });
    if (changed) this.store.setMeta('wireOffsets', JSON.stringify(this.wire.offsets));
  }

  /** One billed model request (main agent or a subagent), as cute-codex reports usage. */
  noteUsage(agent, record) {
    const last = usageOf(record);
    if (!last) return;
    const total = addUsage(this.usageTotal, last);
    this.usageTotal = total;
    const turnId = this.turnId ?? this.store.turnIdPage({ limit: 1, direction: 'desc' })[0] ?? null;
    this.store.append({ method: 'kimi/tokenUsage', params: { threadId: this.sessionId, turnId, agent, model: record.model ?? null, total, last } });
    if (!turnId) return;
    this.live('thread/tokenUsage/updated', { threadId: this.sessionId, turnId,
      tokenUsage: { total, last, ...(Number.isSafeInteger(this.contextWindow) && this.contextWindow > 0 ? { modelContextWindow: this.contextWindow } : {}) } });
  }

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
        this.livenessError = new Error(`Kimi stopped responding (${limit} liveness probes unanswered); restarting it`);
        rpc.kill();
      }
    }, interval);
    timer.unref?.();
    return timer;
  }

  recover() {
    if (this.recovering || this.closed || !this.sessionId) return this.recovering;
    this.recovering = (async () => {
      for (const delay of this.liveness.restartDelays) {
        await new Promise(done => setTimeout(done, delay).unref?.());
        if (this.closed) return;
        try {
          this.rpc = this.spawn();
          this.recoveringInit = true;
          await this.initialize(this.mcpServers ?? []);
          this.recoveryError = undefined;
          return;
        } catch (error) {
          this.recoveryError = error;
          if (!this.rpc.dead) { this.rpc.kill(); await this.rpc.exited; }
        } finally { this.recoveringInit = false; }
      }
    })().finally(() => {
      this.recovering = undefined;
      if (!this.closed && !this.rpc.dead) this.pumpDeliveries();
    });
    return this.recovering;
  }

  update({ sessionId, update }) {
    if (sessionId !== this.sessionId || !object(update)) return;
    const kind = update.sessionUpdate;
    if (kind === 'usage_update') {
      if (Number.isFinite(update.used) && update.used >= 0 && Number.isFinite(update.size) && update.size > 0) {
        this.contextWindow = update.size;
        this.record('kimi/contextUsage/updated', { threadId: sessionId, turnId: this.turnId ?? null, used: update.used, size: update.size, observedAt: new Date().toISOString(), source: 'acp' });
      }
      return;
    }
    if (kind === 'config_option_update') {
      if (Array.isArray(update.configOptions)) this.configOptions = update.configOptions;
      return;
    }
    if (kind === 'current_mode_update') { this.currentMode = update.currentModeId; return; }
    if (!this.projection) return;
    this.admit?.();
    for (const event of this.projection.update(update)) this.emitProjected(event);
  }

  /**
   * One Kimi ask (tool approval, plan review, question). cutex MCP tools are
   * pre-approved (their own policy runs in cutex); an agent whose approval
   * policy is `never` gets a refusal without a prompt; everything else waits
   * for the owner (no clock) until answered or the turn ends.
   */
  async permission(params) {
    if (params?.sessionId !== this.sessionId || !this.turnId) return { outcome: { outcome: 'cancelled' } };
    const options = Array.isArray(params.options) ? params.options : [];
    const turnId = this.turnId, signal = this.turnController.signal;
    const toolCallId = params.toolCall?.toolCallId;
    const call = this.projection?.tools.get(toolCallId);
    const title = params.toolCall?.title;
    const name = call?.name ?? title;
    if (typeof name === 'string' && name.startsWith('mcp__cutex__')) return permissionOutcome(params, 'accept');
    // Kimi's question bridge (`q<n>_*` options): cutex agents do not ask the owner mid-turn questions.
    if (options.length && options.every(option => /^q\d+_/.test(option.optionId))) {
      const skip = options.find(option => option.kind === 'reject_once');
      return skip ? { outcome: { outcome: 'selected', optionId: skip.optionId } } : { outcome: { outcome: 'cancelled' } };
    }
    if (this.approvalNever || this.sandboxReadOnly) return permissionOutcome(params, 'decline');
    const plan = options.some(option => /^plan_/.test(option.optionId));
    const fileChange = !plan && call && (this.projection.items.get(toolCallId)?.type === 'fileChange');
    const base = { threadId: this.sessionId, turnId, itemId: toolCallId ?? randomUUID(), approvalId: randomUUID() };
    const request = plan
      ? { method: 'item/commandExecution/requestApproval', params: { ...base, command: 'ExitPlanMode', cwd: this.cwd, reason: `Kimi asks to carry out its plan:\n${textOf(params.toolCall?.content)}`, availableDecisions: ['accept', 'decline', 'cancel'] } }
      : fileChange
        ? { method: 'item/fileChange/requestApproval', params: { ...base, reason: approvalText(call, params, this.cwd).reason, grantRoot: null, availableDecisions: ['accept', 'acceptForSession', 'decline', 'cancel'] } }
        : { method: 'item/commandExecution/requestApproval', params: { ...base, ...approvalText(call, params, this.cwd), availableDecisions: ['accept', 'acceptForSession', 'decline', 'cancel'] } };
    let answer = null;
    try { answer = await this.approve({ ...request, signal }); }
    catch { answer = null; }
    if (signal.aborted || this.turnId !== turnId) return { outcome: { outcome: 'cancelled' } };
    return permissionOutcome(params, answer?.decision);
  }

  cancel() {
    this.turnController?.abort();
    try { this.rpc.send({ method: 'session/cancel', params: { sessionId: this.sessionId } }); } catch {}
  }

  async close() {
    if (this.closed) return;
    this.closed = true;
    clearInterval(this.poller);
    this.turnController?.abort();
    try { await this.rpc.close(); }
    finally {
      try { this.syncWire(); } catch {}
      this.store.close();
    }
  }
}
