"""Regression tests for historical single-memory imports."""

from __future__ import annotations

import json

import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient
from tests.conftest import API_KEY, BASE_URL, MEMORY_STUB

OBSERVED_AT = "2024-02-03T04:05:06Z"


@pytest.mark.parametrize(
    ("idempotency_key", "expected_path"),
    [(None, "/v1/memories"), ("import-key", "/v1/memories/idempotent")],
)
def test_store_forwards_import_fields(
    httpx_mock: HTTPXMock, idempotency_key: str | None, expected_path: str
) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)

    with MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        client.memories.store(
            "historical fact",
            observed_at=OBSERVED_AT,
            initial_confidence=0.42,
            branch="main" if idempotency_key else None,
            idempotency_key=idempotency_key,
        )

    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == expected_path
    body = json.loads(request.content)
    assert body["observed_at"] == OBSERVED_AT
    assert body["initial_confidence"] == 0.42


def test_store_preserves_import_field_defaults(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)

    with MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        client.memories.store("current fact")

    request = httpx_mock.get_request()
    assert request is not None
    body = json.loads(request.content)
    assert "observed_at" not in body
    assert "initial_confidence" not in body


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("idempotency_key", "expected_path"),
    [(None, "/v1/memories"), ("import-key", "/v1/memories/idempotent")],
)
async def test_async_store_forwards_import_fields(
    httpx_mock: HTTPXMock, idempotency_key: str | None, expected_path: str
) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)

    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        await client.memories.store(
            "historical fact",
            observed_at=OBSERVED_AT,
            initial_confidence=0.42,
            branch="main" if idempotency_key else None,
            idempotency_key=idempotency_key,
        )

    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == expected_path
    body = json.loads(request.content)
    assert body["observed_at"] == OBSERVED_AT
    assert body["initial_confidence"] == 0.42


@pytest.mark.asyncio
async def test_async_store_preserves_import_field_defaults(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)

    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        await client.memories.store("current fact")

    request = httpx_mock.get_request()
    assert request is not None
    body = json.loads(request.content)
    assert "observed_at" not in body
    assert "initial_confidence" not in body
