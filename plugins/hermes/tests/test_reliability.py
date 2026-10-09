import json
import sqlite3
import threading
import time

import httpx
import pytest
from agent.memory_provider import spawn_context_thread
from conftest import eventually, plugin
from memoria_hermes.diagnostics import main as diagnostics
from memoria_hermes.outbox import Outbox
from test_provider import call, count_observe


def start_capture(p):
    p._worker = spawn_context_thread(p._capture_loop, name="test-capture")
    p._worker.start()


def test_eventually_retries_a_real_sqlite_read_lock(tmp_path):
    box = Outbox(tmp_path / "outbox")
    box.enqueue("binding", {"messages": []}, "history")
    locked, release, saw_busy = threading.Event(), threading.Event(), threading.Event()

    def hold_lock():
        connection = sqlite3.connect(box.path)
        try:
            connection.execute("BEGIN EXCLUSIVE")
            locked.set()
            release.wait(10)
        finally:
            connection.rollback()
            connection.close()

    holder = threading.Thread(target=hold_lock, daemon=True)
    holder.start()
    try:
        assert locked.wait(5)

        def completed():
            try:
                return box.counts("binding") == {"pending": 1}
            except sqlite3.OperationalError:
                saw_busy.set()
                release.set()
                raise

        eventually(completed)
        assert saw_busy.is_set(), "must exercise an actual locked SELECT before recovery"
    finally:
        release.set()
        holder.join(timeout=5)
    assert not holder.is_alive()


def test_eventually_does_not_hide_non_lock_sqlite_errors(tmp_path):
    box = Outbox(tmp_path / "outbox")
    with box.db() as db:
        db.execute("DROP TABLE events")
    with pytest.raises(sqlite3.OperationalError, match="no such table"):
        eventually(lambda: box.counts("binding"))


def test_real_sqlite_lock_does_not_kill_claim_worker(factory):
    make, server = factory
    p = make()
    p._outbox.enqueue(p._binding, {"messages": [], **p._scope()}, "history")
    lock = sqlite3.connect(p._outbox.path)
    try:
        lock.execute("BEGIN IMMEDIATE")
        start_capture(p)
        eventually(lambda: p._last_error == "capture_storage_busy")
        assert p._worker.is_alive()
        assert count_observe(server) == 0
    finally:
        lock.rollback()
        lock.close()
    eventually(lambda: p._outbox.counts(p._binding) == {"done": 1})
    assert p._worker.is_alive()
    assert count_observe(server) == 1


def test_real_sqlite_lock_retries_finish_without_resending(factory):
    make, server = factory
    started, release = threading.Event(), threading.Event()

    def observe(request, body):
        started.set()
        assert release.wait(3)
        return httpx.Response(200, json={"memories": []})

    server.observe_handler = observe
    p = make(auto_capture=True)
    p.sync_turn("remember", "okay")
    assert started.wait(2)
    lock = sqlite3.connect(p._outbox.path)
    try:
        lock.execute("BEGIN IMMEDIATE")
        release.set()
        eventually(lambda: p._last_error == "capture_storage_busy")
        assert p._worker.is_alive()
        assert count_observe(server) == 1
    finally:
        lock.rollback()
        lock.close()
    eventually(lambda: p._outbox.counts(p._binding) == {"done": 1})
    assert count_observe(server) == 1
    p.sync_turn("another fact", "okay")
    eventually(lambda: p._outbox.counts(p._binding) == {"done": 2})


def test_rate_limit_is_scheduled_and_recovered(factory):
    make, server = factory
    limited = True

    def observe(request, body):
        return (
            httpx.Response(429, headers={"Retry-After": "120"})
            if limited
            else httpx.Response(200, json={"memories": []})
        )

    server.observe_handler = observe
    p = make(auto_capture=True)
    p.sync_turn("remember", "okay")
    eventually(lambda: p._outbox.counts(p._binding) == {"pending": 1})

    # Wait for the rejection to be recorded, not merely the initial enqueue.
    def rejected():
        with p._outbox.db() as db:
            return db.execute("SELECT failure_kind FROM events").fetchone()[0] == "rejected"

    eventually(rejected)
    with p._outbox.db() as db:
        row = db.execute("SELECT * FROM events").fetchone()
        assert row["next_attempt_at"] > time.time() + 110
        limited = False
        db.execute("UPDATE events SET next_attempt_at=0")
    p._wake.set()
    eventually(lambda: p._outbox.counts(p._binding) == {"done": 1})
    assert count_observe(server) == 2


