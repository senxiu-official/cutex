"""Single-writer SQLite store; no implicit re-execution on crash recovery.

Canonical model messages and display items are separate. A tool-use response and
its result slots are written atomically *before* invoking any tool. Each slot is
replaced with the observed result afterwards. An interrupted side effect is thus
represented as uncertain, never silently retried and never an orphan tool_use.
"""
from __future__ import annotations

import hashlib
import json
import os
import sqlite3
import time
import uuid
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Iterator

from .errors import RpcError, require
from .asks import ask_ref
from .browser_content import journal_result


def dump(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False)


def identifier() -> str:
    # Protocol accepts UUID strings. These are UUID4, not claimed to be Codex UUID7.
    return str(uuid.uuid4())


# Inbox event type of a `thread/compact/start` turn: runs /compact, no user bubble.
COMPACT_EVENT = "thread.compact"


class Store:
    def __init__(self, directory: Path):
        self.history_mutations: set[str] = set()
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        if directory.is_symlink():
            raise ValueError("state directory must not be a symlink")
        self.directory = directory
        self._lock = open(directory / "writer.lock", "a+b")
        try:
            if os.name == "nt":
                import msvcrt
                self._lock.seek(0)
                self._lock.write(b"0")
                self._lock.flush()
                self._lock.seek(0)
                msvcrt.locking(self._lock.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                import fcntl
                fcntl.flock(self._lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except (OSError, BlockingIOError):
            self._lock.close()
            raise RuntimeError("another app-server owns this state directory") from None
        self.path = directory / "state.sqlite3"
        self.db = sqlite3.connect(self.path, isolation_level=None)
        self.db.row_factory = sqlite3.Row
        self.db.execute("PRAGMA journal_mode=WAL")
        self.db.execute("PRAGMA synchronous=FULL")
        self.db.execute("PRAGMA foreign_keys=ON")
        self.db.execute("PRAGMA journal_size_limit=4194304")
        self.db.execute("PRAGMA wal_autocheckpoint=256")
        if os.name != "nt":
            os.chmod(directory, 0o700)
            os.chmod(self.path, 0o600)
        self.db.executescript('''
          CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS threads(
            id TEXT PRIMARY KEY, model TEXT NOT NULL, cwd TEXT NOT NULL,
            created REAL NOT NULL, updated REAL NOT NULL, name TEXT, instructions TEXT NOT NULL DEFAULT '',
            halted INTEGER NOT NULL DEFAULT 0, archived INTEGER NOT NULL DEFAULT 0
          );
          CREATE TABLE IF NOT EXISTS inputs(
            seq INTEGER PRIMARY KEY AUTOINCREMENT, thread_id TEXT NOT NULL REFERENCES threads(id),
            message_id TEXT NOT NULL, semantic_hash TEXT NOT NULL, payload TEXT NOT NULL,
            turn_id TEXT NOT NULL UNIQUE, state TEXT NOT NULL, created REAL NOT NULL,
            UNIQUE(thread_id,message_id)
          );
          CREATE INDEX IF NOT EXISTS pending_inputs ON inputs(thread_id,state,seq);
          CREATE TABLE IF NOT EXISTS turns(
            id TEXT PRIMARY KEY, thread_id TEXT NOT NULL REFERENCES threads(id),
            state TEXT NOT NULL, started REAL NOT NULL, completed REAL, error TEXT,
            usage TEXT NOT NULL DEFAULT '{}'
          );
          CREATE TABLE IF NOT EXISTS items(
            seq INTEGER PRIMARY KEY AUTOINCREMENT, turn_id TEXT NOT NULL REFERENCES turns(id),
            id TEXT NOT NULL UNIQUE, payload TEXT NOT NULL
          );
          CREATE TABLE IF NOT EXISTS history(
            seq INTEGER PRIMARY KEY AUTOINCREMENT, thread_id TEXT NOT NULL REFERENCES threads(id),
            turn_id TEXT NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL
          );
          CREATE TABLE IF NOT EXISTS tool_calls(
            thread_id TEXT NOT NULL, call_id TEXT NOT NULL, turn_id TEXT NOT NULL,
            state TEXT NOT NULL, arguments TEXT NOT NULL, result TEXT,
            PRIMARY KEY(thread_id,call_id)
          );
          CREATE TABLE IF NOT EXISTS presentations(
            thread_id TEXT NOT NULL, message_id TEXT NOT NULL, receipt TEXT NOT NULL,
            PRIMARY KEY(thread_id,message_id)
          );
          CREATE TABLE IF NOT EXISTS events(
            seq INTEGER PRIMARY KEY AUTOINCREMENT, thread_id TEXT NOT NULL,
            method TEXT NOT NULL, params TEXT NOT NULL, created REAL NOT NULL
          );
          -- Passive notes (cutex agent messaging v2, 0.4.0): never start a turn;
          -- read into the next turn's start or after a tool call (at most once).
          CREATE TABLE IF NOT EXISTS notes(
            seq INTEGER PRIMARY KEY AUTOINCREMENT, thread_id TEXT NOT NULL REFERENCES threads(id),
            message_id TEXT NOT NULL, semantic_hash TEXT NOT NULL, payload TEXT NOT NULL,
            state TEXT NOT NULL, turn_id TEXT, created REAL NOT NULL,
            UNIQUE(thread_id,message_id)
          );
          CREATE INDEX IF NOT EXISTS pending_notes ON notes(thread_id,state,seq);
        ''')
        version = self.db.execute("SELECT value FROM meta WHERE key='schema'").fetchone()
        if version and version[0] not in {"1", "2"}:
            self.close()
            raise RuntimeError("unsupported database version; migration required")
        try:
            self._migrate_schema()
        except BaseException:
            self.close()
            raise
        self.recover()

    def _migrate_schema(self):
        # Schema 2 is a writer fence: r7 cannot understand reverted visibility.
        # Publish the version only after every additive migration succeeds.
        with self.transaction():
            if 'reverted' not in {r[1] for r in self.db.execute('PRAGMA table_info(turns)')}:
                self.db.execute('ALTER TABLE turns ADD COLUMN reverted INTEGER NOT NULL DEFAULT 0')
            self.db.execute('''CREATE TABLE IF NOT EXISTS thread_properties(
                thread_id TEXT PRIMARY KEY REFERENCES threads(id), data TEXT NOT NULL DEFAULT '{}')''')
            self.db.execute('''CREATE TABLE IF NOT EXISTS sdk_turn_messages(
                seq INTEGER PRIMARY KEY AUTOINCREMENT, thread_id TEXT NOT NULL,
                turn_id TEXT NOT NULL, native_uuid TEXT NOT NULL, role TEXT NOT NULL,
                UNIQUE(thread_id,native_uuid))''')
            self.db.execute('''CREATE TABLE IF NOT EXISTS session_operations(
                id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, kind TEXT NOT NULL,
                state TEXT NOT NULL, data TEXT NOT NULL, created REAL NOT NULL)''')
            self.db.execute("INSERT OR REPLACE INTO meta VALUES('schema','2')")

    def thread_properties(self, tid: str) -> dict:
        # The standalone read-only reader also supports pre-r8 databases.
        exists = self.db.execute("SELECT 1 FROM sqlite_master WHERE name='thread_properties'").fetchone()
        row = self.db.execute('SELECT data FROM thread_properties WHERE thread_id=?', (tid,)).fetchone() if exists else None
        return json.loads(row[0]) if row else {}

    @contextmanager
    def history_mutation(self, tid: str):
        require(tid not in self.history_mutations, 'session history mutation already in progress', -32009)
        self.history_mutations.add(tid)
        try:
            yield
        finally:
            self.history_mutations.discard(tid)

    def set_thread_properties(self, tid: str, properties: dict) -> None:
        self.db.execute('INSERT OR REPLACE INTO thread_properties VALUES(?,?)', (tid, dump(properties)))

    def visible_turns_filter(self, alias: str = '') -> str:
        # Column inspection is cached by SQLite and cheap; avoid migrations in an offline reader.
        return (alias + 'reverted=0') if 'reverted' in {r[1] for r in self.db.execute('PRAGMA table_info(turns)')} else '1=1'

    def record_sdk_message(self, tid: str, turn_id: str, native_uuid: str, role: str) -> None:
        """Observed native message identities only; never adapter display IDs."""
        try:
            uuid.UUID(native_uuid)
        except (ValueError, TypeError, AttributeError):
            return
        if role not in {'user', 'assistant'}:
            return
        self.db.execute('INSERT OR IGNORE INTO sdk_turn_messages(thread_id,turn_id,native_uuid,role) VALUES(?,?,?,?)',
                        (tid, turn_id, native_uuid, role))

    def bind_backend(self, backend: str) -> None:
        """Prevent cross-provider history/session reuse; never replay API history into SDK."""
        if backend not in {"mock", "messages", "subscription"}:
            raise ValueError("unsupported backend binding")
        row = self.db.execute("SELECT value FROM meta WHERE key='backend'").fetchone()
        if row and row[0] != backend:
            raise ValueError("state-dir belongs to another backend; select a new state directory")
        if not row:
            if backend == "subscription" and self.db.execute("SELECT 1 FROM threads LIMIT 1").fetchone():
                raise ValueError("legacy state cannot become a subscription transcript; use a fresh state-dir")
            self.db.execute("INSERT INTO meta VALUES('backend',?)", (backend,))

    @contextmanager
    def transaction(self) -> Iterator[None]:
        if self.db.in_transaction:
            savepoint = 'nested_' + uuid.uuid4().hex
            self.db.execute('SAVEPOINT ' + savepoint)
            try:
                yield
            except BaseException:
                self.db.execute('ROLLBACK TO ' + savepoint)
                self.db.execute('RELEASE ' + savepoint)
                raise
            else:
                self.db.execute('RELEASE ' + savepoint)
            return
        self.db.execute("BEGIN IMMEDIATE")
        try:
            yield
        except BaseException:
            self.db.execute("ROLLBACK")
            raise
        else:
            self.db.execute("COMMIT")

    def close(self) -> None:
        self.db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        self.db.close()
        self._lock.close()

    def recover(self) -> None:
        # Starting/resuming the server NEVER creates a paid model request.
        # A turn this store still holds as running was cut off by the previous
        # adapter process (killed, crashed, host restart): it is settled as
        # `interrupted` (what cute-codex reports for a turn a restart cut off),
        # every open item closed, and the settling events are kept for the
        # first subscribers of this process (`recovered_events`): nobody was
        # connected when they were written, so a client would otherwise keep
        # showing the turn and its tool as running.
        self.recovered_events: dict[str, list[tuple[str, dict]]] = {}
        from .native_tools import interrupted_item
        note = "Adapter restarted during this tool call. Outcome is uncertain; inspect side effects before retrying."
        with self.transaction():
            tids = self.db.execute("SELECT DISTINCT thread_id FROM inputs WHERE state IN ('queued','processing')").fetchall()
            for row in tids:
                self.db.execute("UPDATE threads SET halted=1 WHERE id=?", (row[0],))
            active = self.db.execute("SELECT turn_id,thread_id FROM inputs WHERE state='processing'").fetchall()
            stale = self.db.execute("SELECT id,thread_id FROM turns WHERE state='inProgress'").fetchall()
            seen, settle = set(), []
            for turn_id, tid in [(r[0], r[1]) for r in active] + [(r[0], r[1]) for r in stale]:
                if turn_id not in seen:
                    seen.add(turn_id)
                    settle.append((turn_id, tid))
            for turn_id, tid in settle:
                if self.db.execute("SELECT 1 FROM turns WHERE id=? AND state='inProgress'", (turn_id,)).fetchone() is None:
                    # Already terminal (a processing input of a finished turn): inputs only.
                    self.db.execute("UPDATE inputs SET state='uncertain' WHERE turn_id=? AND state='processing'", (turn_id,))
                    continue
                self.clear_browser_replay(tid, turn_id)
                error = {"message": "Server stopped during this turn. Tool effects may be uncertain; inspect before resuming.", "codexErrorInfo": "other", "additionalDetails": None}
                self.db.execute("UPDATE turns SET state='interrupted',completed=?,error=? WHERE id=?", (time.time(), dump(error), turn_id))
                self.db.execute("UPDATE inputs SET state='uncertain' WHERE turn_id=? AND state='processing'", (turn_id,))
                self.db.execute("UPDATE threads SET halted=1 WHERE id=?", (tid,))
                self.db.execute("UPDATE tool_calls SET state='uncertain' WHERE turn_id=? AND state='started'", (turn_id,))
                self.drop_unread_steers(tid)
                events = self.recovered_events.setdefault(tid, [])
                # A restored UI must not keep displaying a tool as running forever.
                for item in self.items(turn_id):
                    if item.get("status") == "inProgress":
                        interrupted_item(item, note)
                        if item.get("status") == "inProgress":
                            item["status"] = "failed"  # any other item kind with a status
                        self.put_item(turn_id, item)
                        params = {"threadId": tid, "turnId": turn_id, "item": item}
                        self.event(tid, "item/completed", params)
                        events.append(("item/completed", params))
                self.event(tid, "turn/completed", {"threadId": tid, "turn": self.turn(turn_id)})
                events.append(("turn/completed", {"threadId": tid, "turn": self.turn(turn_id)}))
                self.event(tid, "thread/status/changed", {"threadId": tid, "status": {"type": "idle"}})
                events.append(("thread/status/changed", {"threadId": tid, "status": {"type": "idle"}}))

    def new_thread(self, model: str, cwd: str, instructions: str = "") -> str:
        tid, now = identifier(), time.time()
        self.db.execute("INSERT INTO threads(id,model,cwd,created,updated,instructions) VALUES(?,?,?,?,?,?)", (tid, model, cwd, now, now,instructions))
        return tid

    def set_thread_model(self, tid: str, model: str) -> None:
        # Preserve the creation descriptor for idempotent controller ensure calls.
        self.db.execute("CREATE TABLE IF NOT EXISTS thread_launch_models(thread_id TEXT PRIMARY KEY REFERENCES threads(id), model TEXT NOT NULL)")
        with self.transaction():
            self.db.execute("INSERT OR IGNORE INTO thread_launch_models SELECT id,model FROM threads WHERE id=?", (tid,))
            self.db.execute("UPDATE threads SET model=?,updated=? WHERE id=?", (model,time.time(),tid))

    def ensure_thread(self, tid: str, model: str, cwd: str, instructions: str) -> bool:
        """Create the controller's preallocated identity, or verify an exact retry.

        This never submits a prompt, resumes a queue, or starts a native process.
        Keeping creation and identity in one SQLite transaction makes a lost RPC
        response recoverable without allocating a second conversation.
        """
        try:
            canonical = str(uuid.UUID(tid))
        except (ValueError, TypeError, AttributeError):
            canonical = None
        require(canonical == tid, "threadId must be a canonical UUID")
        with self.transaction():
            row = self.db.execute("SELECT model,cwd,instructions FROM threads WHERE id=?", (tid,)).fetchone()
            if row:
                has_binding = self.db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='thread_launch_models'").fetchone()
                original = self.db.execute("SELECT model FROM thread_launch_models WHERE thread_id=?", (tid,)).fetchone() if has_binding else None
                require((original[0] if original else row[0], row[1], row[2]) == (model, cwd, instructions), "thread identity already has different settings", -32009)
                return False
            now = time.time()
            self.db.execute("INSERT INTO threads(id,model,cwd,created,updated,instructions) VALUES(?,?,?,?,?,?)",
                            (tid, model, cwd, now, now, instructions))
            return True

    def thread(self, tid: str) -> dict:
        row = self.db.execute("SELECT * FROM threads WHERE id=?", (tid,)).fetchone()
        require(row is not None, "thread not found", -32004)
        return dict(row)

    def pending(self, tid: str) -> bool:
        return self.db.execute("SELECT 1 FROM inputs WHERE thread_id=? AND state IN ('queued','processing') LIMIT 1", (tid,)).fetchone() is not None

    def input_known(self, tid: str, mid: str) -> bool:
        return self.db.execute("SELECT 1 FROM inputs WHERE thread_id=? AND message_id=?", (tid, mid)).fetchone() is not None

    def input_status(self, tid: str, mid: str) -> dict:
        self.thread(tid)
        row = self.db.execute("SELECT * FROM inputs WHERE thread_id=? AND message_id=?", (tid, mid)).fetchone()
        if row is None:
            note = self.db.execute("SELECT * FROM notes WHERE thread_id=? AND message_id=?", (tid, mid)).fetchone()
            require(note is not None, "input not found", -32004)
            return self.note_receipt(note)
        return self.receipt(row)

    @staticmethod
    def note_receipt(row: sqlite3.Row | dict, duplicate: bool = False) -> dict:
        """A note has no turn of its own: `turnId` stays null (stable receipt)."""
        return {"threadId": row["thread_id"], "messageId": row["message_id"], "turnId": None,
                "sequence": row["seq"], "semanticSha256": row["semantic_hash"],
                "state": "queued" if row["state"] == "pending" else row["state"],
                "durable": True, "duplicate": duplicate, "receiptScope": "local-inbox",
                "sourceVerified": False, "deliveryMode": "passive"}

    def enqueue_note(self, tid: str, mid: str, payload: dict, max_pending: int) -> dict:
        require(tid not in self.history_mutations, 'session history mutation is in progress', -32009)
        thread = self.thread(tid)
        require(not thread["archived"], "thread is archived", -32009)
        canonical = dump(payload)
        digest = hashlib.sha256(canonical.encode()).hexdigest()
        with self.transaction():
            require(not self.input_known(tid, mid), "messageId reused with different semantic content", -32010)
            old = self.db.execute("SELECT * FROM notes WHERE thread_id=? AND message_id=?", (tid, mid)).fetchone()
            if old:
                require(old["semantic_hash"] == digest, "messageId reused with different semantic content", -32010)
                return self.note_receipt(old, True)
            count = self.db.execute("SELECT count(*) FROM notes WHERE thread_id=? AND state='pending'", (tid,)).fetchone()[0]
            require(count < max_pending, "inbox capacity reached", -32001)
            self.db.execute("INSERT INTO notes(thread_id,message_id,semantic_hash,payload,state,created) VALUES(?,?,?,?,'pending',?)",
                            (tid, mid, digest, canonical, time.time()))
            row = self.db.execute("SELECT * FROM notes WHERE thread_id=? AND message_id=?", (tid, mid)).fetchone()
            return self.note_receipt(row)

    def pending_notes(self, tid: str) -> int:
        return self.db.execute("SELECT count(*) FROM notes WHERE thread_id=? AND state='pending'", (tid,)).fetchone()[0]

    def take_notes(self, tid: str, turn_id: str) -> list[tuple[dict, str]]:
        """Mark pending notes as read into `turn_id` (at most once) and record
        their timeline items: [(item, model frame)], oldest first."""
        with self.transaction():
            return self._take_notes(tid, turn_id)

    def _take_notes(self, tid: str, turn_id: str) -> list[tuple[dict, str]]:
        taken = []
        rows = self.db.execute("SELECT * FROM notes WHERE thread_id=? AND state='pending' ORDER BY seq", (tid,)).fetchall()
        for row in rows:
            payload = json.loads(row["payload"])
            item = self.external_item(row["message_id"], payload)
            self.put_item(turn_id, item)
            self.db.execute("UPDATE notes SET state='injected',turn_id=? WHERE seq=?", (turn_id, row["seq"]))
            taken.append((item, self.project_input(payload)[0]["text"]))
        return taken

    @staticmethod
    def external_item(mid: str, payload: dict) -> dict:
        """The Codex-shaped timeline item of an external input (cutex
        `external_event`): same parsing for both runtimes."""
        native = payload.get('metadata', {}).get('nativeExternalInput') is True
        kind = "message" if payload.get("eventType") == "agent_message" and not native else payload.get("eventType")
        output = {"source": payload["source"], "type": kind, "text": payload["text"]}
        return {"type": "functionCallOutput", "id": mid, "name": "external_event", "namespace": "external",
                "output": json.dumps(output, ensure_ascii=False, separators=(",", ":")),
                "externalInputView": payload.get("view")}

    @staticmethod
    def receipt(row: sqlite3.Row | dict, duplicate: bool = False) -> dict:
        return {"threadId": row["thread_id"], "messageId": row["message_id"],
                "turnId": row["turn_id"], "sequence": row["seq"],
                "semanticSha256": row["semantic_hash"], "state": row["state"],
                "durable": True, "duplicate": duplicate,
                "receiptScope": "local-inbox", "sourceVerified": False,
                **Store.receipt_ask(row)}

    @staticmethod
    def receipt_ask(row: sqlite3.Row | dict) -> dict:
        """Ask events echo their ask ids (cutex's attempt <-> turn mapping)."""
        if "payload" not in row.keys():
            return {}
        ref = ask_ref(json.loads(row["payload"]))
        return {"ask": ref} if ref is not None else {}

    def check_user_continuation(self, tid: str) -> None:
        if self.thread(tid)['halted']:
            latest = self.db.execute('SELECT state FROM turns WHERE thread_id=? AND reverted=0 ORDER BY rowid DESC LIMIT 1', (tid,)).fetchone()
            require(latest is not None and latest[0] in {'interrupted', 'failed'},
                    'queue paused without a terminal failed/interrupted turn; inspect the runtime', -32009)

    def user_turn_busy(self, tid: str) -> bool:
        # A fresh user instruction may take priority over untouched queued inputs
        # after a terminal failure; an actually processing input remains busy.
        if self.thread(tid)['halted']:
            return self.db.execute("SELECT 1 FROM inputs WHERE thread_id=? AND state='processing' LIMIT 1", (tid,)).fetchone() is not None
        return self.pending(tid)

    def enqueue(self, tid: str, mid: str, payload: dict, max_pending: int, *, resume_interrupted: bool = False) -> dict:
        require(tid not in self.history_mutations, 'session history mutation is in progress', -32009)
        thread = self.thread(tid)
        require(not thread["archived"], "thread is archived", -32009)
        canonical = dump(payload)
        digest = hashlib.sha256(canonical.encode()).hexdigest()
        with self.transaction():
            if resume_interrupted:
                self.check_user_continuation(tid)
            old = self.db.execute("SELECT * FROM inputs WHERE thread_id=? AND message_id=?", (tid, mid)).fetchone()
            if old:
                require(old["semantic_hash"] == digest, "messageId reused with different semantic content", -32010)
                return self.receipt(old, True)
            note = self.db.execute("SELECT 1 FROM notes WHERE thread_id=? AND message_id=?", (tid, mid)).fetchone()
            require(note is None, "messageId reused with different semantic content", -32010)
            count = self.db.execute("SELECT count(*) FROM inputs WHERE thread_id=? AND state IN ('queued','processing')", (tid,)).fetchone()[0]
            require(count < max_pending, "inbox capacity reached", -32001)
            self.db.execute("INSERT INTO inputs(thread_id,message_id,semantic_hash,payload,turn_id,state,created) VALUES(?,?,?,?,?,'queued',?)", (tid, mid, digest, canonical, identifier(), time.time()))
            self.input_presentation(tid, mid)
            if resume_interrupted:
                # Accepting a fresh user turn resumes dispatch, never the old input.
                # Commit both together so invalid submissions cannot unpause the queue.
                self.db.execute('UPDATE threads SET halted=0 WHERE id=?', (tid,))
            return self.input_status(tid, mid)

    def claim(self, tid: str, message_id: str | None = None) -> dict | None:
        if tid in self.history_mutations:
            return None
        with self.transaction():
            thread = self.thread(tid)
            if thread["halted"] or thread["archived"]:
                return None
            active = self.db.execute("SELECT * FROM inputs WHERE thread_id=? AND state='processing' LIMIT 1", (tid,)).fetchone()
            if active:
                return dict(active)
            row = self.db.execute("SELECT * FROM inputs WHERE thread_id=? AND state='queued' AND (? IS NULL OR message_id=?) ORDER BY seq LIMIT 1", (tid, message_id, message_id)).fetchone()
            if row is None:
                return None
            payload = json.loads(row["payload"])
            content = self.project_input(payload)
            now = time.time()
            self.db.execute("INSERT INTO turns(id,thread_id,state,started) VALUES(?,?,'inProgress',?)", (row["turn_id"], tid, now))
            # Clients saw this thread again once a new turn runs: no more replay.
            self.recovered_events.pop(tid, None)
            owner = payload["source"]["kind"] == "user"
            compact = payload.get("eventType") == COMPACT_EVENT
            parts = [{"type": "text", "text": content[0]["text"], "text_elements": []}] if content[0]["text"] else []
            item = {"type": "userMessage", "id": identifier(), "clientId": payload.get("clientId"),
                    "content": parts + list(payload.get("display") or [])}
            if owner and not compact:
                self.put_item(row["turn_id"], item)
            # Notes waiting for this thread are read at the start of every turn
            # (Codex reads passive input at a turn's first request too): an owner
            # turn, an owner's wake (cutex/agentBus/wake: "read the unread notes")
            # and a turn an agent or service started. Their items follow the
            # owner's message and precede an agent's input; the model reads them
            # before the input text. The subscription backend sends them to an
            # owner turn as separate text blocks before the owner's text
            # (`turn_note_frames`), so only the owner's own text expands @path
            # mentions. `/compact` reads none: they wait for the next turn.
            notes = [] if compact else self._take_notes(tid, row["turn_id"])
            model_content = content
            if notes:
                model_content = [{"type": "text", "text": "\n\n".join([frame for _, frame in notes] + [content[0]["text"]])}]
            if not self.db.execute("SELECT 1 FROM meta WHERE key='backend' AND value='subscription'").fetchone():
                self.db.execute("INSERT INTO history(thread_id,turn_id,role,content) VALUES(?,?,'user',?)", (tid, row["turn_id"], dump(model_content)))
            if not owner and (isinstance(payload.get("view"), dict) or payload.get('metadata', {}).get('nativeExternalInput') is True):
                # cutex r208 inputs with a view (agent wakes, 委托 events) show as
                # `external_event` items, keyed by their message id.
                self.put_item(row["turn_id"], self.external_item(row["message_id"], payload))
            self.db.execute("UPDATE inputs SET state='processing' WHERE seq=?", (row["seq"],))
            self.db.execute("UPDATE threads SET updated=? WHERE id=?", (now, tid))
            result = dict(row)
            result["state"] = "processing"
            # An owner turn's prompt stays its own text (see `turn_note_frames`).
            result["prompt"] = content[0]["text"] if owner else model_content[0]["text"]
            return result

    def turn_note_frames(self, turn_id: str) -> list[str]:
        """Model frames of the notes read in at the start of `turn_id`, oldest
        first. Called before the turn's first request (no PostToolUse yet)."""
        rows = self.db.execute("SELECT payload FROM notes WHERE turn_id=? AND state='injected' ORDER BY seq", (turn_id,)).fetchall()
        return [self.project_input(json.loads(row[0]))[0]["text"] for row in rows]

    def turn_source(self, turn_id: str) -> dict | None:
        """Who started a turn: {kind, id, name} of its input; None if unknown."""
        row = self.db.execute("SELECT payload FROM inputs WHERE turn_id=?", (turn_id,)).fetchone()
        if row is None:
            return None
        payload = json.loads(row[0])
        source = payload.get("source") if isinstance(payload.get("source"), dict) else {}
        metadata = payload.get("metadata") if isinstance(payload.get("metadata"), dict) else {}
        name = metadata.get("sourceName")
        return {"kind": source.get("kind"), "id": source.get("id"),
                "name": name if isinstance(name, str) and name.strip() else None}

    @staticmethod
    def project_input(payload: dict) -> list[dict]:
        if payload["source"]["kind"] == "user":
            value = payload["text"]
        else:
            # JSON framing protects record boundaries; this is not a prompt-injection sandbox.
            frame = {"source": payload["source"], "eventType": payload["eventType"], "text": payload["text"]}
            ref = ask_ref(payload)
            if ref is not None:
                frame["ask"] = ref
            value = "EXTERNAL_INPUT_JSON\n" + dump(frame)
        return [{"type": "text", "text": value}]

    def put_item(self, turn_id: str, item: dict) -> None:
        self.db.execute("INSERT INTO items(turn_id,id,payload) VALUES(?,?,?) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", (turn_id, item["id"], dump(item)))

    def items(self, turn_id: str) -> list[dict]:
        return [json.loads(row[0]) for row in self.db.execute("SELECT payload FROM items WHERE turn_id=? ORDER BY seq", (turn_id,))]

    def turn(self, turn_id: str, *, include_items: bool = True) -> dict:
        row = self.db.execute("SELECT * FROM turns WHERE id=?", (turn_id,)).fetchone()
        require(row is not None, "turn not found", -32004)
        return {"id": row["id"], "items": self.items(turn_id) if include_items else [],
                "itemsView": "full" if include_items else "notLoaded", "status": row["state"],
                "error": json.loads(row["error"]) if row["error"] else None,
                "startedAt": int(row["started"]),
                "completedAt": int(row["completed"]) if row["completed"] else None,
                "durationMs": int((row["completed"]-row["started"])*1000) if row["completed"] else None}

    def model_history(self, tid: str, limit_bytes: int) -> list[dict]:
        size = self.db.execute("SELECT COALESCE(sum(length(CAST(content AS BLOB))),0) FROM history WHERE thread_id=?", (tid,)).fetchone()[0]
        require(size <= limit_bytes, "History byte budget reached; explicit compaction/new thread required. No silent truncation.", -32012)
        return [{"role": row[0], "content": json.loads(row[1])} for row in self.db.execute("SELECT role,content FROM history WHERE thread_id=? ORDER BY seq", (tid,))]

    def append_assistant(self, tid: str, turn_id: str, content: list[dict]) -> int | None:
        calls = [b for b in content if b["type"] == "tool_use"]
        with self.transaction():
            self.db.execute("INSERT INTO history(thread_id,turn_id,role,content) VALUES(?,?,'assistant',?)", (tid, turn_id, dump(content)))
            if not calls:
                return None
            results = [{"type": "tool_result", "tool_use_id": b["id"], "is_error": True,
                        "content": "Tool was not started. Do not infer success."} for b in calls]
            cursor = self.db.execute("INSERT INTO history(thread_id,turn_id,role,content) VALUES(?,?,'user',?)", (tid, turn_id, dump(results)))
            return cursor.lastrowid

    def tool_started(self, tid: str, turn_id: str, call: dict, slot: int) -> None:
        with self.transaction():
            old = self.db.execute("SELECT 1 FROM tool_calls WHERE thread_id=? AND call_id=?", (tid, call["id"])).fetchone()
            require(old is None, "duplicate provider tool call ID; refusing re-execution", -32010)
            self.db.execute("INSERT INTO tool_calls VALUES(?,?,?,'started',?,NULL)", (tid, call["id"], turn_id, dump(call)))
            self._replace_slot(slot, call["id"], {"type": "tool_result", "tool_use_id": call["id"], "is_error": True,
                "content": "Tool execution outcome is uncertain (interrupted before recording its result). Inspect side effects; do not automatically retry."})

    def tool_finished(self, tid: str, call_id: str, slot: int, output: str | list[dict], success: bool) -> None:
        result = {"type": "tool_result", "tool_use_id": call_id, "content": output, "is_error": not success}
        with self.transaction():
            self._replace_slot(slot, call_id, result)
            call = self.db.execute('SELECT arguments FROM tool_calls WHERE thread_id=? AND call_id=?', (tid, call_id)).fetchone()
            saved = journal_result(json.loads(call[0]).get('name', ''), result) if call else result
            self.db.execute("UPDATE tool_calls SET state='completed',result=? WHERE thread_id=? AND call_id=?", (dump(saved), tid, call_id))

    def clear_browser_replay(self, tid: str, turn_id: str) -> None:
        # Same-turn SDK retries may use a pointer to the canonical display item.
        # Retire that pointer on every terminal path without touching history.
        self.db.execute("""UPDATE tool_calls SET result=json_remove(result,'$.browserReplayItemId')
            WHERE thread_id=? AND turn_id=? AND json_type(result,'$.browserReplayItemId')='text'""", (tid, turn_id))

    def _replace_slot(self, seq: int, call_id: str, result: dict) -> None:
        row = self.db.execute("SELECT content FROM history WHERE seq=?", (seq,)).fetchone()
        blocks = json.loads(row[0])
        blocks = [result if b.get("tool_use_id") == call_id else b for b in blocks]
        self.db.execute("UPDATE history SET content=? WHERE seq=?", (dump(blocks), seq))

    def finish(self, tid: str, turn_id: str, state: str, error: dict | None = None) -> None:
        with self.transaction():
            self.clear_browser_replay(tid, turn_id)
            self.db.execute("UPDATE turns SET state=?,completed=?,error=? WHERE id=?", (state, time.time(), dump(error) if error else None, turn_id))
            self.db.execute("UPDATE inputs SET state=? WHERE turn_id=?", (state, turn_id))
            self.db.execute("UPDATE threads SET updated=?,halted=? WHERE id=?", (time.time(), state != "completed", tid))
            if state != "completed":
                self.db.execute("UPDATE tool_calls SET state='uncertain' WHERE turn_id=? AND state='started'", (turn_id,))
                self.drop_unread_steers(tid)

    def drop_unread_steers(self, tid: str) -> list[int]:
        """An owner steer (`turn/steer`) the running turn never read is dropped
        when that turn ends interrupted or failed, as cute-codex drops pending
        steer input on an abort: clients resend it (the desktop's "interrupt &
        send" does, under a new id). Kept queued it was injected into a later,
        unrelated turn at its first tool call, so the model got the message
        twice. A turn that completes still runs it as the next ordinary turn.

        Also called when the owner requests the interrupt (before the abort
        reaches Claude Code: a PostToolUse in between, or a turn that still
        completes, must not deliver it besides the client's resend). Image
        steers have no busy_inputs row (they never ride a hook) and are
        dropped too.
        Returns the dropped input seqs."""
        if not self.db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='busy_inputs'").fetchone():
            return []
        rows = self.db.execute("""SELECT i.seq,i.payload FROM inputs i LEFT JOIN busy_inputs b
            ON b.thread_id=i.thread_id AND b.message_id=i.message_id
            WHERE i.thread_id=? AND i.state='queued' AND b.context_turn_id IS NULL""", (tid,)).fetchall()
        dropped = []
        for row in rows:
            payload = json.loads(row["payload"])
            if payload.get("source", {}).get("kind") != "user" or (payload.get("metadata") or {}).get("steer") is not True:
                continue
            self.db.execute("UPDATE inputs SET state='interrupted' WHERE seq=?", (row["seq"],))
            dropped.append(row["seq"])
        return dropped

    def restore_steers(self, tid: str, seqs: list[int]) -> None:
        """Undo an interrupt-time drop when the interrupt request itself failed
        and the turn keeps running (its steers then still wait for it)."""
        for seq in seqs:
            self.db.execute("""UPDATE inputs SET state='queued' WHERE seq=? AND thread_id=? AND state='interrupted'
                AND EXISTS(SELECT 1 FROM turns WHERE thread_id=? AND state='inProgress')""", (seq, tid, tid))

    @staticmethod
    def stamp_event(method: str, params: dict) -> dict:
        if method in {"item/started", "item/completed"}:
            params = dict(params)
            key = "startedAtMs" if method == "item/started" else "completedAtMs"
            params.setdefault(key, int(time.time() * 1000))
        return params

    def input_presentation(self, tid: str, mid: str) -> dict | None:
        from .presentation import input_presentation
        saved = self.db.execute("SELECT receipt FROM presentations WHERE thread_id=? AND message_id=?", (tid, mid)).fetchone()
        if saved:
            return json.loads(saved[0])
        row = self.db.execute("SELECT payload,created FROM inputs WHERE thread_id=? AND message_id=?", (tid, mid)).fetchone()
        require(row is not None, "input not found", -32004)
        payload = json.loads(row['payload'])
        if payload['source']['kind'] == 'user':
            return None
        receipt = input_presentation(tid, mid, payload, row['created'])
        self.db.execute('INSERT INTO presentations VALUES(?,?,?)', (tid, mid, dump(receipt)))
        return receipt

    def event(self, tid: str, method: str, params: dict) -> int:
        params = self.stamp_event(method, params)
        stored = dict(params)
        # Completed content already lives in items/turns. Events retain references.
        if method == 'item/completed' and 'item' in stored:
            stored['_itemRef'] = stored.pop('item')['id']
        elif method == 'turn/completed' and 'turn' in stored:
            stored['_turnRef'] = stored.pop('turn')['id']
        return self.db.execute("INSERT INTO events(thread_id,method,params,created) VALUES(?,?,?,?)", (tid, method, dump(stored), time.time())).lastrowid

    def hydrate_event(self, tid: str, params: dict) -> dict:
        if '_itemRef' in params:
            row = self.db.execute('SELECT payload FROM items WHERE id=?', (params.pop('_itemRef'),)).fetchone()
            params['item'] = json.loads(row[0])
        if '_turnRef' in params:
            params['turn'] = self.turn(params.pop('_turnRef'))
        if '_inputPresentationRef' in params:
            params['item'] = self.input_presentation(tid, params.pop('_inputPresentationRef'))
        return params

    def events(self, tid: str, after: int, limit: int) -> list[dict]:
        self.thread(tid)
        return [{"sequence": r[0], "method": r[1], "params": self.hydrate_event(tid, json.loads(r[2]))} for r in self.db.execute("SELECT seq,method,params FROM events WHERE thread_id=? AND seq>? ORDER BY seq LIMIT ?", (tid, after, limit))]

    def timeline(self, tid: str, cursor: int | None, limit: int) -> dict:
        from .history import PAGE_BYTES, ITEM_BYTES, size
        self.thread(tid)
        require(limit > 0, 'timeline limit must be positive')
        limit = min(limit, 200)
        rows = self.db.execute("""SELECT seq,method,params FROM events WHERE thread_id=? AND seq<?
            AND method IN ('item/completed','turn/started','turn/completed','thread/presentation/appended')
            AND NOT EXISTS (SELECT 1 FROM turns t WHERE t.reverted=1 AND
              t.id=COALESCE(json_extract(events.params,'$.turnId'),json_extract(events.params,'$._turnRef'),json_extract(events.params,'$.turn.id')))
            ORDER BY seq DESC LIMIT ?""", (tid, cursor if cursor is not None else 9223372036854775807, limit + 1))
        entries, positions, used, more = [], [], 0, False
        for row in rows:
            if len(entries) == limit:
                more = True
                break
            p = json.loads(row['params'])
            # Turn boundaries only need metadata, never all tool outputs in that turn.
            if '_turnRef' in p:
                p['turn'] = self.turn(p.pop('_turnRef'), include_items=False)
            if '_itemRef' in p:
                length = self.db.execute('SELECT length(CAST(payload AS BLOB)) FROM items WHERE id=?', (p['_itemRef'],)).fetchone()
                require(length and length[0] <= ITEM_BYTES, 'single history item exceeds transport limit; inspect the local transcript', -32001)
            p = self.hydrate_event(tid, p)
            entry = {'position': row['seq']}
            if row['method'] == 'thread/presentation/appended':
                entry.update(type='presentation', item=p['item'])
            elif row['method'] == 'item/completed':
                entry.update(type='item', turnId=p['turnId'], item=p['item'])
            else:
                turn = p['turn']
                entry.update(type='turnStarted' if row['method'] == 'turn/started' else 'turnCompleted', turnId=turn['id'], startedAt=turn.get('startedAt'))
                if entry['type'] == 'turnCompleted':
                    entry.update({k: turn.get(k) for k in ('status','error','completedAt','durationMs')})
            cost = size(entry)
            require(cost <= ITEM_BYTES, 'single timeline entry exceeds transport limit; inspect the local transcript', -32001)
            if entries and used + cost > PAGE_BYTES:
                more = True
                break
            entries.append(entry)
            positions.append(row['seq'])
            used += cost
        return {'data': list(reversed(entries)), 'nextCursor': str(positions[-1]) if more else None,
                'activeRealtimeSessionAtPageStart': None}
