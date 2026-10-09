"""Small REST adapter: no implicit retries for potentially committed mutations."""

import json
import threading
import time
from email.utils import parsedate_to_datetime

import httpx


class APIError(Exception):
    def __init__(
        self,
        code: str,
        *,
        uncertain: bool = False,
        retryable: bool = False,
        retry_after: float = 0,
        failure_kind: str = "",
    ):
        super().__init__(code)
        self.code, self.uncertain, self.retryable = code, uncertain, retryable
        self.retry_after = retry_after
        self.failure_kind = failure_kind or ("unknown" if uncertain else "rejected")


class Client:
    def __init__(self, url: str, key: str, timeout: float, *, transport=None):
        self._lock = threading.Lock()
        self._active = 0
        self._closing = False
        self._closed = False
        self.on_success = None
        self.http = httpx.Client(
            base_url=url.rstrip("/"),
            headers={"Authorization": f"Bearer {key}"},
            timeout=timeout,
            follow_redirects=False,
            transport=transport,
        )

    def request(self, method: str, path: str, *, timeout=None, **kwargs):
        with self._lock:
            if self._closing:
                raise APIError("client_closed")
            self._active += 1
        try:
            result = self._request(method, path, timeout=timeout, **kwargs)
            if self.on_success:
                self.on_success()
            return result
        finally:
            with self._lock:
                self._active -= 1
                if self._closing and not self._active and not self._closed:
                    self.http.close()
                    self._closed = True

    def _request(self, method: str, path: str, *, timeout=None, **kwargs):
        mutation = method in {"POST", "PUT", "DELETE"} and path != "/v1/memories/retrieve"
        try:
            # Limit untrusted response bodies before JSON decoding.
            with self.http.stream(
                method, path, timeout=timeout or self.http.timeout, **kwargs
            ) as r:
                if not 200 <= r.status_code < 300:
                    code = {
                        401: "authentication_failed",
                        403: "permission_denied",
                        404: "not_found",
                        429: "rate_limited",
                    }.get(r.status_code, f"http_{r.status_code}")
                    deduplicated = (
                        method == "POST"
                        and path == "/v1/observe/deduplicated"
                        and r.headers.get("X-Memoria-Observe-Deduplicated") == "1"
                    )
                    if method == "POST" and path == "/v1/observe/deduplicated":
                        if r.status_code == 404 and not deduplicated:
                            code = "capture_dedup_endpoint_unavailable"
                        elif (
                            r.status_code == 503
                            and deduplicated
                            and r.headers.get("X-Memoria-Observe-Error") == "extraction_unavailable"
                        ):
                            # This route-specific server marker certifies extraction
                            # failed before any persistence. Generic 503s remain unknown.
                            raise APIError("observe_extraction_unavailable", retryable=True)
                    retry_after = 0
                    if r.status_code == 429:
                        value = r.headers.get("Retry-After", "")
                        try:
                            retry_after = float(value)
                        except ValueError:
                            try:
                                retry_after = parsedate_to_datetime(value).timestamp() - time.time()
                            except (TypeError, ValueError, OverflowError):
                                retry_after = 0
                        retry_after = min(86400, max(0, retry_after))
                    raise APIError(
                        code,
                        uncertain=mutation and r.status_code >= 500,
                        retryable=r.status_code == 429,
                        retry_after=retry_after,
                    )
                body = bytearray()
                for chunk in r.iter_bytes():
                    body.extend(chunk)
                    if len(body) > 1024 * 1024:
                        raise APIError("response_too_large", uncertain=mutation)
                if not body:
                    return None
                try:
                    return json.loads(body)
                except (ValueError, UnicodeError):
                    raise APIError("invalid_response", uncertain=mutation) from None
        except (httpx.ConnectError, httpx.ConnectTimeout, httpx.PoolTimeout):
            raise APIError("connection_failed", retryable=True, failure_kind="not_sent") from None
        except httpx.HTTPError:
            raise APIError(
                "network_result_unknown" if mutation else "network_failed", uncertain=mutation
            ) from None

    def close(self):
        with self._lock:
            self._closing = True
            if not self._active and not self._closed:
                self.http.close()
                self._closed = True
