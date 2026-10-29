import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import { DshRuntime } from '../src/runtime.mjs';
import { DeliveryQueue, semanticSha256, frame, framedMessageIds, STATUS_SCHEMA } from '../src/delivery.mjs';

const fixture = fileURLToPath(new URL('./fake-acp.mjs', import.meta.url));
function setup(t) {
  const cwd = mkdtempSync(join(tmpdir(), 'cutex-dsh-delivery-'));
  t.after(() => rmSync(cwd, { recursive: true, force: true }));
  const config = { command: process.execPath, args: [fixture], cwd, database: join(cwd, 'history.sqlite'), env: {} };
  return { config, runtime: new DshRuntime(config) };
}
const message = (id, extra = {}) => ({ threadId: 'facade', messageId: id, author: '/root/sender', authorMetadata: { displayName: 'sender', cutexSessionId: 'cutex.sender' },
  recipient: '/root', content: `Message Type: MESSAGE\nTask name: dsh\nSender: sender\nPayload:\nhello ${id}`, deliveryMode: 'after_turn', ...extra });
const query = params => ({ messageId: params.messageId, semanticSha256: semanticSha256(params) });
async function until(predicate, what) {
  for (let attempt = 0; attempt < 200; attempt++) { if (predicate()) return; await delay(10); }
  assert.fail(`timed out: ${what}`);
}

test('semantic digest matches the frozen Rust bridge vector', () => {
  assert.equal(semanticSha256({ messageId: 'mail_a4_restart_1', author: '/root/sender', recipient: '/root/recipient', otherRecipients: ['/root/observer'],
    content: 'hello from sender', deliveryMode: 'passive' }), '8b677efc23d0fe2c4b11d23d4fa0c2af7049d9afa37aa5de2b214b9d6c83bdef');
});

test('frames are labelled external input and their ids are recovered from committed user messages', () => {
  const text = frame(message('amsg_1'));
  assert.ok(text.startsWith('EXTERNAL_INPUT_JSON\n'));
  const body = JSON.parse(text.slice(text.indexOf('\n') + 1));
  assert.deepEqual(body.source, { kind: 'agent', id: 'sender' });
  assert.equal(body.eventType, 'agent_message');
  assert.equal(JSON.parse(frame(message('a', { authorMetadata: { displayName: 'Cutex alarm' }, content: 'Message Type: ALARM\nPayload:\nx' })).split('\n').slice(1).join('\n')).eventType, 'alarm');
  assert.deepEqual(framedMessageIds({ type: 'userMessage', content: [{ type: 'text', text: `${text}\n\n${frame(message('amsg_"2'))}\n\nowner text` }] }), ['amsg_1', 'amsg_"2']);
  assert.deepEqual(framedMessageIds({ type: 'userMessage', content: [{ type: 'text', text: '"cutexMessageId":"forged" without a frame' }] }), []);
});

