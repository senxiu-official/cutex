/** Pure projection of committed native facts; no stream chunks or usage estimates. */
import { acpResultBlocks, parseArguments, todoPlan, toolResultItem, toolStartItem } from './toolItems.mjs';

export const nativeTurnId = (sessionId, turn) => `${sessionId}:turn:${turn}`;

const object = value => value !== null && typeof value === 'object' && !Array.isArray(value);
const validTurn = value => Number.isSafeInteger(value) && value >= 1;

export function nativeTurnNumber(event, currentTurn = null) {
  const value = event.data?.turn;
  // A malformed or absent native turn number must not brick a long-lived reader:
  // fall back to the preceding durable turn/start (DSH user/message.data IS the
  // UserMessage and has no turn field at all).
  if (validTurn(value)) return value;
  return validTurn(currentTurn) ? currentTurn : null;
}

function kilobytes(bytes) {
  if (!Number.isSafeInteger(bytes) || bytes < 0) return null;
  return bytes < 1024 ? `${bytes} B` : bytes < 1024 * 1024 ? `${(bytes / 1024).toFixed(1)} KB` : `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}
const safeName = value => typeof value === 'string' && value.length ? value.slice(0, 200) : null;

/**
 * Text placeholder for a DSH attachment block (image/file). DSH stores image
 * bytes in its attachment service (attachment.attachmentId), not in the log, so
 * a log reader cannot inline them; Codex/Claude timelines likewise show a text
 * placeholder when image bytes are not available. Metadata only, no paths.
 */
export function attachmentPlaceholder(block) {
  const attachment = object(block?.attachment) ? block.attachment : {};
  if (block?.type === 'image') {
    const parts = [typeof attachment.mediaType === 'string' ? attachment.mediaType : 'image'];
    if (Number.isSafeInteger(attachment.width) && Number.isSafeInteger(attachment.height)) parts.push(`${attachment.width}x${attachment.height}`);
    const size = kilobytes(attachment.bytes);
    if (size) parts.push(size);
    const name = safeName(attachment.name);
    return `[Image${name ? ` ${name}` : ''}: ${parts.join(', ')}${block.offloaded === true ? ', offloaded' : ''}]`;
  }
  if (block?.type === 'file') {
    const name = safeName(attachment.name);
    const size = kilobytes(attachment.bytes);
    return `[File${name ? ` ${name}` : ''}${size ? `: ${size}` : ''}]`;
  }
  return `[Unsupported content${typeof block?.type === 'string' ? `: ${block.type.slice(0, 64)}` : ''}]`;
}
/** Display metadata kept beside a placeholder (never bytes or filesystem paths). */
export function attachmentMetadata(block) {
  const attachment = object(block?.attachment) ? block.attachment : {};
  const result = { kind: typeof block?.type === 'string' ? block.type : 'unknown' };
  for (const key of ['attachmentId', 'mediaType', 'bytes', 'width', 'height', 'name']) {
    if (typeof attachment[key] === 'string' || Number.isSafeInteger(attachment[key])) result[key] = attachment[key];
  }
  if (block?.offloaded === true) result.offloaded = true;
  return result;
}

/**
 * Whether a committed DSH user/message is the owner's (or cutex's delivered)
 * input. DSH also commits user-role messages it injects itself, each with its
 * producer's `data.source.kind` (`runtime-context`: "Current runtime
 * context..."; `model-selection`: "[model changed: ...]"; DSH's MessageSource
 * is merge-extensible). Those are model context, not chat messages: like
 * cute-codex's injected context they stay raw in native_events and are not
 * projected. ACP prompts are `{kind: 'user'}`; a message without a source kind
 * is shown (never lose input). Any other event type is not affected.
 */
export function isOwnerInput(event) {
  if (event?.type !== 'user/message') return true;
  const kind = event.data?.source?.kind;
  return typeof kind !== 'string' || kind === 'user';
}

/** Native content blocks as a list; malformed content is an empty list, never an exception. */
export const nativeBlocks = content => Array.isArray(content) ? content.filter(object) : [];

/**
 * The owner's original image of each image block, as cute-codex echoes it
 * (`{type: 'localImage', path}`), when the turn's recorded inputs
 * (`dsh/turnInputs`, see TimelineStore) match the message's image blocks one
 * to one; otherwise none (every block stays a placeholder).
 */
function originalImages(blocks, inputs) {
  const images = blocks.filter(block => block.type === 'image');
  if (!Array.isArray(inputs) || !images.length || inputs.length !== images.length) return new Map();
  return new Map(images.map((block, index) => [block, inputs[index]])
    .filter(([, input]) => input?.type === 'localImage' && typeof input.path === 'string' && input.path.length > 0));
}

/**
 * DSH user content (text/image/file blocks) as userMessage UserInput entries.
 * An image the owner sent as a local file (`inputs`, recorded per turn) is
 * that `localImage`, so clients draw its thumbnail as for cute-codex;
 * otherwise (data URLs, forks, deleted files, older timelines) a text
 * placeholder with metadata.
 */
export function userContent(content, inputs) {
  const result = [];
  const blocks = nativeBlocks(content);
  const originals = originalImages(blocks, inputs);
  for (const block of blocks) {
    if (block.type === 'text') { if (typeof block.text === 'string') result.push({ type: 'text', text: block.text }); }
    else if (originals.has(block)) result.push({ type: 'localImage', path: originals.get(block).path, dshAttachment: attachmentMetadata(block) });
    else if (block.type === 'image' || block.type === 'file') result.push({ type: 'text', text: attachmentPlaceholder(block), dshAttachment: attachmentMetadata(block) });
    else if (block.type !== 'reasoning') result.push({ type: 'text', text: attachmentPlaceholder(block) });
  }
  return result;
}

const EXTERNAL_MARKER = 'EXTERNAL_INPUT_JSON\n';
/**
 * Split cutex's external-input frames (delivered agent messages, delegations,
 * results, alarms: `EXTERNAL_INPUT_JSON\n<one-line JSON>`, joined by a blank
 * line, before the owner's text) off the front of a committed user text.
 */
export function splitExternalFrames(text) {
  const frames = [];
  let rest = typeof text === 'string' ? text : '';
  while (rest.startsWith(EXTERNAL_MARKER)) {
    const body = rest.slice(EXTERNAL_MARKER.length);
    const end = body.indexOf('\n');
    let frame;
    try { frame = JSON.parse(end < 0 ? body : body.slice(0, end)); } catch { break; }
    if (!object(frame)) break;
    frames.push(frame);
    rest = end < 0 ? '' : body.slice(end + 1);
    if (rest.startsWith('\n')) rest = rest.slice(1);
  }
  return { frames, rest };
}

/** The Codex / Claude timeline item of one external input (cutex `external_event`). */
export function externalItem(id, frame) {
  const type = frame.eventType === 'agent_message' ? 'message' : frame.eventType;
  return { id, type: 'functionCallOutput', name: 'external_event', namespace: 'external',
    output: JSON.stringify({ source: frame.source, type, text: frame.text }),
    ...(typeof frame.cutexMessageId === 'string' ? { cutexMessageId: frame.cutexMessageId } : {}) };
}

/** A committed user message: its external inputs as their own items, then the owner's part (if any). */
function userItems(id, content, inputs) {
  const blocks = userContent(content, inputs);
  const first = blocks[0];
  const { frames, rest } = first?.type === 'text' && !first.dshAttachment ? splitExternalFrames(first.text) : { frames: [] };
  if (!frames.length) return [{ id, type: 'userMessage', content: blocks }];
  const remaining = [...(rest.trim() ? [{ type: 'text', text: rest }] : []), ...blocks.slice(1)];
  return [...frames.map((frame, index) => externalItem(`${id}:external:${index}`, frame)),
    ...(remaining.length ? [{ id, type: 'userMessage', content: remaining }] : [])];
}

const eventTime = event => Number.isSafeInteger(event?.time) ? event.time : null;
/** A committed tool call's name and parsed arguments (null when the call is not visible). */
function callOf(call) {
  if (!call || call.type !== 'tool/call' || !object(call.data) || typeof call.data.name !== 'string') return null;
  return { name: call.data.name, args: parseArguments(call.data.arguments) };
}

/** context is reconstructed by TimelineStore from transactionally saved facts. */
export function projectNativeEvent({ sessionId, event, updates }, { turn, toolCall, items = [], inputs } = {}) {
  if (!['turn/start', 'user/message', 'assistant/message', 'tool/call', 'tool/result', 'todo/write', 'turn/end'].includes(event.type)) return [];
  // Facts outside any committed turn (e.g. an inherited fork seed) stay raw in
  // native_events; projecting them would need an invented turn.
  if (!validTurn(turn)) return [];
  const data = object(event.data) ? event.data : {};
  const base = { threadId: sessionId, turnId: nativeTurnId(sessionId, turn) };
  const itemEvent = (method, item) => ({ method, params: { ...base, item } });
  if (event.type === 'turn/start') return [{ method: 'turn/started', params: {
    threadId: sessionId, turn: { id: base.turnId, status: 'inProgress', items: [] },
  } }];
  if (event.type === 'user/message') {
    if (typeof data.id !== 'string') return [];
    // DSH-injected context (runtime-context snapshot, model-selection note,
    // ...) is model input, not something the owner wrote; Codex keeps its
    // environment context out of the timeline as well.
    if (!isOwnerInput(event)) return [];
    return userItems(data.id, data.content, inputs).map(item => itemEvent('item/completed', item));
  }
  if (event.type === 'assistant/message') {
    const message = object(data.message) ? data.message : {};
    const id = message.id;
    if (typeof id !== 'string') return [];
    const chunks = nativeBlocks(message.content);
    const text = chunks.filter(block => block.type === 'text' && typeof block.text === 'string').map(block => block.text).join('');
    const reasoning = chunks.filter(block => block.type === 'reasoning' && typeof block.text === 'string').map(block => block.text).join('');
    const attachments = chunks.filter(block => block.type === 'image' || block.type === 'file').map(attachmentPlaceholder);
    const shown = [text, ...attachments].filter(Boolean).join('\n');
    const result = [];
    // Completion snapshots contain the entire native message; never append
    // deltas as well, which would double its text during server reconstruction.
    const kinds = [...new Set(chunks.map(block => block.type === 'image' || block.type === 'file' ? 'text' : block.type))];
    for (const kind of kinds) {
      if (kind === 'text' && shown) result.push(itemEvent('item/completed', { id: `agent_message_chunk:${id}`, type: 'agentMessage', text: shown }));
      if (kind === 'reasoning' && reasoning) result.push(itemEvent('item/completed', { id: `agent_thought_chunk:${id}`, type: 'reasoning', summary: [], content: [reasoning] }));
    }
    return result;
  }
  if (event.type === 'tool/call') {
    const call = callOf(event);
    if (typeof data.callId !== 'string' || !call) return [];
    const item = toolStartItem(call.name, call.args, data.callId);
    return item ? [itemEvent('item/started', item)] : [];
  }
  if (event.type === 'tool/result') {
    const callId = object(data.message) ? data.message.toolCallId : undefined;
    const update = updates.find(update => update.sessionUpdate === 'tool_call_update');
    if (!update || update.toolCallId !== callId) return [];
    // Robust history: a result without a visible call (e.g. damaged or seeded
    // prefix) is still shown rather than refusing every later event.
    const call = callOf(toolCall) ?? { name: 'unknown', args: {} };
    const started = eventTime(toolCall), finished = eventTime(event);
    const item = toolResultItem(call.name, call.args, callId, { isError: update.status === 'failed',
      blocks: acpResultBlocks(update.content), meta: data.meta,
      durationMs: started !== null && finished !== null ? finished - started : null });
    return item ? [itemEvent('item/completed', item)] : [];
  }
  if (event.type === 'todo/write') {
    // DSH's checklist (todo_write) as Codex `update_plan` / Claude `TodoWrite`.
    const plan = todoPlan(data.todos);
    return plan ? [{ method: 'turn/plan/updated', params: { ...base, explanation: null, plan } }] : [];
  }
  const reason = data.reason;
  const kind = reason?.kind;
  const status = kind === 'completed' ? 'completed'
    : ['aborted', 'interrupted', 'forked'].includes(kind) ? 'interrupted' : 'failed';
  return [{ method: 'turn/completed', params: { threadId: sessionId, turn: {
    id: base.turnId, status, items, dshEndReason: reason,
    ...(reason?.error ? { error: reason.error } : {}),
  } } }];
}
