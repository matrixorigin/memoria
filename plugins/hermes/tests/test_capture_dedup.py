import json

import httpx
import pytest
from conftest import eventually, plugin
from test_provider import call


def write_messages(subject, *, tool="memoria_store", success=True, memory_id="a" * 32):
    return [
        {"role": "user", "content": "I like rainy days and drink black coffee."},
        {
            "role": "assistant",
            "tool_calls": [{"id": "save-1", "function": {"name": tool}}],
        },
        {
            "role": "tool",
            "tool_call_id": "save-1",
            "content": json.dumps(
                {
                    "success": success,
                    "result": {"memory_id": memory_id, "subject_id": subject},
                }
            ),
        },
        {"role": "assistant", "content": "Saved your rainy-day preference."},
    ]


def test_successful_store_excludes_only_saved_ids_and_preserves_whole_turn(factory):
    make, server = factory
    p = make(auto_capture=True)
    saved = call(p, "store", content="User likes rainy days", memory_type="profile")["result"]
    messages = write_messages(p._subject, memory_id=saved["memory_id"])
    p.sync_turn(messages[0]["content"], messages[-1]["content"], messages=messages)
    eventually(lambda: p._outbox.counts(p._binding) == {"done": 1})
    observes = [(path, body) for _, path, body, _ in server.calls if "/observe" in path]
    assert len(observes) == 1
    path, payload = observes[0]
    assert path == "/v1/observe/deduplicated"
    assert payload["exclude_memory_ids"] == [saved["memory_id"]]
    assert payload["messages"] == [messages[0], messages[-1]]
    assert "black coffee" in payload["messages"][0]["content"]
    assert "tool_calls" not in json.dumps(payload)
    # The capture receipt, not a mutable in-memory flag, suppresses repeated callbacks.
    p.sync_turn(messages[0]["content"], messages[-1]["content"], messages=messages)
    assert p._outbox.counts(p._binding) == {"done": 1}


@pytest.mark.parametrize("tool", ["memoria_store", "memoria_update"])
def test_current_turn_write_ids_are_correlated_and_deduplicated(tool):
    messages = write_messages("subject", tool=tool)
    messages.append(messages[-2])
    assert plugin._turn_saved_ids(messages, "subject") == ["a" * 32]
    messages.append({"role": "user", "content": "A different next turn"})
    assert plugin._turn_saved_ids(messages, "subject") == []


@pytest.mark.parametrize("tool", ["memoria_store", "memoria_update"])
def test_oversized_write_response_keeps_receipt_through_capture(factory, tool):
    make, server = factory
    p = make(auto_capture=True)
    content = '"a",' * 5000  # Valid input; JSON escaping pushes the output over 30,000.
    if tool == "memoria_store":
        args = {"content": content, "memory_type": "profile"}
    else:
        saved = call(p, "store", content="Original preference", memory_type="profile")["result"]
        args = {"memory_id": saved["memory_id"], "new_content": content}
    raw = p.handle_tool_call(tool, args)
    receipt = json.loads(raw)
    assert receipt["success"] and len(raw) < 1024
    result = receipt["result"]
    assert result["subject_id"] == p._subject
    assert result["memory_type"] == "profile"
    assert "content" not in result
    assert server.memories[result["memory_id"]]["content"] == content
    messages = write_messages(p._subject, tool=tool)
    messages[2]["content"] = raw  # Use the actual response, not a fabricated receipt.
    assert plugin._turn_saved_ids(messages, p._subject) == [result["memory_id"]]
    p.sync_turn(messages[0]["content"], "Saved", messages=messages)
    eventually(lambda: p._outbox.counts(p._binding) == {"done": 1})
    observes = [(path, body) for _, path, body, _ in server.calls if "/observe" in path]
    assert len(observes) == 1
    path, payload = observes[0]
    assert path == "/v1/observe/deduplicated"
    assert payload["exclude_memory_ids"] == [result["memory_id"]]


@pytest.mark.parametrize("case", ["failed", "other_subject", "search", "unmatched", "malformed"])
def test_capture_does_not_trust_unsuccessful_or_unrelated_tool_results(case):
    messages = write_messages("subject")
    if case == "failed":
        messages = write_messages("subject", success=False)
    elif case == "other_subject":
        messages = write_messages("different")
    elif case == "search":
        messages = write_messages("subject", tool="memoria_search")
    elif case == "unmatched":
        messages[2]["tool_call_id"] = "not-the-save-call"
    else:
        messages[2]["content"] = "invalid json"
    assert plugin._turn_saved_ids(messages, "subject") == []