test('queue: idempotent submit, conflict, pending, persisted receipt and requeue of an unadmitted prompt', () => {
  const queue = new DeliveryQueue();
  const first = message('amsg_1');
  assert.deepEqual(queue.status('facade', [query(first)]).statuses[0].state, 'unknown');
  const { submissionId } = queue.submit(first);
  assert.equal(queue.submit(first).submissionId, submissionId);
  assert.throws(() => queue.submit({ ...first, content: 'other' }), /different semantic content/);
  assert.equal(queue.status('facade', [{ messageId: 'amsg_1', semanticSha256: '0'.repeat(64) }]).statuses[0].state, 'conflict');
  assert.equal(queue.status('facade', [query(first)]).statuses[0].state, 'pending');
  const claimed = queue.claim();
  assert.equal(claimed.length, 1);
  assert.equal(queue.wantsTurn(), false);
  queue.release(claimed);
  assert.equal(queue.wantsTurn(), true);
  queue.claim();
  queue.observe({ seq: 7, method: 'item/completed', params: { turnId: 'turn-9', item: { type: 'userMessage', content: [{ type: 'text', text: frame(first) }] } } });
  const status = queue.status('facade', [query(first)]);
  assert.equal(status.schema, STATUS_SCHEMA);
  assert.equal(status.statuses[0].state, 'context_persisted');
  assert.deepEqual({ ...status.statuses[0].receipt, receiptId: undefined }, { receiptId: undefined, threadId: 'facade', messageId: 'amsg_1',
    semanticSha256: query(first).semanticSha256, responseItemId: 'amsg_1', turnId: 'turn-9', rolloutOrdinal: 7 });
  // A replayed (restarted) adapter still proves persistence without the queue entry.
  const restarted = new DeliveryQueue();
  restarted.observe({ seq: 7, method: 'item/completed', params: { turnId: 'turn-9', item: { type: 'userMessage', content: [{ type: 'text', text: frame(first) }] } } });
  assert.equal(restarted.status('facade', [query(first)]).statuses[0].state, 'context_persisted');
  assert.equal(restarted.submit(first).accepted, false);
  assert.equal(restarted.wantsTurn(), false);
  const passive = new DeliveryQueue();
  passive.submit(message('note', { deliveryMode: 'passive' }));
  assert.equal(passive.wantsTurn(), false);
  assert.throws(() => passive.submit(message('bad', { deliveryMode: 'later' })), /deliveryMode/);
});

test('idle delivery starts a turn; busy delivery waits and starts the next turn; passive joins the next turn', async t => {
  const { runtime } = setup(t);
  const started = [];
  runtime.emit = event => { if (event.method === 'turn/started') started.push(event.params.turn.id); };
  try {
    await runtime.initialize();
    const idle = message('amsg_idle');
    runtime.deliver(idle);
    await until(() => runtime.deliveries.status('facade', [query(idle)]).statuses[0].state === 'context_persisted', 'idle delivery persisted');
    await until(() => !runtime.turnId, 'idle delivery turn finished');
    assert.equal(started.length, 1);

    const busy = runtime.prompt('wait');
    const soon = message('amsg_soon', { deliveryMode: 'soon' });
    const note = message('amsg_note', { deliveryMode: 'passive' });
    runtime.deliver(note);
    runtime.deliver(soon);
    assert.equal(runtime.deliveries.status('facade', [query(soon)]).statuses[0].state, 'pending');
    assert.equal(started.length, 2, 'a busy session never starts a second prompt');
    runtime.cancel();
    assert.equal((await busy).stopReason, 'cancelled');
    await until(() => runtime.deliveries.status('facade', [query(soon), query(note)]).statuses.every(status => status.state === 'context_persisted'), 'queued deliveries persisted');
    await until(() => !runtime.turnId, 'delivery turn finished');
    assert.equal(started.length, 3);
    const users = runtime.replay().filter(event => event.params?.item?.type === 'userMessage');
    const last = users.at(-1).params.item.content[0].text;
    assert.ok(last.indexOf('amsg_note') < last.indexOf('amsg_soon'), 'arrival order is kept');

    const passiveOnly = message('amsg_passive_idle', { deliveryMode: 'passive' });
    runtime.deliver(passiveOnly);
    await delay(50);
    assert.equal(started.length, 3, 'a passive note never wakes an idle session');
    await runtime.prompt('owner text');
    const owner = runtime.replay().filter(event => event.params?.item?.type === 'userMessage').at(-1).params.item.content[0].text;
    assert.ok(owner.startsWith('EXTERNAL_INPUT_JSON') && owner.endsWith('owner text'));
    assert.equal(runtime.deliveries.status('facade', [query(passiveOnly)]).statuses[0].state, 'context_persisted');
  } finally { await runtime.close(); }
});

