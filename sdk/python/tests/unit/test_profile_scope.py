"""Profile lookups preserve the server's subject and branch isolation."""

from __future__ import annotations

from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient
from tests.conftest import API_KEY, BASE_URL

PROFILE_STUB = {
    "user_id": "user_1",
    "profile": "Prefers concise answers",
    "stats": {"total": 1},
}


def test_sync_profile_forwards_subject_and_branch(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=PROFILE_STUB)

    with MemoriaClient(BASE_URL, API_KEY, max_retries=0) as client:
        profile = client.profile.me(subject_id="customer/a", branch="release & qa")

    request = httpx_mock.get_request()
    assert request is not None
    assert dict(request.url.params) == {
        "subject_id": "customer/a",
        "branch": "release & qa",
    }
    assert profile.user_id == "user_1"


async def test_async_profile_forwards_subject_and_branch(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=PROFILE_STUB)

    async with AsyncMemoriaClient(BASE_URL, API_KEY, max_retries=0) as client:
        profile = await client.profile.me(subject_id="customer/a", branch="release & qa")

    request = httpx_mock.get_request()
    assert request is not None
    assert dict(request.url.params) == {
        "subject_id": "customer/a",
        "branch": "release & qa",
    }
    assert profile.user_id == "user_1"


def test_sync_profile_omits_unspecified_scope(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=PROFILE_STUB)

    with MemoriaClient(BASE_URL, API_KEY, max_retries=0) as client:
        client.profile.me()

    request = httpx_mock.get_request()
    assert request is not None
    assert not request.url.params


async def test_async_profile_omits_unspecified_scope(httpx_mock: HTTPXMock) -> None:
    httpx_mock.add_response(json=PROFILE_STUB)

    async with AsyncMemoriaClient(BASE_URL, API_KEY, max_retries=0) as client:
        await client.profile.me()

    request = httpx_mock.get_request()
    assert request is not None
    assert not request.url.params
