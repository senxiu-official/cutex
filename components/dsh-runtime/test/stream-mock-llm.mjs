// Test-only offline Cordis plugin: streams reasoning and text in several
// deltas with pauses, so live frames precede the committed message.
import { createRequire } from 'node:module';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
const require = createRequire(join(process.env.DSH_SPIKE_INSTALL_ROOT, 'package.json'));
const { LlmAdapter } = await import(pathToFileURL(require.resolve('@deepseek-ai/dsh-llm')).href);
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));

class StreamMock extends LlmAdapter {
  providerInfo(provider) { return { id: provider, name: 'Cutex stream fixture' }; }
  async listModels(provider) { return [{ provider, id: 'offline', name: 'Offline', inputModalities: ['text'] }]; }
  async resolveModel(provider, model) { return { provider, id: model, name: model, inputModalities: ['text'], context: { contextWindow: 65536 } }; }
  async *stream() {
    yield { type: 'block-start', index: 0, blockType: 'reasoning' };
    for (const text of ['think', 'ing']) { await pause(80); yield { type: 'reasoning-delta', index: 0, text }; }
    yield { type: 'block-end', index: 0, block: { type: 'reasoning', text: 'thinking' } };
    yield { type: 'block-start', index: 1, blockType: 'text' };
    for (const text of ['Hel', 'lo ', 'live']) { await pause(80); yield { type: 'text-delta', index: 1, text }; }
    yield { type: 'block-end', index: 1, block: { type: 'text', text: 'Hello live' } };
    yield { type: 'usage', usage: { inputTokens: 21, outputTokens: 5 } };
    yield { type: 'finish', reason: { kind: 'stop' } };
  }
}
export const name = 'cutex-stream-offline-llm';
export const inject = ['llm'];
export function apply(ctx) { ctx.llm.registerAdapter(['cutex-offline'], new StreamMock()); }
