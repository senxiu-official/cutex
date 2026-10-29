import { randomUUID } from 'node:crypto';
import { isToolName, planSteps, toolName, toolResultItem, toolStartItem } from './toolItems.mjs';

/**
 * Kimi's ACP stream of one turn as Codex v2 thread events. Kimi streams
 * text and thinking as anonymous chunks (no message id), so consecutive
 * chunks of one kind form one item until another kind (or a tool call)
 * starts. Each event says whether it is durable (`store`) or live only
 * (`item/started` of text, deltas, tool progress): history keeps completed
 * items, live clients see the stream.
 */
export class TurnProjection {
  constructor({ threadId, turnId, cwd }) {
    this.base = { threadId, turnId };
    this.cwd = cwd;
    this.open = null;
    this.count = 0;
    this.tools = new Map();
    this.items = new Map();
    this.plan = null;
  }

  #close(out) {
    const open = this.open;
    if (!open) return;
    this.open = null;
    const item = open.kind === 'text' ? { id: open.id, type: 'agentMessage', text: open.text } : { id: open.id, type: 'reasoning', summary: [], content: [open.text] };
    this.items.set(item.id, item);
    out.push({ store: true, method: 'item/completed', params: { ...this.base, item } });
  }

  /** One ACP `session/update`; returns the events it produces. */
  update(update) {
    const out = [];
    const kind = update?.sessionUpdate;
    if (kind === 'agent_message_chunk' || kind === 'agent_thought_chunk') {
      if (update.content?.type !== 'text' || typeof update.content.text !== 'string') return out;
      const segment = kind === 'agent_message_chunk' ? 'text' : 'reasoning';
      if (this.open?.kind !== segment) {
        this.#close(out);
        const id = `${this.base.turnId}:${segment === 'text' ? 'msg' : 'reasoning'}:${++this.count}`;
        this.open = { kind: segment, id, text: '' };
        out.push({ store: false, method: 'item/started', params: { ...this.base, item: segment === 'text' ? { id, type: 'agentMessage', text: '' } : { id, type: 'reasoning', summary: [], content: [] } } });
      }
      this.open.text += update.content.text;
      out.push(segment === 'text'
        ? { store: false, method: 'item/agentMessage/delta', params: { ...this.base, itemId: this.open.id, delta: update.content.text } }
        : { store: false, method: 'item/reasoning/textDelta', params: { ...this.base, itemId: this.open.id, delta: update.content.text, contentIndex: 0 } });
      return out;
    }
    if (kind === 'tool_call' || kind === 'tool_call_update') {
      const id = update.toolCallId;
      if (typeof id !== 'string' || !id) return out;
      let call = this.tools.get(id);
      if (!call) {
        this.#close(out);
        call = { id, name: isToolName(update.title) ? update.title : undefined, title: update.title, kind: update.kind, rawInput: update.rawInput, startContent: undefined, started: false, done: false };
        this.tools.set(id, call);
      }
      if (isToolName(update.title) && !call.name) call.name = update.title;
      if (update.title) call.title = update.title;
      if (update.kind) call.kind = update.kind;
      if (update.rawInput !== undefined) call.rawInput = update.rawInput;
      if (Array.isArray(update.content) && update.status !== 'completed' && update.status !== 'failed') {
        call.startContent = update.content;
        // While the model streams the arguments, ACP carries them as text only (an ask may come first).
        if (call.rawInput === undefined) {
          const text = update.content.find(entry => entry?.type === 'content' && entry.content?.type === 'text')?.content.text;
          try { const parsed = JSON.parse(text); if (parsed && typeof parsed === 'object') call.streamedArgs = parsed; } catch {}
        }
      }
      if (call.done) return out;
      const name = toolName(call);
      if (update.status === 'completed' || update.status === 'failed') {
        call.done = true;
        const item = toolResultItem(name, call.rawInput, id, { isError: update.status === 'failed', content: update.content, rawOutput: update.rawOutput, startContent: call.startContent, cwd: this.cwd });
        if (item) { this.items.set(id, item); out.push({ store: true, method: 'item/completed', params: { ...this.base, item } }); }
        return out;
      }
      // Arguments still streaming: nothing to show until Kimi knows them.
      if (call.rawInput === undefined) return out;
      const item = toolStartItem(name, call.rawInput, id, { cwd: this.cwd, content: call.startContent });
      if (!item) return out;
      this.items.set(id, item);
      out.push({ store: !call.started, method: call.started ? 'item/updated' : 'item/started', params: { ...this.base, item } });
      call.started = true;
      return out;
    }
    if (kind === 'plan') {
      const plan = planSteps(update.entries);
      if (plan) {
        this.plan = plan;
        out.push({ store: true, method: 'turn/plan/updated', params: { ...this.base, explanation: null, plan } });
      }
    }
    return out;
  }

  /** The turn ended: close the open segment and any call Kimi never finished. */
  finish({ interrupted = false } = {}) {
    const out = [];
    this.#close(out);
    for (const call of this.tools.values()) {
      if (call.done || !call.started) continue;
      const started = this.items.get(call.id);
      // An interrupted command / edit never ran to its end (`declined`); others failed.
      const item = { ...started, status: interrupted && (started.type === 'commandExecution' || started.type === 'fileChange') ? 'declined' : 'failed' };
      this.items.set(call.id, item);
      out.push({ store: true, method: 'item/completed', params: { ...this.base, item } });
    }
    return out;
  }

  get itemList() { return [...this.items.values()]; }
}

/** A fresh cutex turn id (opaque; Kimi's own turn number is a separate fact). */
export const newTurnId = () => randomUUID();