@pytest.mark.parametrize("state", ["failed", "uncertain"])
def test_capacity_is_per_binding_and_counts_only_automatic_work(tmp_path, state):
    box = Outbox(tmp_path / "state", capacity=1)
    box.enqueue("old", {"value": 1}, "h")
    assert box.enqueue("current", {"value": 1}, "h")
    row = box.claim("current")
    assert box.finish(row["id"], state, claim_token=row["claim_token"])
    assert box.enqueue("current", {"value": 2}, "h")
    assert box.other_binding_counts("current") == {"old": 1}
    with pytest.raises(ValueError, match="capture_queue_full"):
        box.enqueue("current", {"value": 3}, "h")


def test_old_binding_is_reported_on_startup(factory):
    make, _ = factory
    old = make()
    old._outbox.enqueue(old._binding, {"messages": [], **old._scope()}, "history")
    old.shutdown()
    new = make(home=old._home, key="new-key")
    assert new._last_error == "other_bindings_need_attention"
    assert new._outbox.enqueue(new._binding, {"messages": [], **new._scope()}, "history")


@pytest.mark.parametrize("action", ["discard", "retry"])
def test_late_finish_cannot_overwrite_manual_action_or_new_claim(tmp_path, action, capsys):
    box = Outbox(tmp_path / "plugin-data/memoria")
    box.enqueue("binding", {"messages": []}, "history")
    old = box.claim("binding")
    with box.db() as db:
        db.execute("UPDATE events SET updated=?", (time.time() - 121,))
    assert box.claim("binding") is None
    args = ["--home", str(tmp_path), "--" + action, old["id"]]
    if action == "retry":
        args.append("--acknowledge-duplicate-risk")
    diagnostics(args)
    capsys.readouterr()
    new = box.claim("binding") if action == "retry" else None
    assert not box.finish(old["id"], "done", claim_token=old["claim_token"])
    if new:
        assert new["claim_token"] != old["claim_token"]
        assert box.counts("binding") == {"inflight": 1}
        assert box.finish(new["id"], "done", claim_token=new["claim_token"])
    else:
        assert box.counts("binding") == {"discarded": 1}


def test_safe_manual_retry_needs_no_duplicate_ack(tmp_path, capsys):
    box = Outbox(tmp_path / "plugin-data/memoria")
    box.enqueue("binding", {"messages": []}, "history")
    row = box.claim("binding")
    box.finish(
        row["id"],
        "pending",
        "connection_failed",
        claim_token=row["claim_token"],
        failure_kind="not_sent",
        delay=300,
    )
    diagnostics(["--home", str(tmp_path), "--retry", row["id"]])
    capsys.readouterr()
    assert box.claim("binding") is not None


@pytest.mark.parametrize("mode", ["warm", "cold"])
@pytest.mark.parametrize("mutation", ["forget", "update", "capture"])
def test_late_search_cannot_repopulate_invalidated_cache(factory, mode, mutation):
    make, server = factory
    p = make(auto_capture=mutation == "capture")
    saved = call(p, "store", content="old fact")["result"]
    started, release = threading.Event(), threading.Event()

    def retrieve(request, body):
        snapshot = [dict(saved)]
        started.set()
        assert release.wait(3)
        return httpx.Response(200, json=snapshot)

    server.retrieve_handler = retrieve
    results = []
    if mode == "warm":
        p.queue_prefetch("query")
        worker = p._warm_threads[-1]
    else:
        worker = spawn_context_thread(lambda: results.append(p.prefetch("query")), name="cold")
        worker.start()
    assert started.wait(2)
    if mutation == "capture":
        p.sync_turn("new fact", "okay")
        eventually(lambda: p._outbox.counts(p._binding) == {"done": 1})
    elif mutation == "forget":
        assert call(p, "forget", memory_id=saved["memory_id"])["success"]
    else:
        assert call(p, "update", memory_id=saved["memory_id"], new_content="new fact")["success"]
    release.set()
    worker.join(2)
    assert not worker.is_alive()
    assert p._cache_key("query", "") not in p._cache
    if mode == "cold":
        assert results == [""]


