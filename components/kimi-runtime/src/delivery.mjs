import { createHash } from 'node:crypto';

/**
 * Cutex inter-agent delivery into a Kimi Code session (the legacy app-server
 * `thread/inter_agent_message` contract that the daemon's Bus bridge speaks
 * to cute-codex). ACP has no same-turn input: an accepted message waits in
 * this adapter and enters the next prompt. An idle session starts that
 * prompt at once unless the message is passive; passive notes wait for the
 * next turn, like Claude's notes. The native log stays the only proof of
 * context persistence: a status is `context_persisted` only once a committed
 * user message carries the message's frame.
 */
export const STATUS_SCHEMA = 'cutex/inter-agent-delivery-status/v1';
const HASH_DOMAIN = 'cutex:inter-agent-message-semantic:v1\0';
const MODES = new Set(['after_turn', 'soon', 'passive', 'interrupt']);
const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
const fault = (message, code = -32602) => Object.assign(new Error(message), { code });
const MARKER = /"cutexMessageId":"((?:[^"\\]|\\.){1,512})"/g;

function u64(value) { const bytes = Buffer.alloc(8); bytes.writeBigUInt64BE(BigInt(value)); return bytes; }

/** Byte-identical to `bus_bridge::inter_agent_semantic_sha256`. */
export function semanticSha256(params) {
  const hash = createHash('sha256').update(HASH_DOMAIN);
  const field = text => { const bytes = Buffer.from(text, 'utf8'); hash.update(u64(bytes.length)); hash.update(bytes); };
  field(params.messageId); field(params.author); field(params.recipient);
  const others = params.otherRecipients ?? [];
  hash.update(u64(others.length));
  for (const other of others) field(other);
  field(params.deliveryMode);
  field(params.content);
  return hash.digest('hex');
}

export function validateMessage(params) {
  const text = (value, name, max = 256) => {
    if (typeof value !== 'string' || !value.trim() || Buffer.byteLength(value) > max) throw fault(`invalid ${name}`);
  };
  text(params.messageId, 'messageId'); text(params.author, 'author'); text(params.recipient, 'recipient');
  text(params.content, 'content', 4 * 1024 * 1024);
  if (!MODES.has(params.deliveryMode)) throw fault('unsupported deliveryMode');
  if (params.otherRecipients !== undefined && !(Array.isArray(params.otherRecipients) && params.otherRecipients.every(value => typeof value === 'string'))) throw fault('invalid otherRecipients');
  for (const key of ['authorMetadata', 'recipientMetadata']) if (params[key] !== undefined && params[key] !== null && !object(params[key])) throw fault(`invalid ${key}`);
}

/** The model-visible frame, the same shape as Claude's EXTERNAL_INPUT_JSON. */
export function frame(params) {
  const metadata = object(params.authorMetadata) ? params.authorMetadata : {};
  const agent = typeof metadata.cutexSessionId === 'string' && metadata.cutexSessionId.length > 0;
  const name = typeof metadata.displayName === 'string' && metadata.displayName.trim() ? metadata.displayName.trim() : params.author;
  const eventType = params.content.startsWith('Message Type: ALARM') ? 'alarm' : agent ? 'agent_message' : 'service_event';
  return 'EXTERNAL_INPUT_JSON\n' + JSON.stringify({ source: { kind: agent ? 'agent' : 'service', id: name.slice(0, 256) }, eventType,
    cutexMessageId: params.messageId, delivery: params.deliveryMode, text: params.content });
}

/** The frame of a cutex input (asks: delegations and their results). */
export function inputFrame(params) {
  return 'EXTERNAL_INPUT_JSON\n' + JSON.stringify({ source: params.source, eventType: params.eventType,
    cutexMessageId: params.messageId, delivery: 'after_turn', text: params.text });
}

export function validateInput(params) {
  const text = (value, name, max = 256) => {
    if (typeof value !== 'string' || !value.trim() || Buffer.byteLength(value) > max) throw fault(`invalid ${name}`);
  };
  text(params.messageId, 'messageId'); text(params.eventType, 'eventType', 128);
  text(params.text, 'text', 4 * 1024 * 1024);
  if (!object(params.source) || !['agent', 'service'].includes(params.source.kind)) throw fault('invalid source');
  text(params.source.id, 'source.id');
  if (params.admission !== 'exclusive_turn') throw fault('unsupported admission');
}

/** Message ids whose frames a committed user message contains. */
export function framedMessageIds(item) {
  // r265: committed frames project as their own external_event items.
  if (item?.type === 'functionCallOutput' && item.name === 'external_event') return typeof item.cutexMessageId === 'string' ? [item.cutexMessageId] : [];
  if (item?.type !== 'userMessage') return [];
  const ids = [];
  for (const block of item.content ?? []) {
    if (block?.type !== 'text' || typeof block.text !== 'string' || !block.text.includes('EXTERNAL_INPUT_JSON')) continue;
    for (const match of block.text.matchAll(MARKER)) {
      try { ids.push(JSON.parse(`"${match[1]}"`)); } catch {}
    }
  }
  return ids;
}

