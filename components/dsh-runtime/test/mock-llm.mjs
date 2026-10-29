// Test-only Cordis plugin. Runs inside the real DSH ACP profile, without a provider key.
import { createRequire } from 'node:module';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
const require = createRequire(join(process.env.DSH_SPIKE_INSTALL_ROOT, 'package.json'));
const { LlmAdapter } = await import(pathToFileURL(require.resolve('@deepseek-ai/dsh-llm')).href);

class MockAdapter extends LlmAdapter {
  providerInfo(provider) { return { id: provider, name: 'Cutex offline fixture' }; }
  async listModels(provider) { return [{ provider, id: 'offline', name: 'Offline', inputModalities: ['text'] }]; }
  async resolveModel(provider, model) { return { provider, id: model, name: model, inputModalities: ['text'], context: { contextWindow: 2048 } }; }
  async *stream(options) {
    const index = options.messages.findLastIndex(m => m.role === 'user' && m.source?.kind === 'user');
    const messages = options.messages.slice(index);
    const text = messages.filter(m => m.role === 'user').flatMap(m => m.content).filter(b => b.type === 'text').map(b => b.text).join('');
    const result = messages.find(m => m.role === 'tool');
    if (text.includes('wait') && !result) {
      await new Promise((resolve, reject) => {
        if (options.signal?.aborted) return reject(new Error('cancelled'));
        options.signal?.addEventListener('abort', () => reject(new Error('cancelled')), { once: true });
      });
      return;
    }
    const block = result
      ? { type: 'text', text: `offline tool result: ${result.content.filter(b => b.type === 'text').map(b => b.text).join('')}` }
      : { type: 'tool-call', id: `spike-${options.messages.length}`, name: 'mcp__cutex__agent_list', arguments: '{}' };
    yield { type: 'block-start', index: 0, blockType: block.type };
    if (block.type === 'text') yield { type: 'text-delta', index: 0, text: block.text };
    else yield { type: 'tool-call-delta', index: 0, id: block.id, name: block.name, argumentsDelta: block.arguments };
    yield { type: 'block-end', index: 0, block };
    yield { type: 'usage', usage: { inputTokens: 10, outputTokens: 5 } };
    yield { type: 'finish', reason: { kind: result ? 'stop' : 'tool-calls' } };
  }
}
export const name = 'cutex-spike-offline-llm';
export const inject = ['llm', 'tools', 'approval'];
export function apply(ctx) {
  ctx.llm.registerAdapter(['cutex-offline'], new MockAdapter());
  // Exercise DSH's real approval service and ACP request path on a harmless MCP read.
  ctx.on('tools/execute', async (exec, next) => {
    if (exec.name !== 'mcp__cutex__agent_list') return next();
    const outcome = await ctx.approval.request({ agent: exec.agent, toolName: exec.name, callId: exec.callId, reason: 'Offline spike requests the mocked agent list.' });
    if (outcome !== 'allowed-once') return { isError: true, content: [{ type: 'text', text: 'offline approval denied' }] };
    return next();
  });
}
