"""A malformed activity flag must never turn an inactive record into an active one."""

import pytest

from memoria import MemoriaResponseError
from tests.conftest import MEMORY_STUB


@pytest.mark.parametrize("flag", ["false", 0, 1, None, [], {}])
def test_list_rejects_non_boolean_activity(sync_client, httpx_mock, flag):
    httpx_mock.add_response(json={"items": [{**MEMORY_STUB, "is_active": flag}]})
    with pytest.raises(MemoriaResponseError, match="is_active"):
        sync_client.memories.list()


async def test_async_list_rejects_non_boolean_activity(async_client, httpx_mock):
    httpx_mock.add_response(json={"items": [{**MEMORY_STUB, "is_active": "false"}]})
    with pytest.raises(MemoriaResponseError, match="is_active"):
        await async_client.memories.list()


def test_list_preserves_boolean_activity_and_legacy_default(sync_client, httpx_mock):
    legacy = {key: value for key, value in MEMORY_STUB.items() if key != "is_active"}
    httpx_mock.add_response(json={"items": [
        {**legacy, "is_active": False}, {**legacy, "is_active": True}, legacy,
    ]})
    assert [item.is_active for item in sync_client.memories.list().items] == [False, True, True]