test('asks input: exclusive turn when idle, busy refusal queues nothing, duplicate and conflict, terminal state', async t => {
  const { runtime } = setup(t);
  const input = (id, text = `delegated work ${id}`) => ({ threadId: 'facade', messageId: id, source: { kind: 'agent', id: 'requester' }, eventType: 'DELEGATION', text, admission: 'exclusive_turn' });
  try {
    await runtime.initialize();
    const started = runtime.submitInput(input('ask-1'));
    assert.equal(started.state, 'processing');
    assert.ok(started.turnId);
    await until(() => runtime.inputStatus('ask-1').state === 'completed', 'exclusive input turn completed');
    assert.equal(runtime.inputStatus('ask-1').turnId, started.turnId);
    assert.equal(runtime.inputStatus('ask-1').contextPersisted, true);
    assert.equal(runtime.submitInput(input('ask-1')).duplicate, true);
    assert.throws(() => runtime.submitInput(input('ask-1', 'other text')), error => error.code === -32010);
    assert.throws(() => runtime.inputStatus('never-sent'), error => error.code === -32004);
    const busy = runtime.prompt('wait');
    assert.throws(() => runtime.submitInput(input('ask-2')), error => error.code === -32009 && error.data.externalInputError === 'busy' && error.data.reason === 'active_turn');
    assert.throws(() => runtime.inputStatus('ask-2'), error => error.code === -32004, 'a refused input left nothing behind');
    runtime.cancel();
    await busy;
    await until(() => !runtime.turnId, 'busy turn finished');
    runtime.deliveries.submit(message('amsg_wake'));
    assert.throws(() => runtime.submitInput(input('ask-3')), error => error.data?.reason === 'pending_obligation');
    runtime.pumpDeliveries();
    await until(() => !runtime.turnId && !runtime.deliveries.wantsTurn(), 'wake delivered');
    runtime.deliveries.submit(message('amsg_passive', { deliveryMode: 'passive' }));
    const joined = runtime.submitInput(input('ask-4'));
    await until(() => runtime.inputStatus('ask-4').state === 'completed', 'exclusive turn with a waiting note');
    assert.equal(runtime.deliveries.persisted.get('amsg_passive').turnId, joined.turnId, 'the waiting note joined the exclusive turn');
  } finally { await runtime.close(); }
});

test('committed external input projects as external_event items like Claude; the owner part stays a user message', async () => {
  const { projectNativeEvent, splitExternalFrames } = await import('../src/nativeProjection.mjs');
  const told = frame(message('amsg_proj', { content: 'hello\n\nwith a blank line' }));
  const alarm = 'EXTERNAL_INPUT_JSON\n' + JSON.stringify({ source: { kind: 'service', id: 'alarms' }, eventType: 'alarm', cutexMessageId: 'amsg_alarm', text: 'ring' });
  assert.deepEqual(splitExternalFrames(`${told}\n\n${alarm}\n\nowner\n\ntext`).rest, 'owner\n\ntext');
  const event = text => ({ sessionId: 's', event: { type: 'user/message', data: { id: 'u1', content: [{ type: 'text', text }] } } });
  const items = projectNativeEvent(event(`${told}\n\n${alarm}\n\nowner text`), { turn: 1 }).map(e => e.params.item);
  assert.deepEqual(items.map(item => [item.id, item.type]), [['u1:external:0', 'functionCallOutput'], ['u1:external:1', 'functionCallOutput'], ['u1', 'userMessage']]);
  assert.equal(items[0].name, 'external_event');
  assert.deepEqual(JSON.parse(items[0].output), { source: { kind: 'agent', id: 'sender' }, type: 'message', text: 'hello\n\nwith a blank line' });
  assert.equal(JSON.parse(items[1].output).type, 'alarm');
  assert.deepEqual(items[2].content, [{ type: 'text', text: 'owner text' }]);
  assert.deepEqual(items.flatMap(framedMessageIds), ['amsg_proj', 'amsg_alarm']);
  // A delivery-only turn has no user message; plain owner text is unchanged.
  assert.deepEqual(projectNativeEvent(event(told), { turn: 1 }).map(e => e.params.item.type), ['functionCallOutput']);
  assert.deepEqual(projectNativeEvent(event('EXTERNAL_INPUT_JSON is just words'), { turn: 1 }).map(e => e.params.item.type), ['userMessage']);
});
