"""Structured-query cursors share list()'s 32-hex memory ID contract."""

from __future__ import annotations

import asyncio
import json

import httpx
import pytest

from memoria._http import _HttpTransport
from memoria.exceptions import MemoriaValidationError
from memoria.resources.memories import AsyncMemoriesResource, MemoriesResource


@pytest.mark.parametrize("cursor", ["", " ", "abc", "f" * 31, "f" * 33, "g" * 32, 12, True])
@pytest.mark.parametrize("mode", ["sync", "async"])
def test_query_rejects_invalid_cursor_before_network(mode, cursor):
    calls = []

    def handler(request):
        calls.append(request)
        return httpx.Response(200, json={"items": [], "next_cursor": None})

    owner = _HttpTransport("https://memoria.test", "key", max_retries=0)
    if mode == "sync":
        with httpx.Client(transport=httpx.MockTransport(handler)) as client:
            owner._http = client
            with pytest.raises(MemoriaValidationError, match="query: cursor"):
                MemoriesResource(owner).query(subject_id="subject", cursor=cursor)
    else:
        async def run():
            async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as client:
                owner._ahttp = client
                with pytest.raises(MemoriaValidationError, match="query: cursor"):
                    await AsyncMemoriesResource(owner).query(subject_id="subject", cursor=cursor)
        asyncio.run(run())
    assert calls == []


@pytest.mark.parametrize("mode", ["sync", "async"])
def test_query_sends_valid_hex_cursor_unchanged(mode):
    cursor = "a" * 32
    calls = []

    def handler(request):
        calls.append(json.loads(request.content))
        return httpx.Response(200, json={"items": [], "next_cursor": None})

    owner = _HttpTransport("https://memoria.test", "key", max_retries=0)
    if mode == "sync":
        with httpx.Client(transport=httpx.MockTransport(handler)) as client:
            owner._http = client
            result = MemoriesResource(owner).query(subject_id="subject", cursor=cursor)
    else:
        async def run():
            async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as client:
                owner._ahttp = client
                return await AsyncMemoriesResource(owner).query(subject_id="subject", cursor=cursor)
        result = asyncio.run(run())
    assert result.items == []
    assert calls == [{"subject_id": "subject", "limit": 100, "cursor": cursor}]
