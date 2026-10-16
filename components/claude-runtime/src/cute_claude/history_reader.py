"""Read-only history of a stopped Claude agent, for cutex's offline history route.

`python -m cute_claude.history_reader --state-dir DIR` answers JSON-RPC lines on
stdio: `initialize`, `thread/read`, `thread/turns/list` and `thread/items/list`,
from the adapter's SQLite state with the same shapes as the running adapter.
The database is opened read-only and the writer lease is never taken, so a warm
reader never blocks the agent from starting. Nothing else is served.
"""
from __future__ import annotations

import argparse
import json
import platform
import sqlite3
import sys
from pathlib import Path

from . import __version__
from .errors import RpcError, text
from .history import History, thread_wire
from .store import Store, dump

METHODS = ("initialize", "thread/read", "thread/turns/list", "thread/items/list")


class ReadOnlyStore(Store):
    """`Store` queries over a read-only connection: no lease, schema writes or recovery."""

    def __init__(self, db: sqlite3.Connection):  # noqa: super().__init__ would take the writer lease
        self.db = db


def open_database(state_dir: Path) -> sqlite3.Connection | None:
    path = state_dir / "state.sqlite3"
    if not path.is_file():
        return None  # Never started: no history yet.
    db = sqlite3.connect(f"{path.resolve().as_uri()}?mode=ro", uri=True, isolation_level=None)
    db.row_factory = sqlite3.Row
    db.execute("PRAGMA query_only=1")
    return db


class Reader:
    def __init__(self, state_dir: Path):
        self.state_dir = state_dir
        self.db = open_database(state_dir)
        self.store = ReadOnlyStore(self.db) if self.db else None
        self.history = History(self.store, read_only=True) if self.store else None

    def effort(self, tid: str):
        try:
            row = self.db.execute("SELECT effort FROM thread_efforts WHERE thread_id=?", (tid,)).fetchone()
        except sqlite3.OperationalError:  # table created by the subscription engine only
            return None
        return row[0] if row else None

    def handle(self, method: str, params: dict) -> dict:
        if method == "initialize":
            return {"userAgent": f"cute-claude-history-reader/{__version__}", "codexHome": str(self.state_dir),
                    "platformFamily": "windows" if sys.platform == "win32" else "unix",
                    "platformOs": platform.system().lower()}
        if method not in METHODS:
            raise RpcError(-32601, f"{method} is not served by the offline Claude history reader",
                           {"unsupported": True, "runtime": "claude", "feature": method, "method": method})
        tid = text(params.get("threadId"), "threadId", limit=256)
        if self.store is None:
            if method == "thread/read":
                raise RpcError(-32004, "thread not found: this Claude agent has no history yet")
            return {"data": [], "nextCursor": None, "backwardsCursor": None}
        if method == "thread/read":
            return {"thread": thread_wire(self.store, self.history, tid, include_turns=bool(params.get("includeTurns")),
                                          effort=self.effort(tid))}
        return self.history.page(tid, params, items=method == "thread/items/list")

    def respond(self, message: object) -> dict | None:
        if not isinstance(message, dict) or "method" not in message:
            return {"id": None, "error": {"code": -32600, "message": "invalid request"}}
        if "id" not in message:
            return None  # notifications (`initialized`) need no answer
        params = message.get("params")
        try:
            result = self.handle(message["method"], params if isinstance(params, dict) else {})
            return {"id": message["id"], "result": result}
        except RpcError as error:
            return {"id": message["id"], "error": error.wire()}
        except sqlite3.Error as error:
            return {"id": message["id"], "error": {"code": -32603, "message": f"history database unavailable: {error}"}}


def serve(reader: Reader, stdin=None, stdout=None) -> None:
    stdin = stdin or sys.stdin.buffer
    stdout = stdout or sys.stdout.buffer
    for raw in iter(stdin.readline, b""):
        if not raw.strip():
            continue
        try:
            message = json.loads(raw)
        except ValueError:
            response = {"id": None, "error": {"code": -32700, "message": "invalid JSON"}}
        else:
            response = reader.respond(message)
        if response is not None:
            stdout.write(dump(response).encode("utf-8") + b"\n")
            stdout.flush()


def main() -> None:
    parser = argparse.ArgumentParser(description="Read-only offline history of one Claude agent (stdio JSON-RPC)")
    parser.add_argument("--state-dir", type=Path, required=True)
    args = parser.parse_args()
    reader = Reader(args.state_dir.expanduser().resolve())
    try:
        serve(reader)
    finally:
        if reader.db:
            reader.db.close()


if __name__ == "__main__":
    main()
