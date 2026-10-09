"""Run the documented acceptance caller offline against the HTTP wire fixture."""

import json

import httpx
import pytest
from cloud_smoke import run


@pytest.mark.parametrize("readback", ["valid", "wrong_subject", "wrong_id", "missing"])
def test_cloud_smoke_verifies_receipt_only_updates_by_reading_the_saved_id(
    factory, monkeypatch, tmp_path, readback
):
    _, server = factory
    real_client = httpx.Client

    def transport(request):
        response = server(request)
        if request.method == "GET" and request.url.path.startswith("/v1/memories/corrected_"):
            record = json.loads(response.content) if response.content else None
            if record and readback == "wrong_subject":
                return httpx.Response(200, json={**record, "subject_id": "another-subject"})
            if record and readback == "wrong_id":
                return httpx.Response(200, json={**record, "memory_id": "different-id"})
            if readback == "missing":
                return httpx.Response(200, content="null")
        return response

    def offline_client(*args, **kwargs):
        # Covers both provider clients and the script's independent cleanup client.
        kwargs["transport"] = httpx.MockTransport(transport)
        return real_client(*args, **kwargs)

    monkeypatch.setattr(httpx, "Client", offline_client)

    def capture(request, body):
        # Simulate observe persistence; this tests the caller, not LLM quality.
        assert "turquoise" in json.dumps(body["messages"])
        row = {
            "memory_id": "captured-test-fact",
            "subject_id": body["subject_id"],
            "memory_type": "profile",
            "content": "The favorite test color is turquoise.",
        }
        server.memories[row["memory_id"]] = row
        return httpx.Response(200, json={"memories": [row]})

    server.observe_handler = capture
    key_file = tmp_path / "fake-key"
    key_file.write_text("sk-offline-test-not-a-real-key")
    report_file = tmp_path / "report.json"
    exit_code = run(key_file, report_file)
    report = json.loads(report_file.read_text())
    checks = {check["name"]: check["passed"] for check in report["checks"]}
    if readback != "valid":
        assert exit_code == 1 and report["failure"] == "update"
        assert checks["update"] is False
        return
    assert exit_code == 0 and report["passed"]
    assert len(checks) == 13 and all(checks.values())
    assert checks["forget_removes_active_memory"] and checks["automatic_capture_confirmed"]
    assert report["cleanup"]["active_remaining"] == 0
    assert not server.memories
    reads = [
        (path, params)
        for method, path, _, params in server.calls
        if method == "GET" and path.startswith("/v1/memories/corrected_")
    ]
    assert reads and all(params == {"branch": "main"} for _, params in reads)
