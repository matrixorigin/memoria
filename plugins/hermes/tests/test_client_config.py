import json
import threading

import httpx
import pytest
from memoria_hermes.client import APIError, Client
from memoria_hermes.config import Config


@pytest.mark.parametrize(
    "status,code",
    [(401, "authentication_failed"), (403, "permission_denied"), (429, "rate_limited")],
)
def test_errors_do_not_leak_response_bodies_or_keys(status, code):
    client = Client(
        "https://example.test",
        "very-private-key",
        1,
        transport=httpx.MockTransport(lambda r: httpx.Response(status, text="private")),
    )
    with pytest.raises(APIError) as caught:
        client.request("POST", "/v1/memories", json={"content": "private fact"})
    assert str(caught.value) == code
    assert not caught.value.uncertain
    client.close()


def test_close_waits_for_inflight_request_without_blocking():
    started, release = threading.Event(), threading.Event()

    def handle(request):
        started.set()
        assert release.wait(3)
        return httpx.Response(200, json=[])

    client = Client("https://example.test", "key", 1, transport=httpx.MockTransport(handle))
    worker = threading.Thread(target=lambda: client.request("GET", "/v1/memories"))
    worker.start()
    assert started.wait(2)
    client.close()
    assert not client.http.is_closed
    release.set()
    worker.join(2)
    assert client.http.is_closed


@pytest.mark.parametrize(
    "path,status,headers,code,retryable,uncertain",
    [
        ("/v1/observe/deduplicated", 404, {}, "capture_dedup_endpoint_unavailable", False, False),
        (
            "/v1/observe/deduplicated",
            404,
            {"X-Memoria-Observe-Deduplicated": "1"},
            "not_found",
            False,
            False,
        ),
        (
            "/v1/observe/deduplicated",
            503,
            {
                "X-Memoria-Observe-Deduplicated": "1",
                "X-Memoria-Observe-Error": "extraction_unavailable",
            },
            "observe_extraction_unavailable",
            True,
            False,
        ),
        ("/v1/observe/deduplicated", 503, {}, "http_503", False, True),
        (
            "/v1/observe/deduplicated",
            503,
            {"X-Memoria-Observe-Error": "extraction_unavailable"},
            "http_503",
            False,
            True,
        ),
        (
            "/v1/observe/deduplicated",
            503,
            {"X-Memoria-Observe-Deduplicated": "1"},
            "http_503",
            False,
            True,
        ),
        (
            "/v1/observe/deduplicated",
            500,
            {
                "X-Memoria-Observe-Deduplicated": "1",
                "X-Memoria-Observe-Error": "extraction_unavailable",
            },
            "http_500",
            False,
            True,
        ),
        (
            "/v1/observe",
            503,
            {
                "X-Memoria-Observe-Deduplicated": "1",
                "X-Memoria-Observe-Error": "extraction_unavailable",
            },
            "http_503",
            False,
            True,
        ),
    ],
)
def test_deduplicated_error_markers_are_route_and_status_specific(
    path, status, headers, code, retryable, uncertain
):
    client = Client(
        "https://example.test",
        "key",
        1,
        transport=httpx.MockTransport(lambda r: httpx.Response(status, headers=headers)),
    )
    try:
        with pytest.raises(APIError) as caught:
            client.request("POST", path, json={})
        assert caught.value.code == code
        assert caught.value.retryable is retryable
        assert caught.value.uncertain is uncertain
    finally:
        client.close()


@pytest.mark.parametrize(
    "values",
    [
        {"api_url": "https://secret:password@example.test"},
        {"api_url": "http://remote.test"},
        {"api_url": "https://example.test/v1"},
        {"auto_capture": "false"},
        {"top_k": True},
        {"request_timeout": 0},
        {"api_key": "must not be saved here"},
        {"typo": True},
    ],
)
def test_invalid_configuration_fails_closed(tmp_path, values):
    (tmp_path / "memoria.json").write_text(json.dumps(values))
    with pytest.raises(ValueError):
        Config.load(tmp_path)


def test_cloud_default_and_local_self_host(tmp_path):
    cfg = Config.load(tmp_path)
    assert cfg.api_url == "https://api.thememoria.ai"
    assert cfg.auto_capture is False
    (tmp_path / "memoria.json").write_text('{"api_url": "http://localhost:8100"}')
    assert Config.load(tmp_path).api_url == "http://localhost:8100"


def test_cli_string_boolean_is_normalized_before_atomic_save(factory):
    make, _ = factory
    provider = make()
    provider.save_config({"auto_capture": "true", "api_key": "do-not-persist"}, str(provider._home))
    assert Config.load(provider._home).auto_capture is True
    provider.save_config({"auto_capture": "false"}, str(provider._home))
    assert Config.load(provider._home).auto_capture is False
    with pytest.raises(ValueError, match="must be true or false"):
        provider.save_config({"auto_capture": "invalid"}, str(provider._home))
    assert Config.load(provider._home).auto_capture is False
    assert "do-not-persist" not in (provider._home / "memoria.json").read_text()
