"""RFC 3339 fractions must parse on every supported Python version."""

from datetime import datetime, timedelta, timezone

import pytest

from memoria.models import Branch, Memory, Snapshot, _parse_dt


@pytest.mark.parametrize(
    "fraction", ["1", "12", "123", "1234", "12345", "123456", "1234567", "12345678", "123456789"]
)
@pytest.mark.parametrize("offset", ["Z", "+00:00", "+05:30", "-04:00"])
def test_response_fractional_timestamps(fraction, offset):
    stamp = f"2026-11-11T04:42:18.{fraction}{offset}"
    tz = {
        "Z": timezone.utc,
        "+00:00": timezone.utc,
        "+05:30": timezone(timedelta(hours=5, minutes=30)),
        "-04:00": timezone(-timedelta(hours=4)),
    }[offset]
    expected = datetime(2026, 11, 11, 4, 42, 18, int(fraction[:6].ljust(6, "0")), tzinfo=tz)
    memory = Memory.from_dict(
        {"memory_id": "m", "content": "c", "created_at": stamp, "observed_at": stamp}
    )
    assert memory.created_at == expected
    assert memory.observed_at == expected
    assert Snapshot.from_dict({"name": "s", "created_at": stamp}).created_at == expected
    assert Branch.from_dict({"name": "b", "created_at": stamp}).created_at == expected


@pytest.mark.parametrize(
    "stamp", [None, "", "garbage", "2026-13-11T04:42:18.123456789Z", "2026-11-11T25:42:18.1Z"]
)
def test_invalid_or_missing_timestamp(stamp):
    assert _parse_dt(stamp) is None


@pytest.mark.parametrize(
    "stamp", ["2026-11-11T04:42:18Z", "2026-11-11T04:42:18", "2026-11-11T04:42:18+00:00"]
)
def test_whole_seconds_and_naive_timestamp(stamp):
    assert _parse_dt(stamp) == datetime(2026, 11, 11, 4, 42, 18, tzinfo=timezone.utc)
