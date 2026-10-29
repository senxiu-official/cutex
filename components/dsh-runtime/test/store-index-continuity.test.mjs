// Timeline index continuity across component rollback: an older component
// (r264 / r265 1c36212c) appends to `events` without the derived index; the
// current component must notice and index those events instead of serving
// a stale index (QA host-a R1: rawEvents=2, indexed turns=[before]).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { DatabaseSync } from 'node:sqlite';
import { TimelineStore } from '../src/store.mjs';
import { HistoryReader } from '../src/history-reader.mjs';

const threadId = 'native';
const turnEvents = turn => [
  { method: 'turn/started', params: { threadId, turn: { id: turn, status: 'inProgress', items: [] } } },
  { method: 'item/completed', params: { threadId, turnId: turn, item: { id: `${turn}-user`, type: 'userMessage', content: [{ type: 'text', text: turn }] } } },
  { method: 'item/completed', params: { threadId, turnId: turn, item: { id: `${turn}-agent`, type: 'agentMessage', text: `answer ${turn}` } } },
  { method: 'turn/completed', params: { threadId, turn: { id: turn, status: 'completed', items: [] } } },
];
// What the 1c36212c store's append does: INSERT INTO events(body), nothing else.
const oldAppend = (database, events) => {
  const db = new DatabaseSync(database);
  try { for (const event of events) db.prepare('INSERT INTO events(body) VALUES (?)').run(JSON.stringify(event)); }
  finally { db.close(); }
};

function fixture(t) {
  const dir = mkdtempSync(join(tmpdir(), 'dsh-index-continuity-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const database = join(dir, 'timeline.sqlite');
  const store = new TimelineStore(database);
  store.bind(threadId, dir);
  for (const event of turnEvents('before')) store.append(event);
  store.append({ method: 'dsh/contextUsage/updated', params: { used: 1, size: 10 } });
  store.close();
  return { dir, database };
}

test('events an older component appended are indexed on the next writable open', t => {
  const { database } = fixture(t);
  oldAppend(database, [...turnEvents('during-rollback'), { method: 'dsh/contextUsage/updated', params: { used: 2, size: 10 } }]);
  const store = new TimelineStore(database);
  try {
    assert.deepEqual(store.turnIds(), ['before', 'during-rollback']);
    assert.equal(store.hasTurn('during-rollback'), true);
    assert.equal(store.turnCompleted('during-rollback'), true);
    assert.equal(store.turnEvents('during-rollback').length, 4);
    assert.equal(store.latest('dsh/contextUsage/updated').params.used, 2);
    assert.equal(store.hasTimelineIndex(), true);
    // Appends keep the mark in step; a second open does nothing.
    for (const event of turnEvents('after')) store.append(event);
  } finally { store.close(); }
  const again = new TimelineStore(database);
  try { assert.deepEqual(again.turnIds(), ['before', 'during-rollback', 'after']); }
  finally { again.close(); }
});

test('a read-only reader never trusts a lagging index', t => {
  const { dir, database } = fixture(t);
  oldAppend(database, turnEvents('during-rollback'));
  const reader = new HistoryReader({ database, threadId: 'facade', nativeSessionId: threadId, cwd: dir });
  try {
    assert.equal(reader.store.hasTimelineIndex(), false);
    const turns = reader.handle('thread/read', { threadId: 'facade', includeTurns: true }).thread.turns.map(turn => turn.id);
    assert.deepEqual(turns, ['before', 'during-rollback']);
    const page = reader.handle('thread/turns/list', { threadId: 'facade', sortDirection: 'desc', limit: 1 });
    assert.equal(page.data[0].id, 'during-rollback');
  } finally { reader.close(); }
});

test('an index without its mark (written before the mark existed) is rebuilt once', t => {
  const { database } = fixture(t);
  const db = new DatabaseSync(database);
  db.exec("DELETE FROM store_meta WHERE key='timelineIndexedSeq'");
  db.close();
  oldAppend(database, turnEvents('unmarked'));
  const store = new TimelineStore(database);
  try {
    assert.deepEqual(store.turnIds(), ['before', 'unmarked']);
    assert.equal(store.hasTimelineIndex(), true);
  } finally { store.close(); }
});
