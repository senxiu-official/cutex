import { test } from 'node:test';
import assert from 'node:assert/strict';
import { fileURLToPath } from 'node:url';
import { install, binary, privateDaemon, poll, agentTexts, uuid, offlinePatch } from './private-daemon.mjs';

// r265: a Director's agent_create makes a DSH agent through a Profile's DSH
// variant (offline model, fake key), the agent comes online on the Bus, gets
// its task and the result returns to the Director as an external event.
const plugin = fileURLToPath(new URL('./delivery-mock-llm.mjs', import.meta.url));

test('agent_create makes a DSH agent through the variant; task delivered, result back', { skip: !install || !binary, timeout: 600000 }, async t => {
  const daemon = await privateDaemon(t, { label: 'create', patch: offlinePatch(plugin, 'deepseek-flash'),
    env: { DSH_SPIKE_INSTALL_ROOT: install, CUTEX_OFFLINE_MODELS: 'deepseek-flash,deepseek-v4-pro' } });
  const { api, history, send, request } = daemon;
  const { profile } = await api('/v2/profiles', { name: 'DeepSeek Offline', kind: 'apiKey', preset: 'deepseek', apiKey: 'sk-fake-create-offline-0123456789' });
  await api(`/v2/profiles/${encodeURIComponent(profile.id)}/variants`, { runtime: 'dsh' });
  const created = await api('/v2/human/agents', { requestId: uuid(), name: 'create-a', cwd: daemon.cwd, profile: profile.id, variantId: 'dsh', start: false });
  const a = { name: 'create-a', id: created.cutexSessionId, facade: created.session.native.threadId, path: `/v2/sessions/${encodeURIComponent(created.cutexSessionId)}` };
  daemon.agents.push(a);
  await request(a, 'cutex/session/defaults/update', { patch: { sandboxMode: 'workspace-write', approvalPolicy: 'on-request' } });
  await api('/v2/agent-management/project-mutations', { schema: 'cutex/human-management-project-mutation/v1', action_id: `p-${uuid()}`, project_id: 'create',
    operation: { kind: 'create_project', presentation: { display_name: 'Create', badge_label: 'CR', color: 'cyan' }, workspace_root: daemon.cwd, director_cutex_session_id: a.id } });
  await daemon.online(a);

  await send(a, `CALL agent_create ${JSON.stringify({ name: 'create-b', profile: profile.name, variant: 'dsh', sandbox: 'workspace-write', approval_policy: 'on-request', task: 'say pong' })}`);
  const sessions = await poll(() => api('/v2/sessions'), value => JSON.stringify(value).includes('create-b'), 'B exists', 120000);
  const row = (sessions.sessions ?? sessions.items ?? []).find(session => JSON.stringify(session).includes('create-b'));
  const b = { name: 'create-b', id: row.cutexSessionId ?? row.id };
  b.path = `/v2/sessions/${encodeURIComponent(b.id)}`;
  daemon.agents.push(b);
  const bState = await api(b.path);
  assert.equal(bState.launchBinding?.variantId, 'dsh');
  assert.equal(bState.launchBinding?.runtime, 'dsh');
  assert.equal(bState.launchBinding?.credential?.source, 'stored');

  // The task reaches B (framed as an external event) and B answers it.
  const bTurns = await poll(() => history(b), turns => turns.some(turn => turn.status === 'completed'
    && turn.items.some(item => item.name === 'external_event' && item.output.includes('say pong'))), 'B got the task', 120000);
  assert.ok(agentTexts(bTurns).some(text => text.startsWith('received')), JSON.stringify(bTurns).slice(0, 1500));
  // The delegation result returns to A.
  await poll(() => history(a), turns => turns.flatMap(turn => turn.items).some(item => item.name === 'external_event' && /result/i.test(item.output)), 'result back in A', 120000);
  daemon.markPassed();
});
