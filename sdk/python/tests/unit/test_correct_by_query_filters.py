"""Regression tests for filters on query-based corrections."""

from __future__ import annotations

import json

import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient
from tests.conftest import API_KEY, BASE_URL, MEMORY_STUB


def test_correct_by_query_forwards_subject_and_memory_types(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)

    with MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        client.memories.correct_by_query(
            "obsolete preference",
            new_content="current preference",
            subject_id="agent-7",
            memory_types=["profile", "semantic"],
        )

    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == "/v1/memories/correct"
    body = json.loads(request.content)
    assert body["subject_id"] == "agent-7"
    assert body["memory_types"] == ["profile", "semantic"]


def test_correct_by_query_preserves_default_body(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)

    with MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        client.memories.correct_by_query("obsolete", new_content="current")

    request = httpx_mock.get_request()
    assert request is not None
    body = json.loads(request.content)
    assert "subject_id" not in body
    assert "memory_types" not in body


@pytest.mark.asyncio
async def test_async_correct_by_query_forwards_subject_and_memory_types(
    httpx_mock: HTTPXMock,
) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)

    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        await client.memories.correct_by_query(
            "obsolete preference",
            new_content="current preference",
            subject_id="agent-7",
            memory_types=["profile", "semantic"],
        )

    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == "/v1/memories/correct"
    body = json.loads(request.content)
    assert body["subject_id"] == "agent-7"
    assert body["memory_types"] == ["profile", "semantic"]


@pytest.mark.asyncio
async def test_async_correct_by_query_preserves_default_body(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)

    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        await client.memories.correct_by_query("obsolete", new_content="current")

    request = httpx_mock.get_request()
    assert request is not None
    body = json.loads(request.content)
    assert "subject_id" not in body
    assert "memory_types" not in body
