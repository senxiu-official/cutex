// Dual-purpose TEST fixture: a real DSH Cordis plugin and an IPC controller.
// The plugin is mounted by --patch; no production injection points are used.
import { appendFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { DshRuntime } from '../src/runtime.mjs';

const require = createRequire(join(process.env.DSH_SPIKE_INSTALL_ROOT, 'package.json'));
const { LlmAdapter } = await import(pathToFileURL(require.resolve('@deepseek-ai/dsh-llm')).href);
export const name = 'cutex-native-crash-offline';
export const inject = ['llm'];
export const answer = 'NATIVE_DURABLE_ASSISTANT_CRASH_WINDOW';
class OfflineAdapter extends LlmAdapter {
  providerInfo(provider) { return { id: provider, name: 'Offline native crash fixture' }; }
  async listModels(provider) { return [{ provider, id: 'offline', name: 'Offline', inputModalities: ['text'] }]; }
  async resolveModel(provider, model) {
    return { provider, id: model, name: model, inputModalities: ['text'], context: { contextWindow: 2048 } };
  }
  async *stream(options) {
    appendFileSync(process.env.DSH_NATIVE_CRASH_AUDIT, JSON.stringify({ event: 'provider-request', pid: process.pid,
      userMessages: options.messages.filter(message => message.role === 'user' && message.source?.kind === 'user').map(message => message.id),
    }) + '\n');
    const block = { type: 'text', text: answer };
    yield { type: 'block-start', index: 0, blockType: 'text' };
    yield { type: 'text-delta', index: 0, text: answer };
    yield { type: 'block-end', index: 0, block };
    yield { type: 'usage', usage: { inputTokens: 10, outputTokens: 5 } };
    yield { type: 'finish', reason: { kind: 'stop' } };
  }
}
export function apply(ctx) { ctx.llm.registerAdapter(['native-crash-offline'], new OfflineAdapter()); }

async function controller() {
  const [mode, home, install] = process.argv.slice(2);
  const emitted = [];
  const methods = [];
  const runtime = new DshRuntime({
    command: process.execPath,
    args: [join(install, 'node_modules/@deepseek-ai/dsh/lib/bin.js'), '--profile', 'acp', '--patch', join(home, 'offline.patch.yml')],
    cwd: join(home, 'workspace'), env: { ...process.env }, database: join(home, 'timeline.sqlite'),
    sessionsRoot: join(home, 'sessions'), installRoot: install,
    emit: event => emitted.push(event),
  });
  process.send({ type: 'spawned', dshPid: runtime.rpc.child.pid });
  const request = runtime.rpc.request.bind(runtime.rpc);
  runtime.rpc.request = async (method, ...args) => {
    methods.push(method);
    const result = await request(method, ...args);
    if (method === 'session/prompt') process.send({ type: 'prompt-settled', stopReason: result.stopReason });
    return result;
  };
  if (mode === 'crash') {
    const commit = runtime.store.commitNative.bind(runtime.store);
    let stopped = false;
    runtime.store.commitNative = record => {
      if (record.event.type === 'assistant/message') {
        if (!stopped) {
          stopped = true;
          process.send({ type: 'checkpoint', sessionId: runtime.sessionId, dshPid: runtime.rpc.child.pid,
            nativeSeq: record.nativeSeq, messageId: record.event.data.message.id,
            cursorBefore: runtime.store.nativeCursor(runtime.sessionId), timelineBefore: runtime.replay(),
          });
        }
        // Only controller projection is suspended. DSH's separate process and
        // durable writer continue; no rejection/rollback or later commit occurs.
        return new Promise(() => {});
      }
      return commit(record);
    };
  }
  try {
    await runtime.initialize();
    if (mode === 'crash') {
      await runtime.prompt('UNIQUE_NATIVE_CRASH_PROMPT_DO_NOT_RESEND');
      throw new Error('Projection barrier unexpectedly returned');
    }
    if (mode !== 'resume') throw new Error('Unknown native crash worker mode');
    const first = runtime.replay();
    await runtime.syncNative();
    await runtime.syncNative();
    process.send({ type: 'recovered', sessionId: runtime.sessionId, first, second: runtime.replay(), emitted,
      methods, cursor: runtime.store.nativeCursor(runtime.sessionId) });
    await runtime.close();
    process.send({ type: 'closed' }, () => process.disconnect());
  } catch (error) {
    process.send({ type: 'error', message: error.stack, stderr: runtime.rpc.stderr }, () => process.exit(1));
  }
}

// DSH imports this file as a plugin, with its own --profile argv. Only the
// parent's explicitly forked IPC child may construct a controller here.
if (process.send && ['crash', 'resume'].includes(process.argv[2])) await controller();
