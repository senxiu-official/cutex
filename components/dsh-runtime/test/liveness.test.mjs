import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import { DshRuntime } from '../src/runtime.mjs';

// The prompt has no wall clock: only a liveness watchdog. A dead or wedged
// DSH is restarted and the agent takes new input again.
const fixture = fileURLToPath(new URL('./fake-acp.mjs', import.meta.url));
const liveness = { interval: 60, probeTimeout: 60, misses: 2, restartDelays: [10, 10, 10] };
function setup(t) {
  const cwd = mkdtempSync(join(tmpdir(), 'cutex-dsh-liveness-'));
  t.after(() => rmSync(cwd, { recursive: true, force: true }));
  const events = [];
  const runtime = new DshRuntime({ command: process.execPath, args: [fixture], cwd, database: join(cwd, 'history.sqlite'), env: {}, liveness,
    emit: event => events.push(event) });
  return { runtime, events };
}
async function until(predicate, what) {
  for (let attempt = 0; attempt < 300; attempt++) { if (predicate()) return; await delay(10); }
  assert.fail(`timed out: ${what}`);
}
const terminals = events => events.filter(event => event.method === 'turn/completed');

test('a silent but answering DSH is never cut off by a clock', async t => {
  const { runtime, events } = setup(t);
  try {
    await runtime.initialize();
    // 400 ms of ACP silence is several watchdog intervals; probes answer.
    const result = await runtime.prompt('slow');
    assert.equal(result.stopReason, 'end_turn');
    assert.equal(terminals(events).at(-1).params.turn.status, 'completed');
    assert.equal(runtime.available, true);
  } finally { await runtime.close(); }
});

test('a wedged DSH is killed by the watchdog, the turn ends failed, and a restarted DSH takes the next prompt', async t => {
  const { runtime, events } = setup(t);
  try {
    await runtime.initialize();
    const first = runtime.rpc;
    // The outcome, not the transport's wording (ACP exited / EPIPE / ... differ by platform and load).
    await assert.rejects(runtime.prompt('hang'));
    const failed = terminals(events).at(-1).params.turn;
    assert.equal(failed.status, 'failed');
    assert.match(failed.error.message, /stopped responding/);
    await until(() => runtime.available, 'DSH restarted');
    assert.notEqual(runtime.rpc, first);
    assert.equal((await runtime.prompt('hello')).stopReason, 'end_turn');
  } finally { await runtime.close(); }
});

test('a DSH that exits mid-turn or while idle is restarted and takes new prompts', async t => {
  const { runtime, events } = setup(t);
  try {
    await runtime.initialize();
    await assert.rejects(runtime.prompt('die'));
    assert.equal(terminals(events).at(-1).params.turn.status, 'failed', 'cutex never waits on the dead turn');
    await until(() => runtime.available, 'restart after a mid-turn exit');
    // Idle exit: nobody is prompting, the runtime still restarts DSH.
    const idle = runtime.rpc;
    idle.kill();
    await until(() => runtime.rpc !== idle && runtime.available, 'restart after an idle exit');
    assert.equal((await runtime.prompt('hello')).stopReason, 'end_turn');
  } finally { await runtime.close(); }
});
