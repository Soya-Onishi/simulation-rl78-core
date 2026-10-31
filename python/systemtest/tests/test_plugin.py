import httpx
import pytest

from systemtest.client import cluster_available
from systemtest.plugin import pytest_runtest_setup


def test_cluster_available_false_on_connect_error(monkeypatch) -> None:
    def boom(*_args, **_kwargs):
        raise httpx.ConnectError("refused")

    monkeypatch.setattr("systemtest.client.httpx.get", boom)
    assert cluster_available("http://127.0.0.1:9") is False


def test_sim_case_setup_skips_when_server_is_down(monkeypatch) -> None:
    monkeypatch.setattr("systemtest.plugin.cluster_available", lambda: False)

    class _Item:
        def get_closest_marker(self, name: str):
            return object() if name == "sim_case" else None

    with pytest.raises(pytest.skip.Exception, match="cluster-server is not running"):
        pytest_runtest_setup(_Item())  # type: ignore[arg-type]
