"""Branch listings accept valid empty HTTP responses without leaking TypeError."""

import httpx
import pytest

from memoria import _http
from memoria.resources.branches import AsyncBranchesResource, BranchesResource


def branch_response(shape: str) -> httpx.Response:
    if shape == "no-content":
        return httpx.Response(204)
    if shape == "null":
        return httpx.Response(200, json={"branches": None})
    if shape == "empty":
        return httpx.Response(200, json={"branches": []})
    return httpx.Response(200, json={"branches": [{"name": "main", "active": True}]})


@pytest.mark.parametrize("shape", ["no-content", "null", "empty", "populated"])
def test_sync_branch_list_handles_empty_response(shape: str) -> None:
    def handle(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/v1/branches"
        assert request.method == "GET"
        return branch_response(shape)

    owner = _http._HttpTransport("https://memoria.test", "test-key", max_retries=0)
    with httpx.Client(transport=httpx.MockTransport(handle)) as client:
        owner._http = client
        branches = BranchesResource(owner).list()
    assert [branch.name for branch in branches] == ([] if shape != "populated" else ["main"])


@pytest.mark.parametrize("shape", ["no-content", "null", "empty", "populated"])
@pytest.mark.asyncio
async def test_async_branch_list_handles_empty_response(shape: str) -> None:
    def handle(request: httpx.Request) -> httpx.Response:
        assert request.url.path == "/v1/branches"
        assert request.method == "GET"
        return branch_response(shape)

    owner = _http._HttpTransport("https://memoria.test", "test-key", max_retries=0)
    async with httpx.AsyncClient(transport=httpx.MockTransport(handle)) as client:
        owner._ahttp = client
        branches = await AsyncBranchesResource(owner).list()
    assert [branch.name for branch in branches] == ([] if shape != "populated" else ["main"])
