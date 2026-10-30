"""HTTP client for cluster-server."""

from __future__ import annotations

import json
import os
import threading
import time
from collections.abc import Callable
from typing import Any, TypeVar

import httpx

from systemtest.errors import ApiError
from systemtest.sse import iter_sse

DEFAULT_URL = "http://127.0.0.1:8090"
URL_ENV = "SYSTEMTEST_URL"
RETRY_TIMEOUT_S = 15.0
RETRY_INTERVAL_S = 0.2

T = TypeVar("T")


def default_base_url() -> str:
    return os.environ.get(URL_ENV, DEFAULT_URL).rstrip("/")


def cluster_available(base_url: str | None = None) -> bool:
    """True when cluster-server accepts a TCP connection at the harness URL."""
    url = (base_url or default_base_url()).rstrip("/")
    try:
        httpx.get(url, timeout=0.5)
    except httpx.RequestError:
        return False
    return True


def raise_for_api(response: httpx.Response) -> None:
    if response.is_success:
        return
    code = "http_error"
    message = response.text
    try:
        payload = response.json()
        error = payload.get("error") or {}
        code = str(error.get("code") or code)
        message = str(error.get("message") or message)
    except (json.JSONDecodeError, ValueError, AttributeError):
        pass
    raise ApiError(response.status_code, code, message)


class ViewerHold:
    """Keep the viewer SSE open so the token stays valid."""

    def __init__(self, base_url: str, sim_id: str) -> None:
        self.token = ""
        self._stop = threading.Event()
        self._ready = threading.Event()
        self._error: BaseException | None = None
        self._client = httpx.Client(base_url=base_url, timeout=None)
        self._sim_id = sim_id
        self._thread = threading.Thread(target=self._run, name="systemtest-viewer", daemon=True)

    def start(self) -> str:
        self._thread.start()
        if not self._ready.wait(timeout=15):
            self.close()
            raise ApiError(504, "timeout", "viewer token was not issued")
        if self._error is not None:
            self.close()
            raise self._error
        return self.token

    def close(self) -> None:
        self._stop.set()
        self._client.close()
        self._thread.join(timeout=2)

    def _run(self) -> None:
        try:
            with self._client.stream("GET", f"/api/simulation/{self._sim_id}/viewer") as response:
                raise_for_api(response)
                for event, data in iter_sse(response):
                    if event == "viewer" and not self.token:
                        self.token = data.strip()
                        self._ready.set()
                    if self._stop.is_set():
                        break
        except BaseException as exc:
            self._error = exc
            self._ready.set()


class ClusterClient:
    def __init__(
        self,
        base_url: str | None = None,
        *,
        transport: httpx.BaseTransport | None = None,
        retry_timeout_s: float = RETRY_TIMEOUT_S,
        retry_interval_s: float = RETRY_INTERVAL_S,
        sleep: Callable[[float], None] = time.sleep,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self.base_url = (base_url or default_base_url()).rstrip("/")
        client_kwargs: dict[str, Any] = {"base_url": self.base_url, "timeout": 60.0}
        if transport is not None:
            client_kwargs["transport"] = transport
        self._http = httpx.Client(**client_kwargs)
        self._token: str | None = None
        self._retry_timeout_s = retry_timeout_s
        self._retry_interval_s = retry_interval_s
        self._sleep = sleep
        self._clock = clock

    def close(self) -> None:
        self._http.close()

    def set_token(self, token: str) -> None:
        self._token = token

    def _headers(self) -> dict[str, str]:
        if self._token is None:
            return {}
        return {"x-viewer-token": self._token}

    def _retry(self, op: Callable[[], T]) -> T:
        deadline = self._clock() + self._retry_timeout_s
        while True:
            try:
                return op()
            except Exception:
                if self._clock() >= deadline:
                    raise
                self._sleep(self._retry_interval_s)

    def create(self, topology: dict[str, Any]) -> str:
        response = self._http.post("/api/simulation", json={"topology": topology})
        raise_for_api(response)
        return str(response.json()["id"])

    def start(self, sim_id: str, until_ns: int | None) -> None:
        kwargs: dict[str, Any]
        if until_ns is None:
            kwargs = {"content": b""}
        else:
            kwargs = {"json": {"until_ns": until_ns}}
        response = self._http.post(
            f"/api/simulation/{sim_id}/start",
            headers=self._headers(),
            **kwargs,
        )
        raise_for_api(response)

    def invoke_action(
        self,
        sim_id: str,
        board_id: str,
        device: str,
        action: str,
        params: dict[str, float],
    ) -> None:
        def op() -> None:
            response = self._http.post(
                f"/api/simulation/{sim_id}/actions",
                headers=self._headers(),
                json={
                    "board_id": board_id,
                    "device": device,
                    "action": action,
                    "params": params,
                },
            )
            raise_for_api(response)

        self._retry(op)

    def catalog(self, sim_id: str) -> list[dict[str, Any]]:
        def op() -> list[dict[str, Any]]:
            response = self._http.get(
                f"/api/simulation/{sim_id}/probes/catalog",
                headers=self._headers(),
            )
            raise_for_api(response)
            payload = response.json()
            if not isinstance(payload, list):
                raise ApiError(response.status_code, "bad_response", "catalog was not a list")
            return payload

        return self._retry(op)

    def samples(
        self,
        sim_id: str,
        board: str,
        probe: str,
        from_ns: int | None = None,
        to_ns: int | None = None,
    ) -> dict[str, Any]:
        def op() -> dict[str, Any]:
            params: dict[str, str | int] = {"board": board, "probe": probe}
            if from_ns is not None:
                params["from_ns"] = from_ns
            if to_ns is not None:
                params["to_ns"] = to_ns
            response = self._http.get(
                f"/api/simulation/{sim_id}/probes/samples",
                headers=self._headers(),
                params=params,
            )
            raise_for_api(response)
            payload = response.json()
            if not isinstance(payload, dict):
                raise ApiError(response.status_code, "bad_response", "samples was not an object")
            return payload

        return self._retry(op)

    def csv(self, sim_id: str) -> bytes:
        def op() -> bytes:
            response = self._http.get(
                f"/api/simulation/{sim_id}/probes.csv",
                headers=self._headers(),
            )
            raise_for_api(response)
            return response.content

        return self._retry(op)

    def delete(self, sim_id: str) -> None:
        response = self._http.delete(
            f"/api/simulation/{sim_id}",
            headers=self._headers(),
        )
        raise_for_api(response)

    def iter_status(self, sim_id: str):
        with self._http.stream(
            "GET",
            f"/api/simulation/{sim_id}/status",
            headers=self._headers(),
            timeout=None,
        ) as response:
            raise_for_api(response)
            for event, data in iter_sse(response):
                if event == "status":
                    yield json.loads(data)