def test_notice_repeats_after_request_recovers(factory):
    make, server = factory
    p = make()
    notices = []
    p._warning_callback = notices.append
    server.retrieve_handler = lambda r, b: httpx.Response(503)
    assert p.prefetch("offline question") == ""
    assert p.prefetch("another offline question") == ""
    assert len(notices) == 1
    server.retrieve_handler = lambda r, b: httpx.Response(200, json=[])
    p.prefetch("online question")
    server.retrieve_handler = lambda r, b: httpx.Response(503)
    p.prefetch("offline again")
    assert len(notices) == 2


def test_bad_history_items_do_not_prevent_capture(factory):
    make, _ = factory
    p = make(auto_capture=True)
    p.sync_turn(
        "new fact", "okay", messages=[None, 3, "bad", {"role": "user", "content": "new fact"}]
    )
    eventually(lambda: p._outbox.counts(p._binding) == {"done": 1})


@pytest.mark.parametrize("content", ["x" * 32000, '"\\\n' * 10000], ids=["ascii", "escaped"])
def test_search_preserves_oversized_first_hit_and_later_match(factory, content):
    make, server = factory
    p = make()
    rows = [
        {
            "memory_id": f"{i:032x}",
            "memory_type": "semantic",
            "subject_id": p._subject,
            "content": text,
        }
        for i, text in enumerate([content, "A short useful match"], 1)
    ]
    server.retrieve_handler = lambda r, b: httpx.Response(200, json=rows)
    raw = p.handle_tool_call("memoria_search", {"query": "matching fact"})
    response = json.loads(raw)
    assert response["success"] and response["truncated"]
    assert len(raw) <= 30000
    assert [row["memory_id"] for row in response["result"]] == [row["memory_id"] for row in rows]
    first, second = response["result"]
    assert first["memory_type"] == "semantic"
    assert first["content"] and content.startswith(first["content"])
    assert first["content_truncated"]
    assert second == rows[1]


@pytest.mark.parametrize("large_field", ["content", "extra_metadata"])
def test_search_budget_keeps_all_hit_ids(factory, large_field):
    make, server = factory
    p = make()
    rows = [
        {
            "memory_id": f"{i:032x}",
            "memory_type": "semantic",
            "subject_id": p._subject,
            "content": "x" * 32000 if large_field == "content" else "A short fact",
            "extra_metadata": {"large": "x" * 32000} if large_field == "extra_metadata" else {},
        }
        for i in range(20)
    ]
    server.retrieve_handler = lambda r, b: httpx.Response(200, json=rows)
    raw = p.handle_tool_call("memoria_search", {"query": "matching fact", "top_k": 20})
    response = json.loads(raw)
    assert response["success"] and response["truncated"]
    assert len(raw) <= 30000
    assert [row["memory_id"] for row in response["result"]] == [row["memory_id"] for row in rows]
    assert all(row["truncated"] for row in response["result"])
    if large_field == "extra_metadata":
        assert all(row["content"] == "A short fact" for row in response["result"])
        assert not any(row["content_truncated"] for row in response["result"])
    else:
        assert all(row["content"] and row["content_truncated"] for row in response["result"])


def test_search_within_budget_preserves_full_rows(factory):
    make, server = factory
    p = make()
    rows = [{"memory_id": "a" * 32, "subject_id": p._subject, "content": "完整记录"}]
    server.retrieve_handler = lambda r, b: httpx.Response(200, json=rows)
    response = call(p, "search", query="matching fact")
    assert response == {"success": True, "result": rows}


