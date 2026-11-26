"""Governance results must distinguish completed work from a cooldown."""

import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient, MemoriaResponseError


@pytest.mark.parametrize("flag", ["false", "true", 0, 1, None, [], {}])
def test_run_rejects_non_boolean_skipped(
    sync_client: MemoriaClient, httpx_mock: HTTPXMock, flag: object
) -> None:
    httpx_mock.add_response(method="POST", json={"skipped": flag, "cooldown_remaining_s": 5})
    with pytest.raises(MemoriaResponseError, match="skipped"):
        sync_client.governance.run()


@pytest.mark.parametrize(
    "receipt",
    [
        {"skipped": True},
        {"skipped": True, "cooldown_remaining_s": None},
        {"skipped": True, "cooldown_remaining_s": True},
        {"skipped": True, "cooldown_remaining_s": -1},
        {"skipped": True, "cooldown_remaining_s": 1.5},
        {"skipped": True, "cooldown_remaining_s": "5"},
    ],
)
async def test_reflect_rejects_invalid_remaining_seconds(
    async_client: AsyncMemoriaClient, httpx_mock: HTTPXMock, receipt: dict[str, object]
) -> None:
    httpx_mock.add_response(method="POST", json=receipt)
    with pytest.raises(MemoriaResponseError, match="cooldown_remaining_s"):
        await async_client.governance.reflect()


def test_completed_run_without_skipped_field(
    sync_client: MemoriaClient, httpx_mock: HTTPXMock
) -> None:
    httpx_mock.add_response(method="POST", json={"quarantined": 2, "cleaned_stale": 1})
    result = sync_client.governance.run()
    assert not result.skipped
    assert result.cooldown_remaining_s is None
    assert result.quarantined == 2
