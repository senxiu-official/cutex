import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { install, binary, privateDaemon, uuid, offlinePatch } from './private-daemon.mjs';
import { stopPauseScenario } from './stop-pause-scenario.mjs';

// r266 stop / project pause on DSH agents: private debug daemon + real DSH +
// offline model (`HOLD-TURN` runs until ACP cancel). Scenario and coverage:
// stop-pause-scenario.mjs. Opt-in (DSH_PRODUCT_INSTALL + DSH_PRODUCT_DAEMON).
const plugin = fileURLToPath(new URL('./delivery-mock-llm.mjs', import.meta.url));

test('r266 stop and project pause on DSH agents (exact ACP stop, pause admission)', { skip: !install || !binary, timeout: 900000 }, async t => {
  const daemon = await privateDaemon(t, { label: 'stoppause', patch: offlinePatch(plugin), env: { DSH_STOPPAUSE_TEST_KEY: `dummy-stoppause-key-${uuid()}` } });
  const { api } = daemon;
  const { profile } = await api('/v2/profiles', { name: 'DSH Stop Pause Offline', kind: 'dsh', options: { dsh: { model: 'offline', apiKeyEnv: 'DSH_STOPPAUSE_TEST_KEY' } } });
  const a = await daemon.createAgent('dsh-director', profile.id);
  const b = await daemon.createAgent('dsh-member', profile.id);
  const project = 'dshstoppause';
  await api('/v2/agent-management/project-mutations', { schema: 'cutex/human-management-project-mutation/v1', action_id: `p-${uuid()}`, project_id: project,
    operation: { kind: 'create_project', presentation: { display_name: 'DSH stop pause', badge_label: 'SP', color: 'cyan' }, workspace_root: daemon.cwd, director_cutex_session_id: a.id } });
  await api('/v2/agent-management/project-mutations', { schema: 'cutex/human-management-project-mutation/v1', action_id: `m-${uuid()}`, project_id: project,
    operation: { kind: 'add_member', cutex_session_id: b.id } });
  // r267: an agent outside the project delivers to the Director and the member under pause-all.
  const outside = await daemon.createAgent('dsh-outside', profile.id);
  await daemon.online(a); await daemon.online(b); await daemon.online(outside);
  const results = await stopPauseScenario(t, daemon, { a, b, project, runtime: 'dsh', outside,
    hold: marker => `HOLD-TURN ${marker}`, shell: marker => `SHELL-HOLD ${marker}`, delegateCall: args => `CALL agent_delegate ${JSON.stringify(args)}` });
  console.log(`DSH STOP/PAUSE: ${JSON.stringify(results)}`);
  daemon.markPassed();
});
