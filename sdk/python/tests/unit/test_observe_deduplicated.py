"""Regression tests for the deduplicated observe endpoint."""

from __future__ import annotations

import json

import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient
from tests.conftest import API_KEY, BASE_URL

MESSAGES = [{"role": "user", "content": "Remember this preference"}]
OBSERVE_RESPONSE = {"memories": [{"memory_id": "mem_1"}]}


def test_observe_deduplicated_forwards_exclusions(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=OBSERVE_RESPONSE)

    with MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        result = client.observe_deduplicated(
            MESSAGES,
            exclude_memory_ids=["mem_explicit"],
            session_id="session-1",
            subject_id="subject-1",
            branch="feature-1",
        )

    assert result.memories == OBSERVE_RESPONSE["memories"]
    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == "/v1/observe/deduplicated"
    assert json.loads(request.content) == {
        "messages": MESSAGES,
        "exclude_memory_ids": ["mem_explicit"],
        "session_id": "session-1",
        "subject_id": "subject-1",
        "branch": "feature-1",
    }


def test_observe_default_body_is_unchanged(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=OBSERVE_RESPONSE)

    with MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        client.observe(MESSAGES)

    request = httpx_mock.get_request()
    assert request is not None
    assert json.loads(request.content) == {"messages": MESSAGES}


@pytest.mark.asyncio
async def test_async_observe_deduplicated_forwards_exclusions(
    httpx_mock: HTTPXMock,
) -> None:
    httpx_mock.add_response(json=OBSERVE_RESPONSE)

    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        result = await client.observe_deduplicated(
            MESSAGES,
            exclude_memory_ids=["mem_explicit"],
            session_id="session-1",
            subject_id="subject-1",
            branch="feature-1",
        )

    assert result.memories == OBSERVE_RESPONSE["memories"]
    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == "/v1/observe/deduplicated"
    assert json.loads(request.content) == {
        "messages": MESSAGES,
        "exclude_memory_ids": ["mem_explicit"],
        "session_id": "session-1",
        "subject_id": "subject-1",
        "branch": "feature-1",
    }


@pytest.mark.asyncio
async def test_async_observe_default_body_is_unchanged(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=OBSERVE_RESPONSE)

    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        await client.observe(MESSAGES)

    request = httpx_mock.get_request()
    assert request is not None
    assert json.loads(request.content) == {"messages": MESSAGES}