export class DeliveryQueue {
  constructor() {
    /** messageId -> { params, digest, state: 'queued' | 'prompted', submissionId, order } */
    this.entries = new Map();
    /** messageId -> { turnId, seq } from committed user messages (durable via replay). */
    this.persisted = new Map();
    this.order = 0;
  }

  /** Index one committed timeline event (replayed at start, then live). */
  observe(event) {
    const item = event?.params?.item;
    if (event?.method !== 'item/completed' || !framedMessageIds(item).length) return;
    for (const id of framedMessageIds(item)) {
      if (!this.persisted.has(id)) this.persisted.set(id, { turnId: event.params.turnId, seq: event.seq ?? 0 });
      const entry = this.entries.get(id);
      if (entry) entry.state = 'persisted';
    }
  }

  /** Accept once per message id; the same id with other semantics is a conflict. */
  submit(params) {
    validateMessage(params);
    const digest = semanticSha256(params);
    const existing = this.entries.get(params.messageId);
    if (existing) {
      if (existing.digest !== digest) throw fault('messageId reused with different semantic content', -32010);
      return { submissionId: existing.submissionId, accepted: false };
    }
    const submissionId = `kimi-delivery:${digest.slice(0, 32)}`;
    const persisted = this.persisted.has(params.messageId);
    this.entries.set(params.messageId, { params, digest, submissionId, frame: frame(params), passive: params.deliveryMode === 'passive',
      order: ++this.order, state: persisted ? 'persisted' : 'queued' });
    return { submissionId, accepted: !persisted };
  }

  /**
   * One asks input with exclusive admission: the caller starts a dedicated
   * turn for it at once. `busy` refuses without queueing anything.
   */
  submitInput(params, busy) {
    validateInput(params);
    const digest = createHash('sha256').update(JSON.stringify([params.source.kind, params.source.id, params.eventType, params.text])).digest('hex');
    const existing = this.entries.get(params.messageId);
    if (existing) {
      if (existing.digest !== digest) throw fault('messageId reused with different semantic content', -32010);
      return { entry: existing, duplicate: true };
    }
    if (this.persisted.has(params.messageId)) return { entry: null, duplicate: true };
    if (busy) throw Object.assign(fault('the session is busy', -32009), { data: { externalInputError: 'busy', reason: busy } });
    const entry = { params, digest, frame: inputFrame(params), passive: false, input: true, order: ++this.order, state: 'queued' };
    this.entries.set(params.messageId, entry);
    return { entry, duplicate: false };
  }

  queued() { return [...this.entries.values()].filter(entry => entry.state === 'queued').sort((a, b) => a.order - b.order); }
  /** Whether an idle session must start a turn for the queued input. */
  wantsTurn() { return this.queued().some(entry => !entry.passive); }

  /** Claim every queued message for the next prompt; returns its frames in arrival order. */
  claim() {
    const claimed = this.queued();
    for (const entry of claimed) entry.state = 'prompted';
    return claimed;
  }

  /** A prompt that native never admitted returns its messages to the queue. */
  release(claimed) {
    // An asks input runs at most once: its failed admission is final there.
    for (const entry of claimed) if (entry.state === 'prompted' && !this.persisted.has(entry.params.messageId)) entry.state = entry.input ? 'failed' : 'queued';
  }

  status(threadId, queries) {
    if (!Array.isArray(queries) || queries.length > 256 || !queries.every(query => object(query) && typeof query.messageId === 'string' && /^[0-9a-f]{64}$/.test(query.semanticSha256 ?? ''))) throw fault('invalid status query');
    return { schema: STATUS_SCHEMA, threadId, statuses: queries.map(({ messageId, semanticSha256 }) => {
      const entry = this.entries.get(messageId);
      const base = { messageId, semanticSha256 };
      if (entry && entry.digest !== semanticSha256) return { ...base, state: 'conflict' };
      const persisted = this.persisted.get(messageId);
      if (persisted) {
        const receiptId = 'kimi-receipt:' + createHash('sha256').update(JSON.stringify([threadId, messageId, persisted.turnId])).digest('hex').slice(0, 40);
        return { ...base, state: 'context_persisted', receipt: { receiptId, threadId, messageId, ...(entry ? { semanticSha256 } : {}),
          responseItemId: messageId, turnId: persisted.turnId, rolloutOrdinal: persisted.seq } };
      }
      return { ...base, state: entry ? 'pending' : 'unknown' };
    }) };
  }
}
