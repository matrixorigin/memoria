"""Regression tests for subject scoping on memory list requests."""

from __future__ import annotations

import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient
from tests.conftest import API_KEY, BASE_URL

EMPTY_PAGE = {"items": [], "next_cursor": None}


def test_list_forwards_subject_id(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=EMPTY_PAGE)

    with MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        client.memories.list(subject_id="subject/one")

    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == "/v1/memories"
    assert request.url.params["subject_id"] == "subject/one"


def test_list_preserves_default_params(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=EMPTY_PAGE)

    with MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        client.memories.list()

    request = httpx_mock.get_request()
    assert request is not None
    assert dict(request.url.params) == {"limit": "100"}


@pytest.mark.asyncio
async def test_async_list_forwards_subject_id(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=EMPTY_PAGE)

    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        await client.memories.list(subject_id="subject/one")

    request = httpx_mock.get_request()
    assert request is not None
    assert request.url.path == "/v1/memories"
    assert request.url.params["subject_id"] == "subject/one"


@pytest.mark.asyncio
async def test_async_list_preserves_default_params(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=EMPTY_PAGE)

    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        await client.memories.list()

    request = httpx_mock.get_request()
    assert request is not None
    assert dict(request.url.params) == {"limit": "100"}
