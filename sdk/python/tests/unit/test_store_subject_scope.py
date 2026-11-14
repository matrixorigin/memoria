"""Regression tests for subject scoping on single-memory stores."""

from __future__ import annotations

import json

import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient
from tests.conftest import API_KEY, BASE_URL, MEMORY_STUB


@pytest.mark.parametrize(
    ("idempotency_key", "expected_path"),
    [(None, "/v1/memories"), ("store-key", "/v1/memories/idempotent")],
)
def test_store_forwards_subject_id(
    httpx_mock: HTTPXMock, idempotency_key: str | None, expected_path: str
) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)

    with MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        client.memories.store(
            "subject preference",
            subject_id="subject-1",
            branch="main" if idempotency_key else None,
            idempotency_key=idempotency_key,
        )

    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == expected_path
    assert json.loads(request.content)["subject_id"] == "subject-1"


def test_store_preserves_default_body(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)

    with MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        client.memories.store("subject preference")

    request = httpx_mock.get_request()
    assert request is not None
    assert "subject_id" not in json.loads(request.content)


@pytest.mark.asyncio
@pytest.mark.parametrize(
    ("idempotency_key", "expected_path"),
    [(None, "/v1/memories"), ("store-key", "/v1/memories/idempotent")],
)
async def test_async_store_forwards_subject_id(
    httpx_mock: HTTPXMock, idempotency_key: str | None, expected_path: str
) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)

    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        await client.memories.store(
            "subject preference",
            subject_id="subject-1",
            branch="main" if idempotency_key else None,
            idempotency_key=idempotency_key,
        )

    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == expected_path
    assert json.loads(request.content)["subject_id"] == "subject-1"


@pytest.mark.asyncio
async def test_async_store_preserves_default_body(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)

    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        await client.memories.store("subject preference")

    request = httpx_mock.get_request()
    assert request is not None
    assert "subject_id" not in json.loads(request.content)
