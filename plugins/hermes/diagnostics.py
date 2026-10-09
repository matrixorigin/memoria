"""Local outbox inspection/recovery. Does not load Hermes or print captured conversation text."""

import argparse
import json
import sqlite3
from pathlib import Path


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--home", type=Path, required=True, help="Active profile's HERMES_HOME")
    actions = parser.add_mutually_exclusive_group()
    actions.add_argument("--retry", metavar="EVENT_ID", help="Requeue one failed/uncertain event")
    actions.add_argument("--discard", metavar="EVENT_ID", help="Discard an unsent/failed event")
    parser.add_argument("--acknowledge-duplicate-risk", action="store_true")
    args = parser.parse_args(argv)
    path = args.home.resolve() / "plugin-data" / "memoria" / "outbox.sqlite3"
    if not path.exists():
        print(json.dumps({"status": "not_initialized", "queue": {}}))
        return
    mode = "rw" if args.retry or args.discard else "ro"
    db = sqlite3.connect(path.as_uri() + "?mode=" + mode, uri=True, timeout=1)
    db.row_factory = sqlite3.Row
    try:
        columns = {r["name"] for r in db.execute("PRAGMA table_info(events)")}
        with db:
            if args.retry or args.discard:
                db.execute("BEGIN IMMEDIATE")
                event = args.retry or args.discard
                allowed = ("pending", "failed", "uncertain")
                row = db.execute("SELECT * FROM events WHERE id=?", (event,)).fetchone()
                if row is None or row["state"] not in allowed:
                    parser.error("Event not found or state does not allow this operation")
                kind = (
                    row["failure_kind"]
                    if "failure_kind" in columns
                    else {"connection_failed": "not_sent", "rate_limited": "rejected"}.get(
                        row["error"], ""
                    )
                )
                safe = row["state"] != "uncertain" and kind in {"not_sent", "rejected"}
                if args.retry and not safe and not args.acknowledge_duplicate_risk:
                    parser.error(
                        "Retry can duplicate a remotely committed write. Inspect Cloud "
                        "first, then pass --acknowledge-duplicate-risk."
                    )
                extra = ", claim_token='', next_attempt_at=0" if "claim_token" in columns else ""
                if args.retry:
                    db.execute(
                        "UPDATE events SET state='pending', attempts=0, error='manual_retry' "
                        + extra
                        + " WHERE id=?",
                        (event,),
                    )
                else:
                    db.execute(
                        "UPDATE events SET state='discarded', payload='', error='manual_discard' "
                        + extra
                        + " WHERE id=?",
                        (event,),
                    )
            queue = [
                dict(r)
                for r in db.execute(
                    "SELECT binding, state, COUNT(*) count FROM events GROUP BY binding, state"
                )
            ]
            attention = [
                dict(r)
                for r in db.execute(
                    "SELECT id, binding, state, attempts, error, created"
                    + (", next_attempt_at, failure_kind" if "next_attempt_at" in columns else "")
                    + " FROM events "
                    "WHERE state NOT IN ('done', 'discarded') ORDER BY created LIMIT 100"
                )
            ]
        print(json.dumps({"queue": queue, "needs_attention": attention}, indent=2))
    finally:
        db.close()


if __name__ == "__main__":
    main()
