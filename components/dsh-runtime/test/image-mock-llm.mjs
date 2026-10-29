// Test-only offline Cordis plugin: an image-capable model that reports the
// image blocks of the latest user input.
import { createRequire } from 'node:module';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
const require = createRequire(join(process.env.DSH_SPIKE_INSTALL_ROOT, 'package.json'));
const { LlmAdapter } = await import(pathToFileURL(require.resolve('@deepseek-ai/dsh-llm')).href);

class ImageMock extends LlmAdapter {
  providerInfo(provider) { return { id: provider, name: 'Cutex image fixture' }; }
  async listModels(provider) { return [{ provider, id: 'offline-vision', name: 'Offline Vision', inputModalities: ['text', 'image'] }]; }
  async resolveModel(provider, model) { return { provider, id: model, name: model, inputModalities: ['text', 'image'], context: { contextWindow: 65536 } }; }
  async *stream(options) {
    const user = options.messages.filter(m => m.role === 'user' && m.source?.kind === 'user').at(-1);
    const blocks = user?.content ?? [];
    const text = `images ${blocks.filter(b => b.type === 'image').length} kinds ${[...new Set(blocks.map(b => b.type))].join(',')}`;
    yield { type: 'block-start', index: 0, blockType: 'text' };
    yield { type: 'text-delta', index: 0, text };
    yield { type: 'block-end', index: 0, block: { type: 'text', text } };
    yield { type: 'usage', usage: { inputTokens: 30, outputTokens: 4 } };
    yield { type: 'finish', reason: { kind: 'stop' } };
  }
}
export const name = 'cutex-image-offline-llm';
export const inject = ['llm'];
export function apply(ctx) { ctx.llm.registerAdapter(['cutex-offline'], new ImageMock()); }
