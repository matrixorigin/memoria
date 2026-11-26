"""Branch display names must occupy exactly one URL path segment."""

from __future__ import annotations

import httpx
import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient
from tests.conftest import API_KEY, BASE_URL

NAMES = [
    ("feature/实验", b"feature%2F%E5%AE%9E%E9%AA%8C"),
    ("target?mode=delete", b"target%3Fmode%3Ddelete"),
    ("target#part", b"target%23part"),
    ("literal%2Fname", b"literal%252Fname"),
    (".", b"%2E"),
    ("..", b"%2E%2E"),
    ("simple-name", b"simple-name"),
]
OPERATIONS = [
    ("checkout", b"/checkout", {}),
    ("diff", b"/diff", {}),
    ("diff_items", b"/diff-items", {"limit": 2, "cursor": "next"}),
    ("merge", b"/merge", {}),
    ("delete", b"", {}),
    ("apply", b"/apply", {"adds": ["memory-1"]}),
    ("pick", b"/pick", {"selector": {"type": "key_list", "keys": ["memory-1"]}}),
]


def assert_branch_request(
    request: httpx.Request, encoded_name: bytes, suffix: bytes
) -> httpx.Response:
    assert request.url.raw_path.split(b"?", 1)[0] == b"/v1/branches/" + encoded_name + suffix
    if suffix == b"/diff-items":
        assert dict(request.url.params) == {"limit": "2", "cursor": "next"}
    else:
        assert not request.url.query
    if suffix == b"/apply":
        # Keep the path-encoding fixture faithful to the API's required receipt.
        return httpx.Response(
            200,
            json={
                "applied_adds": ["memory-1"],
                "applied_updates": [],
                "applied_removes": [],
                "applied_conflicts": [],
                "skipped_adds": [],
                "skipped_updates": [],
                "skipped_removes": [],
                "skipped_conflicts": [],
            },
        )
    return httpx.Response(200, json={})


@pytest.mark.parametrize("name,encoded_name", NAMES)
@pytest.mark.parametrize("operation,suffix,kwargs", OPERATIONS)
def test_sync_branch_name_is_one_segment(
    httpx_mock: HTTPXMock, name: str, encoded_name: bytes,
    operation: str, suffix: bytes, kwargs: dict,
) -> None:
    httpx_mock.add_callback(lambda request: assert_branch_request(request, encoded_name, suffix))
    with MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        getattr(client.branches, operation)(name, **kwargs)
    assert len(httpx_mock.get_requests()) == 1


@pytest.mark.parametrize("name,encoded_name", NAMES)
@pytest.mark.parametrize("operation,suffix,kwargs", OPERATIONS)
async def test_async_branch_name_is_one_segment(
    httpx_mock: HTTPXMock, name: str, encoded_name: bytes,
    operation: str, suffix: bytes, kwargs: dict,
) -> None:
    httpx_mock.add_callback(lambda request: assert_branch_request(request, encoded_name, suffix))
    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0) as client:
        await getattr(client.branches, operation)(name, **kwargs)
    assert len(httpx_mock.get_requests()) == 1