@pytest.mark.parametrize("tool", ["memoria_store", "memoria_update"])
@pytest.mark.parametrize("processing", ["per_result", "aggregate", "aggregate_spills_receipt"])
def test_real_host_tool_budgets_preserve_write_exclusions(factory, tool, processing):
    from agent.memory_manager import MemoryManager
    from tools.budget_config import budget_for_context_window
    from tools.tool_result_storage import (
        PERSISTED_OUTPUT_TAG,
        enforce_turn_budget,
        maybe_persist_tool_result,
    )

    make, server = factory
    p = make(auto_capture=True)
    manager = MemoryManager(external_prefetch_timeout=1)
    manager.add_provider(p)
    budget = budget_for_context_window(8192)
    assert budget.default_result_size == 8000 and budget.turn_budget == 16000
    try:
        content = "x" * 10000  # Below the old plugin limit, above the host's floor.
        if tool == "memoria_store":
            args = {"content": content, "memory_type": "profile"}
        else:
            saved = json.loads(
                manager.handle_tool_call(
                    "memoria_store", {"content": "Original preference", "memory_type": "profile"}
                )
            )["result"]
            args = {"memory_id": saved["memory_id"], "new_content": content}
        raw = manager.handle_tool_call(tool, args)
        result = json.loads(raw)["result"]
        assert server.memories[result["memory_id"]]["content"] == content
        messages = write_messages(p._subject, tool=tool)
        messages[1]["tool_calls"][0]["function"]["arguments"] = json.dumps(args)
        messages[2]["content"] = maybe_persist_tool_result(
            raw,
            tool,
            "save-1",
            config=budget,
        )
        if processing != "per_result":
            # Ordinary pressure spills larger results first. With enough already
            # persisted previews, even a compact receipt can be forced to spill.
            others = []
            for i in range(10 if processing == "aggregate_spills_receipt" else 4):
                call_id = f"lookup-{i}"
                text = "z" * (10000 if processing == "aggregate_spills_receipt" else 7000)
                messages[1]["tool_calls"].append(
                    {
                        "id": call_id,
                        "function": {"name": "web_search", "arguments": "{}"},
                    }
                )
                others.append(
                    {
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": maybe_persist_tool_result(
                            text, "web_search", call_id, config=budget
                        ),
                    }
                )
            messages[-1:-1] = others
            enforce_turn_budget(messages[2:-1], config=budget)
            assert any(PERSISTED_OUTPUT_TAG in row["content"] for row in messages[3:-1])
            if processing == "aggregate_spills_receipt":
                assert messages[2]["content"].startswith(PERSISTED_OUTPUT_TAG)
        manager.sync_all(
            messages[0]["content"],
            messages[-1]["content"],
            session_id="host-session",
            messages=messages,
        )
        assert manager.flush_pending(timeout=5)
        eventually(lambda: p._outbox.counts(p._binding) == {"done": 1})
        observes = [(path, body) for _, path, body, _ in server.calls if "/observe" in path]
        assert len(observes) == 1
        path, payload = observes[0]
        assert (path, payload.get("exclude_memory_ids")) == (
            "/v1/observe/deduplicated",
            [result["memory_id"]],
        )
        assert len(raw) < 1500
        assert "content" not in result
    finally:
        manager.shutdown_all()


@pytest.mark.parametrize(
    "case", ["truncated", "failed", "other_subject", "length_mismatch", "trailing"]
)
def test_persisted_preview_requires_a_complete_successful_scoped_receipt(factory, case):
    from tools.tool_result_storage import maybe_persist_tool_result

    make, _ = factory
    p = make()
    messages = write_messages(p._subject)
    raw = messages[2]["content"]
    if case == "truncated":
        response = json.loads(raw)
        response["result"]["content"] = "x" * 10000
        raw = json.dumps(response)
    elif case == "failed":
        raw = write_messages(p._subject, success=False)[2]["content"]
    elif case == "other_subject":
        raw = write_messages("another-subject")[2]["content"]
    wrapped = maybe_persist_tool_result(raw, "memoria_store", "save-1", threshold=0)
    assert wrapped.startswith("<persisted-output>\n")
    if case == "length_mismatch":
        wrapped = wrapped.replace(f"first {len(raw)} chars", f"first {len(raw) + 1} chars")
    elif case == "trailing":
        wrapped += "unexpected trailing text"
    messages[2]["content"] = wrapped
    assert plugin._turn_saved_ids(messages, p._subject) == []


def test_old_server_rejection_never_falls_back_to_plain_observe(factory):
    make, server = factory
    p = make(auto_capture=True)
    server.observe_handler = lambda request, body: httpx.Response(404)
    messages = write_messages(p._subject)
    p.sync_turn(messages[0]["content"], "saved", messages=messages)
    eventually(lambda: p._outbox.counts(p._binding) == {"failed": 1})
    assert [path for _, path, _, _ in server.calls] == ["/v1/observe/deduplicated"]
    with p._outbox.db() as db:
        row = db.execute("SELECT payload, failure_kind, error FROM events").fetchone()
        assert json.loads(row["payload"])["exclude_memory_ids"] == ["a" * 32]
        assert row["failure_kind"] == "rejected"
        assert row["error"] == "capture_dedup_endpoint_unavailable"


