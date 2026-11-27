"""Branch discovery must preserve the server's active-branch indicator."""

import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient, MemoriaResponseError
from tests.conftest import API_KEY, BASE_URL


@pytest.mark.parametrize("value", ["false", 1, 0, None, [], {}])
def test_branch_list_rejects_non_boolean_active(httpx_mock: HTTPXMock, value: object) -> None:
    httpx_mock.add_response(json={"branches": [{"name": "work", "active": value}]})
    with MemoriaClient(BASE_URL, API_KEY) as client:
        with pytest.raises(MemoriaResponseError, match="active"):
            client.branches.list()


async def test_async_branch_list_rejects_false_string(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json={"branches": [{"name": "work", "active": "false"}]})
    async with AsyncMemoriaClient(BASE_URL, API_KEY) as client:
        with pytest.raises(MemoriaResponseError, match="active"):
            await client.branches.list()


def test_branch_list_accepts_boolean_and_legacy_missing_flag(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(
        json={
            "branches": [
                {"name": "main", "active": True},
                {"name": "work", "active": False},
                {"name": "legacy"},
            ]
        }
    )
    with MemoriaClient(BASE_URL, API_KEY) as client:
        assert [branch.active for branch in client.branches.list()] == [True, False, False]
