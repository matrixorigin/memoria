"""SQLite outbox with atomic claims, durable receipts and conservative crash recovery."""

import hashlib
import json
import sqlite3
import time
import uuid
from contextlib import contextmanager
from pathlib import Path


def digest(value) -> str:
    return hashlib.sha256(
        json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()
    ).hexdigest()


class Outbox:
    def __init__(self, directory: Path, capacity: int = 1000):
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        directory.chmod(0o700)
        self.path = directory / "outbox.sqlite3"
        self.capacity = capacity
        # Reserve restrictive permissions before SQLite opens the file.
        self.path.touch(mode=0o600, exist_ok=True)
        self.path.chmod(0o600)
        with self.db() as db:
            db.execute("BEGIN IMMEDIATE")
            db.execute("""CREATE TABLE IF NOT EXISTS events (
                id TEXT PRIMARY KEY, binding TEXT NOT NULL, payload TEXT NOT NULL,
                state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
                updated REAL NOT NULL, created REAL NOT NULL, error TEXT NOT NULL DEFAULT ''
            )""")
            columns = {row["name"] for row in db.execute("PRAGMA table_info(events)")}
            for name, declaration in {
                "next_attempt_at": "REAL NOT NULL DEFAULT 0",
                "failure_kind": "TEXT NOT NULL DEFAULT ''",
                "claim_token": "TEXT NOT NULL DEFAULT ''",
            }.items():
                if name not in columns:
                    db.execute(f"ALTER TABLE events ADD COLUMN {name} {declaration}")
            if "failure_kind" not in columns:
                # Upgrade the old three-attempt policy without replaying ambiguous writes.
                db.execute(
                    "UPDATE events SET state='pending', failure_kind='not_sent' "
                    "WHERE state IN ('failed','pending') AND error='connection_failed'"
                )
                db.execute(
                    "UPDATE events SET state='pending', failure_kind='rejected' "
                    "WHERE state IN ('failed','pending') AND error='rate_limited'"
                )
            db.execute("CREATE INDEX IF NOT EXISTS queue ON events(binding, state, created)")

    @contextmanager
    def db(self):
        # Short transactions; no connection crosses threads or survives shutdown.
        db = sqlite3.connect(self.path, timeout=0.5)
        db.row_factory = sqlite3.Row
        try:
            with db:
                yield db
        finally:
            db.close()

    def enqueue(self, binding: str, payload: dict, history_hash: str) -> bool:
        event = digest([binding, payload, history_hash])
        with self.db() as db:
            db.execute("BEGIN IMMEDIATE")
            if db.execute("SELECT 1 FROM events WHERE id=?", (event,)).fetchone():
                return False
            count = db.execute(
                "SELECT COUNT(*) FROM events WHERE binding=? AND state IN ('pending','inflight')",
                (binding,),
            ).fetchone()[0]
            if count >= self.capacity:
                raise ValueError("capture_queue_full")
            now = time.time()
            db.execute(
                "INSERT INTO events(id,binding,payload,state,updated,created,failure_kind) "
                "VALUES(?,?,?,'pending',?,?,'not_sent')",
                (event, binding, json.dumps(payload, ensure_ascii=False), now, now),
            )
        return True

    def claim(self, binding: str):
        with self.db() as db:
            db.execute("BEGIN IMMEDIATE")
            # A dead process may have committed remotely. Never replay its inflight request.
            db.execute(
                "UPDATE events SET state='uncertain', error='interrupted_request', "
                "failure_kind='unknown', claim_token='' "
                "WHERE binding=? AND state='inflight' AND updated < ?",
                (binding, time.time() - 120),
            )
            row = db.execute(
                "SELECT * FROM events WHERE binding=? AND state='pending' AND next_attempt_at <= ? "
                "ORDER BY created, rowid LIMIT 1",
                (binding, time.time()),
            ).fetchone()
            if row is None:
                return None
            token = uuid.uuid4().hex
            db.execute(
                "UPDATE events SET state='inflight', attempts=attempts+1, updated=?, "
                "claim_token=? WHERE id=?",
                (time.time(), token, row["id"]),
            )
            return {**dict(row), "claim_token": token, "attempts": row["attempts"] + 1}

    def finish(
        self,
        event: str,
        state: str,
        error: str = "",
        *,
        claim_token: str,
        delay: float = 0,
        failure_kind: str = "",
    ) -> bool:
        with self.db() as db:
            changed = db.execute(
                "UPDATE events SET state=?, error=?, updated=?, next_attempt_at=?, "
                "failure_kind=?, claim_token='', "
                "payload=CASE WHEN ?='done' THEN '' ELSE payload END "
                "WHERE id=? AND state='inflight' AND claim_token=?",
                (
                    state,
                    error,
                    time.time(),
                    time.time() + delay,
                    failure_kind,
                    state,
                    event,
                    claim_token,
                ),
            )
            return changed.rowcount == 1

    def other_binding_counts(self, binding: str):
        with self.db() as db:
            return {
                r["binding"]: r["n"]
                for r in db.execute(
                    "SELECT binding, COUNT(*) n FROM events WHERE binding != ? "
                    "AND state NOT IN ('done','discarded') GROUP BY binding",
                    (binding,),
                )
            }

    def counts(self, binding: str):
        with self.db() as db:
            return {
                r["state"]: r["n"]
                for r in db.execute(
                    "SELECT state, COUNT(*) n FROM events WHERE binding=? GROUP BY state",
                    (binding,),
                )
            }
