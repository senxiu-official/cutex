// Test-only offline Cordis plugin: scripted DSH tool calls for the tool-card acceptance.
// A user text `SCRIPT <json>` (one line) names the calls to make in order, each
// `[toolName, argumentsObject]`; after the last result the model replies with a
// short text. CUTEX_TOOLS_DUMP (optional) receives the tool names DSH offered.
import { createRequire } from 'node:module';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
const require = createRequire(join(process.env.DSH_SPIKE_INSTALL_ROOT, 'package.json'));
const { LlmAdapter } = await import(pathToFileURL(require.resolve('@deepseek-ai/dsh-llm')).href);

function* reply(text) {
  yield { type: 'block-start', index: 0, blockType: 'text' };
  yield { type: 'text-delta', index: 0, text };
  yield { type: 'block-end', index: 0, block: { type: 'text', text } };
  yield { type: 'usage', usage: { inputTokens: 12, outputTokens: 3 } };
  yield { type: 'finish', reason: { kind: 'stop' } };
}

class ToolcardsMock extends LlmAdapter {
  providerInfo(provider) { return { id: provider, name: 'Cutex tool-card fixture' }; }
  async listModels(provider) { return [{ provider, id: 'offline', name: 'offline', inputModalities: ['text'] }]; }
  async resolveModel(provider, model) { return { provider, id: model, name: model, inputModalities: ['text'], context: { contextWindow: 65536 } }; }
  async *stream(options) {
    if (process.env.CUTEX_TOOLS_DUMP) {
      try { writeFileSync(process.env.CUTEX_TOOLS_DUMP, JSON.stringify((options.tools ?? []).map(tool => tool.name ?? tool.function?.name ?? null))); } catch { /* diagnostics only */ }
    }
    const index = options.messages.findLastIndex(m => m.role === 'user' && m.source?.kind === 'user');
    const messages = options.messages.slice(index);
    const text = messages.filter(m => m.role === 'user').flatMap(m => m.content).filter(b => b.type === 'text').map(b => b.text).join('\n');
    const done = messages.filter(m => m.role === 'tool').length;
    const script = /SCRIPT (\[.*\])/.exec(text);
    const calls = script ? JSON.parse(script[1]) : [];
    if (done < calls.length) {
      const [name, args] = calls[done];
      const block = { type: 'tool-call', id: `call-${options.messages.length}`, name, arguments: JSON.stringify(args) };
      yield { type: 'block-start', index: 0, blockType: 'tool-call' };
      yield { type: 'tool-call-delta', index: 0, id: block.id, name: block.name, argumentsDelta: block.arguments };
      yield { type: 'block-end', index: 0, block };
      yield { type: 'usage', usage: { inputTokens: 12, outputTokens: 3 } };
      yield { type: 'finish', reason: { kind: 'tool-calls' } };
      return;
    }
    yield* reply(`script done after ${done} tool calls`);
  }
}
export const name = 'cutex-toolcards-offline-llm';
export const inject = ['llm'];
export function apply(ctx) { ctx.llm.registerAdapter(['cutex-offline'], new ToolcardsMock()); }
