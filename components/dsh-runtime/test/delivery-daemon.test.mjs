import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { install, binary, privateDaemon, poll, userTexts, agentTexts, uuid, offlinePatch } from './private-daemon.mjs';

// Private debug daemon + real DSH + offline model: agent_tell, agent_delegate
// (task and result), alarm_set and a DSH crash, into DSH agents. Opt-in only.
// DSH_DELIVERY_ADAPTER may name another adapter entry (reproducing an older one).
const plugin = fileURLToPath(new URL('./delivery-mock-llm.mjs', import.meta.url));

test('private daemon delivers agent_tell, delegation task + result and alarms into DSH agents', { skip: !install || !binary, timeout: 600000 }, async t => {
  const daemon = await privateDaemon(t, { label: 'delivery', patch: offlinePatch(plugin), adapter: process.env.DSH_DELIVERY_ADAPTER,
    env: { DSH_DELIVERY_TEST_KEY: `dummy-delivery-key-${uuid()}` } });
  const { api, history, send, idle } = daemon;
  const { profile } = await api('/v2/profiles', { name: 'DSH Delivery Offline', kind: 'dsh', options: { dsh: { model: 'offline', apiKeyEnv: 'DSH_DELIVERY_TEST_KEY' } } });
  const a = await daemon.createAgent('dsh-a', profile.id);
  const b = await daemon.createAgent('dsh-b', profile.id);
  await daemon.online(a); await daemon.online(b);

  // 1. agent_tell with wake from A to B starts a B turn with the framed note.
  await send(a, `CALL agent_tell {"to":"${b.id}","text":"hello-from-a","wake":true}`);
  const told = await poll(() => history(b), turns => userTexts(turns).some(text => text.includes('EXTERNAL_INPUT_JSON') && text.includes('hello-from-a'))
    && turns.at(-1)?.status === 'completed', 'agent_tell delivered into B and answered');
  assert.ok(agentTexts(told).some(text => text.startsWith('received agent_message:')));
  assert.ok(agentTexts(await history(a)).some(text => text.startsWith('tool done:')), 'A saw its tool result');

  // 2. agent_delegate from A to B: B handles the task; the result returns to A.
  await idle(a); await idle(b);
  await send(a, `CALL agent_delegate {"target":"${b.id}","text":"compute the delegated answer","title":"delegated"}`);
  await poll(() => history(b), turns => userTexts(turns).some(text => text.includes('compute the delegated answer')) && turns.at(-1)?.status === 'completed', 'delegation task delivered into B');
  await poll(() => history(a), turns => userTexts(turns).some(text => text.includes('EXTERNAL_INPUT_JSON') && /delegation result/i.test(text)), 'delegation result delivered back into A', 120000);

  // 3. alarm_set on A for B: the alarm arrives as a framed ALARM.
  await idle(a);
  await send(a, `CALL alarm_set {"alarm_id":"alarm-${uuid().slice(0, 8)}","to":"${b.id}","message":"alarm-for-b","delay_seconds":1}`);
  await poll(() => history(b), turns => userTexts(turns).some(text => text.includes('EXTERNAL_INPUT_JSON') && text.includes('alarm-for-b')) && turns.at(-1)?.status === 'completed', 'alarm delivered into B', 120000);

  // 4. DSH dies mid-turn: the turn ends (cutex is not left waiting), the
  // adapter restarts DSH, DSH closes the dangling turn natively, and the
  // agent takes the next message.
  await idle(a); await idle(b);
  const before = (await history(b)).length;
  await send(b, 'EXIT-DSH-NOW');
  await poll(() => api(b.path), value => value.runtime?.appServerConnected && !value.runtime?.activeTurnId && (value.status?.type ?? 'idle') !== 'active', 'B idle after DSH died', 60000);
  await send(a, `CALL agent_tell {"to":"${b.id}","text":"after-restart","wake":true}`);
  const restarted = await poll(() => history(b), turns => userTexts(turns).some(text => text.includes('after-restart')) && turns.at(-1)?.status === 'completed', 'B answers after its DSH restarted', 120000);
  const died = restarted.slice(before).find(turn => userTexts([turn]).some(text => text.includes('EXIT-DSH-NOW')));
  assert.ok(died && died.status !== 'inProgress' && died.status !== 'completed', `the dead turn is closed natively: ${JSON.stringify(died)?.slice(0, 400)}`);
  daemon.markPassed();
});
