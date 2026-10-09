"""Tests use the real Hermes provider, manager, credential scope and configuration code."""

import importlib.util
import json
import os
import sqlite3
import sys
import threading
import time
from pathlib import Path

import httpx
import pytest

source = Path(os.environ.get("HERMES_SOURCE", Path.home() / ".hermes/hermes-agent"))
if not (source / "agent/memory_provider.py").exists():
    raise RuntimeError(
        "Set HERMES_SOURCE to a Hermes Agent checkout (see README development steps)"
    )
sys.path.insert(0, str(source))

from agent.secret_scope import reset_secret_scope, set_secret_scope
from hermes_constants import reset_hermes_home_override, set_hermes_home_override

root = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "memoria_hermes", root / "__init__.py", submodule_search_locations=[str(root)]
)
plugin = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = plugin
spec.loader.exec_module(plugin)
# Load the real summary classifier before timing nonblocking capture calls.
from agent.context_compressor import is_compaction_summary_message  # noqa: F401


def eventually(check, timeout=5):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            if check():
                return
        except sqlite3.OperationalError as exc:
            # Polling a rollback-journal DB can race the worker's commit lock.
            # Retry only lock contention, never hide SQL/schema errors.
            code = getattr(exc, "sqlite_errorcode", 0) & 0xFF
            if code not in {sqlite3.SQLITE_BUSY, sqlite3.SQLITE_LOCKED}:
                raise
        threading.Event().wait(0.01)
    assert check(), "condition did not become true"


class FakeAPI:
    """HTTP wire fixture implementing the Rust API's response shapes, not a mocked SDK."""

    def __init__(self):
        self.calls = []
        self.memories = {}
        self.observe_handler = None
        self.retrieve_handler = None
        self.profile_handler = None
        self.lock = threading.Lock()

    def __call__(self, request):
        body = json.loads(request.content) if request.content else {}
        path, method = request.url.path, request.method
        with self.lock:
            self.calls.append((method, path, body, dict(request.url.params)))
        if path in {"/v1/observe", "/v1/observe/deduplicated"}:
            if self.observe_handler:
                return self.observe_handler(request, body)
            return httpx.Response(200, json={"memories": []})
        if path == "/v1/memories/retrieve":
            if self.retrieve_handler:
                return self.retrieve_handler(request, body)
            return httpx.Response(
                200,
                json=[r for r in self.memories.values() if r["subject_id"] == body["subject_id"]],
            )
        if path == "/v1/memories" and method == "POST":
            row = {"memory_id": "mem_" + str(len(self.memories) + 1), **body}
            self.memories[row["memory_id"]] = row
            return httpx.Response(201, json=row)
        if path == "/v1/memories" and method == "GET":
            if self.profile_handler:
                return self.profile_handler(request)
            rows = [
                r
                for r in self.memories.values()
                if r["subject_id"] == request.url.params["subject_id"]
                and (
                    request.url.params.get("memory_type") is None
                    or r["memory_type"] == request.url.params["memory_type"]
                )
            ]
            return httpx.Response(200, json={"items": rows, "next_cursor": None})
        memory_id = path.split("/")[3]
        if method == "GET":
            return httpx.Response(200, json=self.memories.get(memory_id))
        if path.endswith("/correct"):
            row = {
                **self.memories[memory_id],
                "content": body["new_content"],
                "memory_id": "corrected_" + memory_id,
            }
            del self.memories[memory_id]
            self.memories[row["memory_id"]] = row
            return httpx.Response(200, json=row)
        if path.endswith("/feedback"):
            return httpx.Response(201, json={"feedback_id": "fb1", **body})
        if method == "DELETE":
            del self.memories[memory_id]
            return httpx.Response(204)
        raise AssertionError((method, path))


@pytest.fixture
def factory(tmp_path, monkeypatch):
    from memoria_hermes.client import Client

    server = FakeAPI()
    providers, scopes = [], []

    def make(
        *,
        home=None,
        key="test-key-not-a-real-credential",
        platform="cli",
        context="primary",
        **config,
    ):
        home = home or tmp_path / "profile"
        home.mkdir(parents=True, exist_ok=True)
        (home / "memoria.json").write_text(json.dumps(config))
        scopes.append(
            (
                set_hermes_home_override(home),
                set_secret_scope({"MEMORIA_API_KEY": key}, profile_home=str(home)),
            )
        )
        monkeypatch.setattr(
            plugin,
            "Client",
            lambda url, key, timeout: Client(
                url, key, timeout, transport=httpx.MockTransport(server)
            ),
        )
        provider = plugin.MemoriaMemoryProvider()
        provider.initialize(
            "session-1", hermes_home=str(home), platform=platform, agent_context=context
        )
        providers.append(provider)
        return provider

    yield make, server
    for provider in providers:
        provider.shutdown()
    for home_token, secret_token in reversed(scopes):
        reset_secret_scope(secret_token)
        reset_hermes_home_override(home_token)