def test_store_then_update_reports_both_real_tool_result_ids(factory):
    make, server = factory
    p = make(auto_capture=True)
    saved = call(p, "store", content="User likes rainy days", memory_type="profile")["result"]
    updated = call(p, "update", memory_id=saved["memory_id"], new_content="User likes sunny days")
    assert updated["success"]
    messages = write_messages(p._subject, memory_id=saved["memory_id"])[:-1]
    update_messages = write_messages(
        p._subject, tool="memoria_update", memory_id=updated["result"]["memory_id"]
    )[1:]
    update_messages[0]["tool_calls"][0]["id"] = "update-1"
    update_messages[1]["tool_call_id"] = "update-1"
    messages.extend(update_messages)
    p.sync_turn(messages[0]["content"], "corrected", messages=messages)
    eventually(lambda: p._outbox.counts(p._binding) == {"done": 1})
    payload = next(body for _, path, body, _ in server.calls if "/observe" in path)
    assert payload["exclude_memory_ids"] == [saved["memory_id"], updated["result"]["memory_id"]]


def test_business_not_found_does_not_report_server_upgrade(factory):
    make, server = factory
    p = make(auto_capture=True)
    server.observe_handler = lambda request, body: httpx.Response(
        404, headers={"X-Memoria-Observe-Deduplicated": "1"}, text="Branch not found"
    )
    messages = write_messages(p._subject)
    p.sync_turn(messages[0]["content"], "saved", messages=messages)
    eventually(lambda: p._outbox.counts(p._binding) == {"failed": 1})
    with p._outbox.db() as db:
        assert db.execute("SELECT error FROM events").fetchone()[0] == "not_found"


def test_extraction_failure_is_pending_and_recovers_automatically(factory):
    make, server = factory
    p = make(auto_capture=True)
    server.observe_handler = lambda request, body: httpx.Response(
        503,
        headers={
            "X-Memoria-Observe-Deduplicated": "1",
            "X-Memoria-Observe-Error": "extraction_unavailable",
        },
    )
    messages = write_messages(p._subject)
    p.sync_turn(messages[0]["content"], "saved", messages=messages)

    def retry_scheduled():
        with p._outbox.db() as db:
            row = db.execute("SELECT state, error FROM events").fetchone()
            return (
                row
                and row["state"] == "pending"
                and row["error"] == "observe_extraction_unavailable"
            )

    eventually(retry_scheduled)
    with p._outbox.db() as db:
        row = db.execute("SELECT failure_kind, next_attempt_at FROM events").fetchone()
        assert row["failure_kind"] == "rejected"
        assert row["next_attempt_at"] > 0
    server.observe_handler = None
    eventually(lambda: p._outbox.counts(p._binding) == {"done": 1})
    assert len(server.calls) == 2
    assert server.calls[0][2] == server.calls[1][2]


def test_next_turn_without_explicit_write_uses_existing_observe(factory):
    make, server = factory
    p = make(auto_capture=True)
    messages = write_messages(p._subject) + [{"role": "user", "content": "I use Rust."}]
    p.sync_turn("I use Rust.", "Okay", messages=messages)
    eventually(lambda: p._outbox.counts(p._binding) == {"done": 1})
    _, path, body, _ = server.calls[-1]
    assert path == "/v1/observe" and "exclude_memory_ids" not in body


def test_malformed_call_lists_do_not_prevent_capture():
    messages = [None, {"role": "user", "content": "hello"}, {"role": "assistant", "tool_calls": 12}]
    assert plugin._turn_saved_ids(messages, "subject") == []


def test_exclusions_survive_capture_worker_restart(factory):
    make, server = factory
    p = make(auto_capture=True)
    server.observe_handler = lambda request, body: (_ for _ in ()).throw(
        httpx.ConnectError("offline")
    )
    messages = write_messages(p._subject)
    p.sync_turn(messages[0]["content"], "saved", messages=messages)
    eventually(lambda: len(server.calls) >= 1 and p._outbox.counts(p._binding) == {"pending": 1})
    p.shutdown()
    server.observe_handler = None
    restarted = make(home=p._home, auto_capture=True)
    eventually(lambda: restarted._outbox.counts(restarted._binding) == {"done": 1})
    assert all(path == "/v1/observe/deduplicated" for _, path, _, _ in server.calls)
    assert server.calls[-1][2]["exclude_memory_ids"] == ["a" * 32]
