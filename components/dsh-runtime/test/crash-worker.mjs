import { DshRuntime } from '../src/runtime.mjs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
const cwd = process.argv[2];
const runtime = new DshRuntime({
  command: process.execPath,
  args: [fileURLToPath(new URL('./fake-acp.mjs', import.meta.url))],
  cwd, database: join(cwd, 'history.sqlite'), env: {},
  emit(event) {
    if (event.method === 'item/reasoning/textDelta') process.send({ pid: runtime.rpc.child.pid, sessionId: runtime.sessionId });
  },
});
await runtime.initialize();
await runtime.prompt('wait');
