"""profile resource — sync and async."""

from __future__ import annotations

from typing import TYPE_CHECKING

from ..models import Profile

if TYPE_CHECKING:
    from .._http import _HttpTransport


def _profile_params(subject_id: str | None, branch: str | None) -> dict[str, str]:
    return {
        key: value
        for key, value in {"subject_id": subject_id, "branch": branch}.items()
        if value is not None
    }


class ProfileResource:
    def __init__(self, client: _HttpTransport) -> None:
        self._client = client

    def me(
        self,
        *,
        subject_id: str | None = None,
        branch: str | None = None,
    ) -> Profile:
        data = self._client._request(
            "GET", "/v1/profiles/me", params=_profile_params(subject_id, branch)
        )
        return Profile.from_dict(data)


class AsyncProfileResource:
    def __init__(self, client: _HttpTransport) -> None:
        self._client = client

    async def me(
        self,
        *,
        subject_id: str | None = None,
        branch: str | None = None,
    ) -> Profile:
        data = await self._client._arequest(
            "GET", "/v1/profiles/me", params=_profile_params(subject_id, branch)
        )
        return Profile.from_dict(data)
