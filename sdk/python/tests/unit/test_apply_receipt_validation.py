"""Branch apply responses must preserve memory IDs without coercing malformed receipts."""

import pytest

from memoria import MemoriaResponseError
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
    with pytest.raises(MemoriaResponseError, match=field_name):
        ApplyResult.from_dict({field_name: invalid})


@pytest.mark.parametrize("field_name", _FIELDS)
def test_apply_receipt_preserves_id_arrays_without_aliasing(field_name: str) -> None:
    values = ["memory-1", "memory-2"]
    result = ApplyResult.from_dict({field_name: values})
    assert getattr(result, field_name) == values
    assert getattr(result, field_name) is not values


def test_apply_receipt_keeps_missing_fields_empty() -> None:
    result = ApplyResult.from_dict({})
    assert all(getattr(result, field_name) == [] for field_name in _FIELDS)
