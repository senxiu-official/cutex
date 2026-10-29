// The owner's approval card text for a Kimi ask (r266 integration E2E: "Kimi asks to Running: …").
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { approvalText } from '../src/runtime.mjs';

const ask = text => ({ toolCall: { content: [{ type: 'content', content: { type: 'text', text } }] } });

test('approval reason names Kimi\'s action after a colon (2.3.3 sends "Requesting approval to Running: <command>")', () => {
  const call = { name: 'Bash', rawInput: { command: 'echo hi' } };
  assert.equal(approvalText(call, ask('Requesting approval to Running: echo hi'), '/w').reason, 'Kimi asks for approval: Running: echo hi');
  assert.equal(approvalText(call, ask('Requesting approval to edit notes.txt'), '/w').reason, 'Kimi asks for approval: edit notes.txt');
  assert.equal(approvalText(call, {}, '/w').reason, 'Kimi asks to run Bash once.');
  assert.equal(approvalText(call, ask('Running: echo hi'), '/w').command, 'echo hi');
});
