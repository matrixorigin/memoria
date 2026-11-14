"""HTTP error details must be strings, including structured FastAPI 422 errors."""

from __future__ import annotations

import json

import httpx
import pytest

from memoria._http import _map_error
from memoria.exceptions import MemoriaServerError, MemoriaUnprocessableError


@pytest.mark.parametrize(
    "payload,expected",
    [
        (
            {"detail": [{"loc": ["body", "content"], "msg": "required"}]},
            '[{"loc":["body","content"],"msg":"required"}]',
        ),
        (
            {"detail": {"field": "content", "reason": "invalid"}},
            '{"field":"content","reason":"invalid"}',
        ),
        ({"detail": True}, "true"),
        ({"detail": "bad input"}, "bad input"),
    ],
)
def test_structured_validation_detail_is_string(payload, expected):
    err = _map_error(httpx.Response(422, json=payload))
    assert isinstance(err, MemoriaUnprocessableError)
    assert isinstance(err.detail, str)
    if expected.startswith(("[", "{")):
        assert json.loads(err.detail) == json.loads(expected)
    else:
        assert err.detail == expected


def test_plain_text_server_error_keeps_readable_detail():
    err = _map_error(httpx.Response(500, content=b"server exploded"))
    assert isinstance(err, MemoriaServerError)
    assert err.detail == "server exploded"
