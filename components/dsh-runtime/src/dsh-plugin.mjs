// Cutex's plugin inside DSH, inserted by absolute path from the session patch
// cutex writes at every start (src/runtime/dsh/product.rs). Plain ESM, no
// DSH imports: only services DSH itself provides are used, optionally.
//
// Instructions: cutex's own system-prompt text for the agent (`# Cutex`, the
// user AGENTS.md, file references, memory), composed by cutex into a private
// file and added to every top-level agent as one literal (never templated)
// section. Project AGENTS.md / CLAUDE.md stay DSH's own agent-instructions.
//
// Streaming: DSH's ACP carries only committed messages. The live assistant
// stream of the top-level agent (text and reasoning deltas, attempt start
// and end with the committed session seq) goes to the adapter as NDJSON on
// the extra pipe it opened (fd in CUTEX_DSH_STREAM_FD). The native log stays
// the authority; these frames are display only.
//
// Permissions: DSH pins a session's sandbox mode and approval policy when the
// session is first created and keeps them on resume. The agent's cutex
// settings may have changed since, so every top-level session is switched to
// the patch's `cutex-agent` preset (the agent's current settings) when it is
// created or resumed. Subagent children keep DSH's own pinning.
import { readFileSync, writeSync } from 'node:fs';

export const name = 'cutex-bridge';

const topLevel = session => session?.header?.origin !== 'subagent' && session?.header?.parentSession === undefined;

function readInstructions(file) {
  if (typeof file !== 'string' || !file) return '';
  try { return readFileSync(file, 'utf8').trim(); } catch { return ''; }
}

function streamFrames(ctx) {
  const fd = Number(process.env.CUTEX_DSH_STREAM_FD);
  if (!Number.isInteger(fd) || fd < 3) return;
  let open = true;
  const send = value => {
    if (!open) return;
    try { writeSync(fd, `${JSON.stringify(value)}\n`); } catch { open = false; }
  };
  ctx.on('agent/assistant-stream', ({ agent, frame }) => {
    if (!topLevel(agent?.session) || !frame) return;
    const base = { sessionId: agent.session.id, attemptId: String(frame.attemptId) };
    if (frame.type === 'start') send({ type: 'start', ...base, turn: frame.turn, step: frame.step });
    else if (frame.type === 'end') {
      const outcome = frame.outcome?.kind === 'committed' ? { kind: 'committed', eventType: frame.outcome.eventType, seq: frame.outcome.seq } : { kind: 'abandoned' };
      send({ type: 'end', ...base, outcome });
    } else if (frame.type === 'chunk' && (frame.chunk?.type === 'text-delta' || frame.chunk?.type === 'reasoning-delta') && frame.chunk.text) {
      send({ type: 'delta', ...base, kind: frame.chunk.type === 'text-delta' ? 'text' : 'reasoning', text: frame.chunk.text });
    }
  });
}

export function apply(ctx, config = {}) {
  streamFrames(ctx);
  const instructions = readInstructions(config?.instructionsFile);
  if (instructions) ctx.inject(['agents'], inner => {
    const fibers = new Map();
    const install = agent => {
      if (fibers.has(agent) || !topLevel(agent?.session)) return;
      fibers.set(agent, agent.ctx.inject(['systemPrompt'], scope => {
        scope.systemPrompt.section({ name: 'cutex:instructions', order: scope.systemPrompt.getSectionOrder('DEPLOYMENT_PERSONA_SUFFIX') + 50,
          text: instructions, interpolate: false });
      }));
    };
    for (const agent of inner.agents.list()) install(agent);
    inner.on('agent/created', ({ agent }) => install(agent));
    inner.on('agent/disposed', ({ agent }) => {
      const fiber = fibers.get(agent);
      fibers.delete(agent);
      void fiber?.dispose?.()?.catch?.(() => {});
    });
  });
  const preset = typeof config?.permissionPreset === 'string' && config.permissionPreset ? config.permissionPreset : undefined;
  if (preset) ctx.inject(['permissionPresets', 'sessions'], inner => {
    const align = session => {
      if (!topLevel(session)) return;
      try { inner.permissionPresets.set(session, preset); }
      catch (error) { inner.logger?.warn?.(`cutex-bridge: permission preset not applied: ${error?.message ?? error}`); }
    };
    inner.on('session/created', align);
    for (const session of inner.sessions.list()) align(session);
  });
}
