from __future__ import annotations

import httpx
import pytest
from systemtest.client import ClusterClient
from systemtest.errors import ApiError


def _client(handler) -> ClusterClient:
    return ClusterClient(
        "http://test",
        transport=httpx.MockTransport(handler),
        not_ready_timeout_s=1.0,
        not_ready_interval_s=0.0,
        sleep=lambda _seconds: None,
    )


def test_catalog_retries_503_then_succeeds() -> None:
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


def test_invoke_retries_action_catalog_409() -> None:
    calls = {"n": 0}

    def handler(request: httpx.Request) -> httpx.Response:
        calls["n"] += 1
        if calls["n"] == 1:
            return httpx.Response(
                409,
                json={
                    "error": {
                        "code": "conflict",
                        "message": "action catalog is not ready",
                    }
                },
            )
        return httpx.Response(202)

    client = _client(handler)
    client.invoke_action("sim1", "a", "sw1", "hold", {})
    assert calls["n"] == 2
    client.close()


def test_viewer_busy_is_not_retried() -> None:
    calls = {"n": 0}

    def handler(_request: httpx.Request) -> httpx.Response:
        calls["n"] += 1
        return httpx.Response(
            409,
            json={"error": {"code": "viewer_busy", "message": "simulation is busy"}},
        )

    client = _client(handler)
    with pytest.raises(ApiError) as err:
        client.catalog("sim1")
    assert err.value.code == "viewer_busy"
    assert calls["n"] == 1
    client.close()
