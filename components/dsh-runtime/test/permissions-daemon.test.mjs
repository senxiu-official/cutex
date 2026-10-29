import { test } from 'node:test';
import assert from 'node:assert/strict';
import { existsSync, mkdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { install, binary, privateDaemon, poll, agentTexts, uuid, offlinePatch, delay } from './private-daemon.mjs';

// Private daemon + real DSH + offline model: the agent's cutex permission
// settings drive DSH's knobs (fresh and resumed sessions); an escalation
// reaches Management as a pending server request and waits for the owner
// (no clock); cutex MCP tools never ask. Opt-in only.
const plugin = fileURLToPath(new URL('./delivery-mock-llm.mjs', import.meta.url));

test('DSH approvals follow the agent settings, wait for the owner and pre-approve cutex tools', { skip: !install || !binary, timeout: 600000 }, async t => {
  const daemon = await privateDaemon(t, { label: 'approvals', patch: offlinePatch(plugin), env: { DSH_APPROVAL_TEST_KEY: `dummy-approval-key-${uuid()}` } });
  const { api, history, send, idle, state } = daemon;
  const { profile } = await api('/v2/profiles', { name: 'DSH Approval Offline', kind: 'dsh', options: { dsh: { model: 'offline', apiKeyEnv: 'DSH_APPROVAL_TEST_KEY' } } });
  const agent = await daemon.createAgent('dsh-approver', profile.id, { sandboxMode: 'workspace-write', approvalPolicy: 'on-request' });
  await daemon.online(agent);
  const outside = join(daemon.home, 'outside');
  mkdirSync(outside);

  // 1. A cutex MCP tool runs without asking.
  await send(agent, 'CALL agent_list {}');
  await poll(() => history(agent), turns => turns.at(-1)?.status === 'completed' && agentTexts(turns).some(text => text.startsWith('tool done:')), 'agent_list ran');
  assert.equal((await state(agent)).runtime.pendingServerRequests.length, 0);

  // 2. An escalation outside the workspace asks the owner and waits past the old 30 s cancel.
  await idle(agent);
  const first = join(outside, 'first.txt');
  await send(agent, `ESCALATE ${first}`);
  const pending = await poll(() => state(agent), value => value.runtime?.pendingServerRequests?.length === 1, 'approval pending in Management');
  const request = pending.runtime.pendingServerRequests[0];
  assert.equal(request.method, 'item/commandExecution/requestApproval');
  await delay(35000);
  const still = await state(agent);
  assert.equal(still.runtime.pendingServerRequests.length, 1, 'no auto-cancel');
  assert.ok(still.runtime.activeTurnId, 'the turn is still waiting');
  await daemon.answer(agent, request, 'accept');
  await poll(() => history(agent), turns => turns.at(-1)?.status === 'completed', 'approved turn completed');
  assert.equal(readFileSync(first, 'utf8'), 'escalated\n');

  // 3. A decline keeps the file unwritten.
  await idle(agent);
  const declined = join(outside, 'declined.txt');
  await send(agent, `ESCALATE ${declined}`);
  const second = await poll(() => state(agent), value => value.runtime?.pendingServerRequests?.length === 1, 'second approval pending');
  await daemon.answer(agent, second.runtime.pendingServerRequests[0], 'decline');
  await poll(() => history(agent), turns => turns.at(-1)?.status === 'completed', 'declined turn completed');
  assert.equal(existsSync(declined), false);

  // 4. New settings reach the resumed session at the next start: full access writes without asking.
  await daemon.stop(agent);
  await daemon.request(agent, 'cutex/session/defaults/update', { patch: { sandboxMode: 'danger-full-access', approvalPolicy: 'never' } });
  await daemon.online(agent);
  const full = join(outside, 'full.txt');
  await send(agent, `ESCALATE ${full}`);
  await poll(() => history(agent), turns => turns.at(-1)?.status === 'completed' && agentTexts([turns.at(-1)]).length > 0, 'full-access turn completed', 120000);
  assert.equal(readFileSync(full, 'utf8'), 'escalated\n', 'the resumed session runs the new knobs');
  daemon.markPassed();
});
