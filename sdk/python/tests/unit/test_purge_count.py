"""A successful purge must report how many memories were deleted."""

import pytest

from memoria import MemoriaResponseError


@pytest.mark.parametrize(
    "payload",
    [{}, {"purged": None}, {"purged": False}, {"purged": -1}, {"purged": 1.5}, {"purged": "2"}],
)
@pytest.mark.parametrize("asynchronous", [False, True])
async def test_invalid_purge_count(sync_client, async_client, httpx_mock, payload, asynchronous):
    httpx_mock.add_response(json=payload)
    with pytest.raises(MemoriaResponseError, match="purged"):
        if asynchronous:
            await async_client.memories.purge(topic="obsolete")
        else:
            sync_client.memories.purge(topic="obsolete")


def test_zero_purge_count(sync_client, httpx_mock):
    httpx_mock.add_response(json={"purged": 0, "warning": "no matches"})
    result = sync_client.memories.purge(topic="obsolete")
    assert result.purged == 0
    assert result.warning == "no matches"