def test_profile_pages_after_output_budget_truncation(factory):
    make, server = factory
    p = make()
    rows = [
        {
            "memory_id": f"{i:032x}",
            "memory_type": "profile",
            "subject_id": p._subject,
            "content": "x" * 16000,
        }
        for i in range(1, 3)
    ]
    server.profile_handler = lambda r: httpx.Response(
        200, json={"items": rows[1:] if r.url.params.get("cursor") else rows, "next_cursor": None}
    )
    first = call(p, "profile")
    assert len(json.dumps(first)) < 30000
    assert len(first["result"]["items"]) == 1
    assert first["result"]["next_cursor"] == rows[0]["memory_id"]
    second = call(p, "profile", cursor=first["result"]["next_cursor"], limit=1)
    assert second["result"]["items"][0]["memory_id"] == rows[1]["memory_id"]


def test_profile_preserves_oversized_first_record_identity(factory):
    make, server = factory
    p = make()
    server.profile_handler = lambda r: httpx.Response(
        200,
        json={
            "items": [{"memory_id": "a" * 32, "subject_id": p._subject, "content": "x" * 40000}],
            "next_cursor": None,
        },
    )
    response = call(p, "profile")
    assert response["success"]
    assert response["result"]["items"][0]["content_truncated"]
    assert len(json.dumps(response)) < 30000


def test_initialize_closes_first_client_if_second_client_fails(factory, monkeypatch):
    make, _ = factory
    existing = make()
    original = plugin.Client
    created = []

    def client(*args):
        if created:
            raise RuntimeError("construction failed")
        created.append(original(*args))
        return created[0]

    monkeypatch.setattr(plugin, "Client", client)
    p = plugin.MemoriaMemoryProvider()
    with pytest.raises(RuntimeError, match="construction failed"):
        p.initialize("s", hermes_home=str(existing._home), platform="cli")
    assert not p._ready
    assert created[0].http.is_closed


def test_profile_context_mismatch_is_rejected(factory, tmp_path):
    make, _ = factory
    make()
    with pytest.raises(ValueError, match="must_match_active_profile"):
        plugin.MemoriaMemoryProvider().initialize("s", hermes_home=str(tmp_path / "other"))


def test_corrupt_config_is_backed_up_and_repaired_by_setup(factory):
    from memoria_hermes.config import Config

    make, _ = factory
    p = make()
    raw = "{broken JSON"
    (p._home / "memoria.json").write_text(raw)
    p.save_config({"auto_capture": "false"}, str(p._home))
    assert Config.load(p._home).auto_capture is False
    backup = list(p._home.glob("memoria.json.invalid-*"))
    assert len(backup) == 1 and backup[0].read_text() == raw
    assert backup[0].stat().st_mode & 0o777 == 0o600


def test_content_utf8_limit_is_checked_before_network_call(factory):
    make, server = factory
    p = make()
    result = call(p, "store", content="中" * 12000)
    assert result["error"] == "content_exceeds_32_KiB"
    assert not server.calls


def test_profile_cursor_is_validated_before_network_call(factory):
    make, server = factory
    p = make()
    assert not call(p, "profile", cursor="invalid-cursor")["success"]
    assert not server.calls


def test_default_recall_io_timeout_is_below_pinned_host_wait(factory):
    from agent.memory_manager import MemoryManager

    make, _ = factory
    p = make()
    assert p._cfg.recall_timeout == 2.0
    assert p._cfg.recall_timeout < MemoryManager()._external_prefetch_timeout == 8.0


def test_schema_upgrade_only_revives_definitely_unsent_legacy_failures(tmp_path):
    directory = tmp_path / "state"
    directory.mkdir()
    with sqlite3.connect(directory / "outbox.sqlite3") as db:
        db.execute(
            "CREATE TABLE events (id TEXT PRIMARY KEY, binding TEXT, payload TEXT, "
            "state TEXT, attempts INTEGER, updated REAL, created REAL, error TEXT)"
        )
        for i, error in enumerate(["connection_failed", "rate_limited", "http_503"]):
            db.execute(
                "INSERT INTO events VALUES(?, 'b', '{}', 'failed', 3, 0, 0, ?)", (str(i), error)
            )
    box = Outbox(directory)
    assert box.counts("b") == {"pending": 2, "failed": 1}
    first = box.claim("b")
    assert first["failure_kind"] == "not_sent"
    assert first["claim_token"]
