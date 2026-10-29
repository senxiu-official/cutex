// Test-only offline Cordis plugin for the private-daemon delivery acceptance.
// `CALL <tool> <json>` calls the cutex MCP tool; inter-agent input is answered
// with a short receipt line; everything else gets a fixed reply.
import { createRequire } from 'node:module';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
const require = createRequire(join(process.env.DSH_SPIKE_INSTALL_ROOT, 'package.json'));
const { LlmAdapter } = await import(pathToFileURL(require.resolve('@deepseek-ai/dsh-llm')).href);

function* reply(text) {
  const block = { type: 'text', text };
  yield { type: 'block-start', index: 0, blockType: 'text' };
  yield { type: 'text-delta', index: 0, text };
  yield { type: 'block-end', index: 0, block };
  yield { type: 'usage', usage: { inputTokens: 12, outputTokens: 3 } };
  yield { type: 'finish', reason: { kind: 'stop' } };
}

class DeliveryMock extends LlmAdapter {
  providerInfo(provider) { return { id: provider, name: 'Cutex delivery fixture' }; }
  // CUTEX_OFFLINE_MODELS (comma list) stands in for another catalog, e.g. DSH's own model ids.
  async listModels(provider) { return (process.env.CUTEX_OFFLINE_MODELS?.split(',') ?? ['offline', 'offline-pro']).map(id => ({ provider, id, name: id, inputModalities: ['text'] })); }
  async resolveModel(provider, model) {
    return { provider, id: model, name: model, inputModalities: ['text'], context: { contextWindow: 65536 },
      reasoning: { efforts: [{ id: 'off', name: 'Off' }, { id: 'low', name: 'Low' }, { id: 'high', name: 'High' }, { id: 'max', name: 'Max' }], defaultEffort: 'high' } };
  }
  async *stream(options) {
    const index = options.messages.findLastIndex(m => m.role === 'user' && m.source?.kind === 'user');
    const messages = options.messages.slice(index);
    const text = messages.filter(m => m.role === 'user').flatMap(m => m.content).filter(b => b.type === 'text').map(b => b.text).join('\n');
    const result = messages.filter(m => m.role === 'tool').at(-1);
    if (result) { yield* reply(`tool done: ${result.content.filter(b => b.type === 'text').map(b => b.text).join('').slice(0, 300)}`); return; }
    // Instructions acceptance: how often each marker reaches the model.
    if (text.includes('SHOW-PROMPT') && !text.includes('EXTERNAL_INPUT_JSON')) {
      const all = options.messages.flatMap(m => Array.isArray(m.content) ? m.content : [{ type: 'text', text: String(m.content ?? '') }]).filter(b => b.type === 'text').map(b => b.text).join('\n');
      const count = marker => all.split(marker).length - 1;
      yield* reply(`prompt ${JSON.stringify({ cutex: all.split(/^# Cutex$/m).length - 1, user: count('USER-AGENTS-MARKER'), project: count('PROJECT-AGENTS-MARKER'), fileReferences: count('# File references') })}`);
      return;
    }
    // Model acceptance: which route served this request.
    if (text.includes('SHOW-MODEL') && !text.includes('EXTERNAL_INPUT_JSON')) { yield* reply(`model ${options.model} effort ${options.reasoningEffort ?? 'default'}`); return; }
    // r266 stop/pause acceptance: `HOLD-TURN` (owner text, not a CALL, or an ASK frame)
    // streams nothing until ACP cancel aborts the request (cap 180 s).
    const asks = text.split('EXTERNAL_INPUT_JSON\n').slice(1).map(part => { try { return JSON.parse(part.split('\n')[0]); } catch { return null; } }).filter(Boolean);
    if ((!text.includes('EXTERNAL_INPUT_JSON') && !/CALL \w+ \{/.test(text) && text.includes('HOLD-TURN')) || asks.some(frame => frame.eventType === 'ASK' && frame.text?.includes('HOLD-TURN'))) {
      await new Promise((resolve, reject) => {
        if (options.signal?.aborted) return reject(new Error('cancelled'));
        const timer = setTimeout(resolve, 180000);
        options.signal?.addEventListener('abort', () => { clearTimeout(timer); reject(new Error('cancelled')); }, { once: true });
      });
      yield* reply('held turn ran out');
      return;
    }
    // r267 B2: `SHELL-HOLD <marker>` (owner text or an ASK frame) runs DSH's bash tool with a
    // long command whose command line carries the marker, until the turn is stopped.
    const shell = /SHELL-HOLD ([\w-]+)/.exec(text);
    if (shell && !/CALL \w+ \{/.test(text)) {
      const block = { type: 'tool-call', id: `call-${options.messages.length}`, name: 'bash', arguments: JSON.stringify({ command: `sleep 600 && echo ${shell[1]}`, description: 'Hold the turn' }) };
      yield { type: 'block-start', index: 0, blockType: 'tool-call' };
      yield { type: 'tool-call-delta', index: 0, id: block.id, name: block.name, argumentsDelta: block.arguments };
      yield { type: 'block-end', index: 0, block };
      yield { type: 'finish', reason: { kind: 'tool-calls' } };
      return;
    }
    // Liveness acceptance: DSH dies in the middle of a turn.
    if (text.includes('EXIT-DSH-NOW') && !text.includes('EXTERNAL_INPUT_JSON')) process.exit(9);
    // `ESCALATE <path>`: a write outside the workspace asking for full access.
    const escalate = /ESCALATE (\S+)/.exec(text);
    if (escalate && !text.includes('EXTERNAL_INPUT_JSON')) {
      const block = { type: 'tool-call', id: `call-${options.messages.length}`, name: 'write', arguments: JSON.stringify({ file_path: escalate[1], content: 'escalated\n',
        sandbox_permissions: 'danger-full-access', justification: 'Cutex acceptance: write one file outside the workspace.' }) };
      yield { type: 'block-start', index: 0, blockType: 'tool-call' };
      yield { type: 'tool-call-delta', index: 0, id: block.id, name: block.name, argumentsDelta: block.arguments };
      yield { type: 'block-end', index: 0, block };
      yield { type: 'finish', reason: { kind: 'tool-calls' } };
      return;
    }
    const call = /CALL (\w+) (\{.*\})/.exec(text);
    if (call && !text.includes('EXTERNAL_INPUT_JSON')) {
      const block = { type: 'tool-call', id: `call-${options.messages.length}`, name: `mcp__cutex__${call[1]}`, arguments: call[2] };
      yield { type: 'block-start', index: 0, blockType: 'tool-call' };
      yield { type: 'tool-call-delta', index: 0, id: block.id, name: block.name, argumentsDelta: block.arguments };
      yield { type: 'block-end', index: 0, block };
      yield { type: 'finish', reason: { kind: 'tool-calls' } };
      return;
    }
    if (text.includes('EXTERNAL_INPUT_JSON')) {
      const frames = text.split('EXTERNAL_INPUT_JSON\n').slice(1).map(part => { try { return JSON.parse(part.split('\n')[0]); } catch { return null; } }).filter(Boolean);
      yield* reply(`received ${frames.map(f => `${f.eventType}:${f.text.replace(/\s+/g, ' ').slice(0, 160)}`).join(' | ')}`);
      return;
    }
    yield* reply(`offline reply to: ${text.slice(0, 80)}`);
  }
}
export const name = 'cutex-delivery-offline-llm';
export const inject = ['llm'];
export function apply(ctx) { ctx.llm.registerAdapter(['cutex-offline'], new DeliveryMock()); }
