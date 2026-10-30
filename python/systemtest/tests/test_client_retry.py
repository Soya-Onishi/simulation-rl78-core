from __future__ import annotations

import httpx
import pytest
from systemtest.client import ClusterClient
from systemtest.errors import ApiError


class _Clock:
    def __init__(self) -> None:
        self.t = 0.0

    def monotonic(self) -> float:
        return self.t

    def sleep(self, seconds: float) -> None:
        self.t += seconds


def _client(handler, clock: _Clock | None = None) -> ClusterClient:
    clock = clock or _Clock()
    return ClusterClient(
        "http://test",
        transport=httpx.MockTransport(handler),
        retry_timeout_s=1.0,
        retry_interval_s=0.2,
        sleep=clock.sleep,
        clock=clock.monotonic,
    )


def test_catalog_retries_error_then_succeeds() -> None:
    calls = {"n": 0}

    def handler(request: httpx.Request) -> httpx.Response:
        calls["n"] += 1
        if "/probes/catalog" not in str(request.url):
            return httpx.Response(404)
        if calls["n"] < 3:
            return httpx.Response(
                503,
                json={"error": {"code": "not_ready", "message": "probe catalog is not ready"}},
            )
        return httpx.Response(
            200, json=[{"board": "a", "probe": "uart0", "kind": "uart"}]
        )

    client = _client(handler)
    assert client.catalog("sim1")[0]["probe"] == "uart0"
    assert calls["n"] == 3
    client.close()


def test_retries_any_http_error() -> None:
    calls = {"n": 0}

    def handler(_request: httpx.Request) -> httpx.Response:
        calls["n"] += 1
        if calls["n"] == 1:
            return httpx.Response(
                409,
                json={"error": {"code": "viewer_busy", "message": "simulation is busy"}},
            )
        return httpx.Response(202)

    client = _client(handler)
    client.invoke_action("sim1", "a", "sw1", "hold", {})
    assert calls["n"] == 2
    client.close()


def test_gives_up_after_retry_timeout() -> None:
    clock = _Clock()

    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            404, json={"error": {"code": "not_found", "message": "unknown probe"}}
        )

    client = _client(handler, clock=clock)
    with pytest.raises(ApiError) as err:
        client.catalog("sim1")
    assert err.value.status == 404
    assert clock.t >= 1.0
    client.close()
