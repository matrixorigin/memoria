"""Regression tests for subject and type filters on retrieve/search."""

from __future__ import annotations

import json

import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient
from tests.conftest import API_KEY, BASE_URL

EMPTY_RESULTS = {"results": []}


@pytest.mark.parametrize("method", ["retrieve", "search"])
def test_retrieve_and_search_forward_subject_and_types(httpx_mock: HTTPXMock, method: str) -> None:
    httpx_mock.add_response(json=EMPTY_RESULTS)

    with MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        getattr(client.memories, method)(
            "hello",
            subject_id="subject-1",
            memory_types=["profile", "semantic"],
        )

    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == f"/v1/memories/{method}"
    body = json.loads(request.content)
    assert body["subject_id"] == "subject-1"
    assert body["memory_types"] == ["profile", "semantic"]


def test_retrieve_and_search_preserve_default_bodies(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=EMPTY_RESULTS)
    httpx_mock.add_response(json=EMPTY_RESULTS)

    with MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        client.memories.retrieve("hello")
        client.memories.search("hello")

    retrieve_request, search_request = httpx_mock.get_requests()
    assert json.loads(retrieve_request.content) == {"query": "hello", "top_k": 5}
    assert json.loads(search_request.content) == {"query": "hello", "top_k": 10}


@pytest.mark.asyncio
@pytest.mark.parametrize("method", ["retrieve", "search"])
async def test_async_retrieve_and_search_forward_subject_and_types(
    httpx_mock: HTTPXMock, method: str
) -> None:
    httpx_mock.add_response(json=EMPTY_RESULTS)

    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        await getattr(client.memories, method)(
            "hello",
            subject_id="subject-1",
            memory_types=["profile", "semantic"],
        )

    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == f"/v1/memories/{method}"
    body = json.loads(request.content)
    assert body["subject_id"] == "subject-1"
    assert body["memory_types"] == ["profile", "semantic"]


@pytest.mark.asyncio
async def test_async_retrieve_and_search_preserve_default_bodies(
    httpx_mock: HTTPXMock,
) -> None:
    httpx_mock.add_response(json=EMPTY_RESULTS)
    httpx_mock.add_response(json=EMPTY_RESULTS)

    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        await client.memories.retrieve("hello")
        await client.memories.search("hello")

    retrieve_request, search_request = httpx_mock.get_requests()
    assert json.loads(retrieve_request.content) == {"query": "hello", "top_k": 5}
    assert json.loads(search_request.content) == {"query": "hello", "top_k": 10}
