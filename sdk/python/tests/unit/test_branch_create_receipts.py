"""Branch creation must not report success for malformed HTTP 201 bodies."""

from __future__ import annotations

import json
from typing import Any

import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient, MemoriaConnectionError

BASE_URL = "http://localhost:8100"
BRANCH = "experiment-1"

BAD_RESPONSES: list[Any] = [
    {},
    {"error": "storage failed"},
    {"name": BRANCH, "error": "storage failed"},
    {"name": BRANCH, "result": "storage failed"},
    {"name": "wrong-branch"},
    {"result": "storage failed"},
    {"result": ""},
    {"result": "Created branch 'wrong-branch'"},
    {"result": ["Created branch 'experiment-1'"]},
    [],
    None,
]
GOOD_RESPONSES: list[dict[str, Any]] = [
    {"result": f"Created branch '{BRANCH}'"},
    {"name": BRANCH, "active": False},
]


def assert_single_creation_request(httpx_mock: HTTPXMock) -> None:
    requests = httpx_mock.get_requests()
    assert len(requests) == 1
    assert requests[0].method == "POST"
    assert requests[0].url.path == "/v1/branches"
    assert json.loads(requests[0].content) == {"name": BRANCH}


@pytest.mark.parametrize("payload", BAD_RESPONSES)
def test_sync_branch_create_rejects_malformed_receipt(
    httpx_mock: HTTPXMock, payload: Any
) -> None:
    httpx_mock.add_response(status_code=201, json=payload)
    with MemoriaClient(BASE_URL, "synthetic-key", max_retries=0) as client:
        with pytest.raises(MemoriaConnectionError, match="Invalid branch creation response"):
            client.branches.create(BRANCH)
    assert_single_creation_request(httpx_mock)


@pytest.mark.asyncio
@pytest.mark.parametrize("payload", BAD_RESPONSES)
async def test_async_branch_create_rejects_malformed_receipt(
    httpx_mock: HTTPXMock, payload: Any
) -> None:
    httpx_mock.add_response(status_code=201, json=payload)
    async with AsyncMemoriaClient(BASE_URL, "synthetic-key", max_retries=0) as client:
        with pytest.raises(MemoriaConnectionError, match="Invalid branch creation response"):
            await client.branches.create(BRANCH)
    assert_single_creation_request(httpx_mock)


@pytest.mark.parametrize("payload", GOOD_RESPONSES)
def test_sync_branch_create_accepts_known_receipts(
    httpx_mock: HTTPXMock, payload: dict[str, Any]
) -> None:
    httpx_mock.add_response(status_code=201, json=payload)
    with MemoriaClient(BASE_URL, "synthetic-key", max_retries=0) as client:
        branch = client.branches.create(BRANCH)
    assert branch.name == BRANCH
    assert_single_creation_request(httpx_mock)


@pytest.mark.asyncio
@pytest.mark.parametrize("payload", GOOD_RESPONSES)
async def test_async_branch_create_accepts_known_receipts(
    httpx_mock: HTTPXMock, payload: dict[str, Any]
) -> None:
    httpx_mock.add_response(status_code=201, json=payload)
    async with AsyncMemoriaClient(BASE_URL, "synthetic-key", max_retries=0) as client:
        branch = await client.branches.create(BRANCH)
    assert branch.name == BRANCH
    assert_single_creation_request(httpx_mock)
