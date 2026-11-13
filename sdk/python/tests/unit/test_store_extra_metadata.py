"""Regression tests for metadata on single-memory stores."""

from __future__ import annotations

import json

import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient
from tests.conftest import API_KEY, BASE_URL, MEMORY_STUB

EXTRA_METADATA = {"scene": "incident", "attempt": 2, "verified": True}


@pytest.mark.parametrize(
    ("idempotency_key", "expected_path"),
    [(None, "/v1/memories"), ("store-key", "/v1/memories/idempotent")],
)
def test_store_forwards_extra_metadata(
    httpx_mock: HTTPXMock, idempotency_key: str | None, expected_path: str
) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)

    with MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        client.memories.store(
            "incident resolution",
            extra_metadata=EXTRA_METADATA,
            branch="main" if idempotency_key else None,
            idempotency_key=idempotency_key,
        )

    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == expected_path
    assert json.loads(request.content)["extra_metadata"] == EXTRA_METADATA


def test_store_preserves_default_body(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)

    with MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        client.memories.store("incident resolution")

    request = httpx_mock.get_request()
    assert request is not None
    assert "extra_metadata" not in json.loads(request.content)


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("idempotency_key", "expected_path"),
    [(None, "/v1/memories"), ("store-key", "/v1/memories/idempotent")],
)
async def test_async_store_forwards_extra_metadata(
    httpx_mock: HTTPXMock, idempotency_key: str | None, expected_path: str
) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)

    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        await client.memories.store(
            "incident resolution",
            extra_metadata=EXTRA_METADATA,
            branch="main" if idempotency_key else None,
            idempotency_key=idempotency_key,
        )

    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == expected_path
    assert json.loads(request.content)["extra_metadata"] == EXTRA_METADATA


@pytest.mark.asyncio
async def test_async_store_preserves_default_body(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)

    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        await client.memories.store("incident resolution")

    request = httpx_mock.get_request()
    assert request is not None
    assert "extra_metadata" not in json.loads(request.content)
