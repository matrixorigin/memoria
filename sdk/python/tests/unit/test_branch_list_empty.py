"""Branch listings accept valid empty HTTP responses without leaking TypeError."""

import httpx
import pytest

from memoria import MemoriaResponseError, _http
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


@pytest.mark.parametrize("payload", [{}, {"result": "ok"}, {"branches": {}}, "", 42])
def test_sync_branch_list_rejects_invalid_envelopes(sync_client, httpx_mock, payload):
    httpx_mock.add_response(json=payload)
    with pytest.raises(MemoriaResponseError, match="branch list"):
        sync_client.branches.list()


async def test_async_branch_list_rejects_missing_branches(async_client, httpx_mock):
    httpx_mock.add_response(json={"result": "ok"})
    with pytest.raises(MemoriaResponseError, match="branch list"):
        await async_client.branches.list()


def test_branch_list_accepts_legacy_array(sync_client, httpx_mock):
    httpx_mock.add_response(json=[{"name": "main"}, {"name": "work"}])
    assert [branch.name for branch in sync_client.branches.list()] == ["main", "work"]
