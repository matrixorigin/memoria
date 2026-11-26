"""Branch apply responses must preserve memory IDs without coercing malformed receipts."""

import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaClient, MemoriaResponseError
from memoria.models import ApplyResult

_FIELDS = (
    "applied_adds",
    "applied_updates",
    "applied_removes",
    "applied_conflicts",
    "skipped_adds",
    "skipped_updates",
    "skipped_removes",
    "skipped_conflicts",
)


@pytest.mark.parametrize("field_name", _FIELDS)
@pytest.mark.parametrize(
    "invalid",
    ["memory-123", {"memory-123": True}, [123], [None], None, True, 12],
)
def test_apply_receipt_rejects_malformed_id_arrays(field_name: str, invalid: object) -> None:
    receipt = {name: [] for name in _FIELDS}
    receipt[field_name] = invalid
    with pytest.raises(MemoriaResponseError, match=field_name):
        ApplyResult.from_dict(receipt)


@pytest.mark.parametrize("field_name", _FIELDS)
def test_apply_receipt_preserves_id_arrays_without_aliasing(field_name: str) -> None:
    values = ["memory-1", "memory-2"]
    receipt = {name: [] for name in _FIELDS}
    receipt[field_name] = values
    result = ApplyResult.from_dict(receipt)
    assert getattr(result, field_name) == values
    assert getattr(result, field_name) is not values


@pytest.mark.parametrize("field_name", _FIELDS)
def test_apply_receipt_requires_every_result_array(field_name: str) -> None:
    receipt = {name: [] for name in _FIELDS}
    del receipt[field_name]
    with pytest.raises(MemoriaResponseError, match=field_name):
        ApplyResult.from_dict(receipt)


def test_apply_receipt_accepts_complete_empty_result() -> None:
    receipt = {name: [] for name in _FIELDS}
    result = ApplyResult.from_dict(receipt)
    assert all(getattr(result, field_name) == [] for field_name in _FIELDS)


def test_sync_apply_rejects_incomplete_success_receipt(
    sync_client: MemoriaClient, httpx_mock: HTTPXMock
) -> None:
    httpx_mock.add_response(method="POST", json={"applied_adds": ["memory-1"]})
    with pytest.raises(MemoriaResponseError, match="applied_updates"):
        sync_client.branches.apply("feature", adds=["memory-1"])


async def test_async_apply_rejects_incomplete_success_receipt(
    async_client: AsyncMemoriaClient, httpx_mock: HTTPXMock
) -> None:
    httpx_mock.add_response(method="POST", json={"applied_adds": ["memory-1"]})
    with pytest.raises(MemoriaResponseError, match="applied_updates"):
        await async_client.branches.apply("feature", adds=["memory-1"])
