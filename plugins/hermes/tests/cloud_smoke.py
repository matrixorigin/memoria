"""Opt-in live Cloud acceptance. Reads a local key file, uses a disposable subject, cleans up."""

import argparse
import importlib.util
import json
import os
import sys
import tempfile
import time
import uuid
from pathlib import Path
from urllib.parse import quote


def run(key_file: Path, report_file: Path):
    source = Path(os.environ.get("HERMES_SOURCE", Path.home() / ".hermes/hermes-agent"))
    sys.path.insert(0, str(source))
    from agent.memory_manager import MemoryManager
    from agent.secret_scope import reset_secret_scope, set_secret_scope
    from hermes_constants import reset_hermes_home_override, set_hermes_home_override

    root = Path(__file__).resolve().parents[1]
    spec = importlib.util.spec_from_file_location(
        "memoria_cloud_smoke", root / "__init__.py", submodule_search_locations=[str(root)]
    )
    plugin = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = plugin
    spec.loader.exec_module(plugin)
    raw = key_file.read_text(encoding="utf-8-sig").strip()
    if raw.startswith("sk-") and len(raw.splitlines()) == 1:
        key = raw
    else:
        from dotenv import dotenv_values

        key = dotenv_values(key_file).get("MEMORIA_API_KEY", "")
    if not key:
        raise ValueError("No Memoria API Key found in the supplied file")

    report = {"endpoint": "https://api.thememoria.ai", "checks": [], "cleanup": {}}

    def check(name, condition):
        report["checks"].append({"name": name, "passed": bool(condition)})
        print(json.dumps({"check": name, "passed": bool(condition)}), flush=True)
        if not condition:
            raise AssertionError(name)

    def call(provider, name, **args):
        result = json.loads(provider.handle_tool_call("memoria_" + name, args))
        if not result.get("success"):
            # Provider errors contain only fixed codes, no response body or credentials.
            raise RuntimeError(result.get("error", "tool_failed"))
        return result["result"]

    known_ids, providers = set(), []
    manager = None
    with tempfile.TemporaryDirectory(prefix="memoria-cloud-acceptance-") as directory:
        home = Path(directory) / "profile-A"
        home.mkdir(mode=0o700)
        (home / "memoria.json").write_text(
            json.dumps(
                {
                    "auto_capture": True,
                    "request_timeout": 30.0,
                }
            )
        )
        home_token = set_hermes_home_override(home)
        secret_token = set_secret_scope({"MEMORIA_API_KEY": key}, profile_home=str(home))
        try:
            p = plugin.MemoriaMemoryProvider()
            p.initialize("cloud-smoke-session-1", hermes_home=str(home), platform="cli")
            providers.append(p)
            manager = MemoryManager()
            manager.add_provider(p)
            check("authenticated_profile_read", call(p, "profile")["items"] == [])
            marker = "hermes-smoke-" + uuid.uuid4().hex[:12]
            content = f"For the disposable {marker} test user, the preferred tea is jasmine."
            row = call(p, "store", content=content, memory_type="profile")
            known_ids.add(row["memory_id"])
            check("store_subject_bound", row.get("subject_id") == p._subject)
            manager.on_session_switch("cloud-smoke-session-2", reset=True)
            rows = call(p, "search", query=f"{marker} preferred tea")
            check("explicit_cross_session_recall", any(r["memory_id"] in known_ids for r in rows))
            started = time.monotonic()
            recalled = manager.prefetch_all(
                f"What tea does {marker} prefer?", session_id="cloud-smoke-session-2"
            )
            report["recall_seconds"] = round(time.monotonic() - started, 3)
            check("automatic_recall_default_timeout", "jasmine" in recalled.lower())
            check(
                "profile_contains_saved_preference",
                any(r["memory_id"] in known_ids for r in call(p, "profile")["items"]),
            )

            other_home = Path(directory) / "profile-B"
            other_home.mkdir(mode=0o700)
            other = plugin.MemoriaMemoryProvider()
            other_home_token = set_hermes_home_override(other_home)
            other_secret_token = set_secret_scope(
                {"MEMORIA_API_KEY": key}, profile_home=str(other_home)
            )
            try:
                other.initialize("other-session", hermes_home=str(other_home), platform="cli")
                providers.append(other)
                check("second_profile_cannot_recall", call(other, "search", query=marker) == [])
                check(
                    "second_profile_cannot_mutate",
                    not json.loads(
                        other.handle_tool_call("memoria_forget", {"memory_id": row["memory_id"]})
                    )["success"],
                )
            finally:
                reset_secret_scope(other_secret_token)
                reset_hermes_home_override(other_home_token)
            feedback = call(p, "feedback", memory_id=row["memory_id"], signal="useful")
            check("feedback", bool(feedback.get("feedback_id")))
            updated_content = f"For the disposable {marker} test user, the preferred tea is oolong."
            corrected = call(
                p,
                "update",
                memory_id=row["memory_id"],
                new_content=updated_content,
            )
            known_ids.add(corrected["memory_id"])
            # Write tools return compact receipts, not record content. Verify the
            # returned ID against the persisted record in the same branch/subject.
            corrected_record = p._read.request(
                "GET",
                "/v1/memories/" + quote(corrected["memory_id"], safe=""),
                params={"branch": p._cfg.branch},
            )
            check(
                "update",
                isinstance(corrected_record, dict)
                and corrected_record.get("memory_id") == corrected["memory_id"]
                and corrected_record.get("subject_id") == p._subject
                and corrected_record.get("content") == updated_content,
            )
            call(p, "forget", memory_id=corrected["memory_id"])
            rows = call(p, "search", query=marker)
            check(
                "forget_removes_active_memory",
                not any(r["memory_id"] == corrected["memory_id"] for r in rows),
            )

            user = f"For this disposable {marker} experiment, remember that my favorite test color is turquoise."
            assistant = "I will remember turquoise as the test preference for this experiment."
            messages = [
                {"role": "user", "content": user},
                {"role": "assistant", "content": assistant},
            ]
            started = time.monotonic()
            manager.sync_all(user, assistant, session_id="cloud-smoke-session-2", messages=messages)
            report["sync_submit_seconds"] = round(time.monotonic() - started, 3)
            deadline = time.monotonic() + 45
            counts = {}
            while time.monotonic() < deadline:
                counts = p._outbox.counts(p._binding)
                if any(counts.get(s) for s in ("done", "failed", "uncertain")):
                    break
                time.sleep(0.1)
            report["capture_states"] = counts
            with p._outbox.db() as db:
                report["capture_warnings"] = [
                    r["error"] for r in db.execute("SELECT error FROM events WHERE error != ''")
                ]
            check("automatic_capture_confirmed", counts == {"done": 1})
            rows = call(p, "search", query=f"{marker} favorite test color")
            known_ids.update(r["memory_id"] for r in rows)
            check(
                "captured_fact_recallable",
                any("turquoise" in r.get("content", "").lower() for r in rows),
            )
            p.sync_turn(user, assistant, session_id="cloud-smoke-session-2", messages=messages)
            check("duplicate_callback_suppressed", p._outbox.counts(p._binding) == {"done": 1})
        except Exception as exc:  # noqa: BLE001 — report a safe code without credentials or bodies
            report["failure"] = (
                str(exc) if isinstance(exc, (AssertionError, RuntimeError)) else type(exc).__name__
            )
            print(json.dumps({"failure": report["failure"]}), flush=True)
        finally:
            if manager:
                manager.shutdown_all()
            else:
                for provider in providers:
                    provider.shutdown()
            # List only our disposable subject; no account-wide operations.
            try:
                if providers:
                    p = providers[0]
                    cleanup = plugin.Client("https://api.thememoria.ai", key, 30)
                    try:
                        cursor = None
                        while True:
                            params = {"subject_id": p._subject, "branch": "main", "limit": 500}
                            if cursor:
                                params["cursor"] = cursor
                            page = cleanup.request("GET", "/v1/memories", params=params)
                            for row in page["items"]:
                                if row.get("subject_id") != p._subject:
                                    raise RuntimeError("cleanup_scope_mismatch")
                                known_ids.add(row["memory_id"])
                            cursor = page.get("next_cursor")
                            if not cursor:
                                break
                        removed = 0
                        for memory_id in known_ids:
                            record = cleanup.request(
                                "GET", "/v1/memories/" + memory_id, params={"branch": "main"}
                            )
                            if record is None:
                                continue
                            if record.get("subject_id") != p._subject:
                                raise RuntimeError("cleanup_scope_mismatch")
                            cleanup.request(
                                "DELETE", "/v1/memories/" + memory_id, params={"branch": "main"}
                            )
                            removed += 1
                        remaining = cleanup.request(
                            "GET",
                            "/v1/memories",
                            params={"subject_id": p._subject, "branch": "main", "limit": 1},
                        )
                        report["cleanup"] = {
                            "removed": removed,
                            "active_remaining": len(remaining["items"]),
                        }
                    finally:
                        cleanup.close()
            except Exception as exc:  # noqa: BLE001 — never print transport exception details
                report["cleanup"]["error"] = type(exc).__name__
            for provider in providers:
                provider.shutdown()
            reset_secret_scope(secret_token)
            reset_hermes_home_override(home_token)
    report["passed"] = (
        "failure" not in report
        and report["cleanup"].get("active_remaining") == 0
        and "error" not in report["cleanup"]
    )
    report_file.write_text(json.dumps(report, indent=2) + "\n")
    print(
        json.dumps(
            {
                "passed": report["passed"],
                "cleanup": report["cleanup"],
                "report_file": str(report_file),
            }
        ),
        flush=True,
    )
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--key-file", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    raise SystemExit(run(args.key_file, args.report))
