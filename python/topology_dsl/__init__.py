"""Thin Python DSL for logical multi-board topologies.

``python -m topology_dsl run <script.py>`` prints logical JSON (elf strings
unchanged). ``python -m topology_dsl create <script.py>`` embeds local ELF
files as ``base64:``, POSTs ``/api/simulation`` on a running cluster-server,
and prints the simulation URL. The server validates JSON
(``sim_cluster::topology``) and starts the arbiter. This is a runtime
handoff, not a checked-in product config.
"""

from __future__ import annotations

import base64
import json
import os
import runpy
import sys
import urllib.error
import urllib.request
import webbrowser
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, Optional
from urllib.parse import unquote, urlparse
from urllib.request import url2pathname

Payload = Literal["uart", "digital", "analog"]
Direction = Literal["in", "out"]


@dataclass
class EndpointRef:
    board_id: str
    name: str
    direction: Direction
    payload: Payload


@dataclass
class Board:
    id: str
    kind: str
    # One firmware source: filesystem path, file://, http(s)://, or
    # base64:<payload>. These are alternatives and are not combined.
    elf: Optional[str] = None
    _endpoints: dict[str, EndpointRef] = field(default_factory=dict, repr=False)

    def endpoint(self, name: str, *, direction: Direction, payload: Payload) -> EndpointRef:
        if name in self._endpoints:
            raise ValueError(f"board {self.id!r}: duplicate endpoint {name!r}")
        ref = EndpointRef(
            board_id=self.id, name=name, direction=direction, payload=payload
        )
        self._endpoints[name] = ref
        return ref

    def port(self, name: str) -> EndpointRef:
        try:
            return self._endpoints[name]
        except KeyError as exc:
            raise KeyError(f"board {self.id!r}: unknown endpoint {name!r}") from exc

    def to_dict(self) -> dict[str, Any]:
        out: dict[str, Any] = {
            "id": self.id,
            "kind": self.kind,
            "endpoints": [
                {
                    "name": ep.name,
                    "direction": ep.direction,
                    "payload": ep.payload,
                }
                for ep in self._endpoints.values()
            ],
        }
        if self.elf is not None:
            out["elf"] = self.elf
        return out


@dataclass
class Topology:
    margin_ns: int
    headroom_threshold_ns: int
    ready_timeout_ms: Optional[int] = None
    _boards: dict[str, Board] = field(default_factory=dict, repr=False)
    _edges: list[dict[str, Any]] = field(default_factory=list, repr=False)

    def board(
        self, board_id: str, *, kind: str = "rl78", elf: Optional[str] = None
    ) -> Board:
        if board_id in self._boards:
            raise ValueError(f"duplicate board id {board_id!r}")
        board = Board(id=board_id, kind=kind, elf=elf)
        self._boards[board_id] = board
        return board

    def plant(self, plant_id: str, *, kind: str = "plant") -> Board:
        """Register a plant node (no ELF). JSON is still a ``boards[]`` entry."""
        return self.board(plant_id, kind=kind)

    def add_edge(
        self,
        src: EndpointRef,
        dst: EndpointRef,
        *,
        uart_ring_len: Optional[int] = None,
    ) -> None:
        if src.direction != "out":
            raise ValueError(f"connect source must be out, got {src.direction!r}")
        if dst.direction != "in":
            raise ValueError(f"connect sink must be in, got {dst.direction!r}")
        if src.payload != dst.payload:
            raise ValueError("connect payload mismatch between endpoints")
        if src.board_id == dst.board_id:
            raise ValueError("connect must link two different boards")
        edge: dict[str, Any] = {
            "from_board": src.board_id,
            "from_endpoint": src.name,
            "to_board": dst.board_id,
            "to_endpoint": dst.name,
            "payload": src.payload,
        }
        if uart_ring_len is not None:
            edge["uart_ring_len"] = uart_ring_len
        self._edges.append(edge)

    def to_dict(self) -> dict[str, Any]:
        out: dict[str, Any] = {
            "boards": [b.to_dict() for b in self._boards.values()],
            "edges": list(self._edges),
            "margin_ns": self.margin_ns,
            "headroom_threshold_ns": self.headroom_threshold_ns,
        }
        if self.ready_timeout_ms is not None:
            out["ready_timeout_ms"] = self.ready_timeout_ms
        return out


def connect(
    src: EndpointRef,
    dst: EndpointRef,
    *,
    uart_ring_len: Optional[int] = None,
) -> None:
    """Record a directed edge. Both endpoints must belong to the same Topology."""
    # Edges are stored on the topology that owns the source board; locate it via
    # a module-level registry filled by Topology.board.
    topo = _OWNER.get(src.board_id)
    if topo is None:
        raise RuntimeError(f"board {src.board_id!r} is not attached to a Topology")
    if _OWNER.get(dst.board_id) is not topo:
        raise RuntimeError("connect endpoints must belong to the same Topology")
    topo.add_edge(src, dst, uart_ring_len=uart_ring_len)


_OWNER: dict[str, Topology] = {}
_EMITTED: list[Topology] = []


def _register_board(topo: Topology, board: Board) -> Board:
    _OWNER[board.id] = topo
    return board


# Patch Topology.board to register ownership for connect().
_orig_board = Topology.board


def _board(
    self: Topology, board_id: str, *, kind: str = "rl78", elf: Optional[str] = None
) -> Board:
    board = _orig_board(self, board_id, kind=kind, elf=elf)
    return _register_board(self, board)


Topology.board = _board  # type: ignore[method-assign]


def emit(topo: Topology) -> None:
    """Mark ``topo`` as the script result (printed as JSON by the runner)."""
    _EMITTED.clear()
    _EMITTED.append(topo)


DEFAULT_SERVER = "http://127.0.0.1:8090"
_USAGE = (
    "Usage:\n"
    "  python -m topology_dsl run <script.py>\n"
    "  python -m topology_dsl create <script.py> [--server URL] [--open]\n"
)


def _usage(code: int) -> None:
    print(_USAGE, end="", file=sys.stderr)
    raise SystemExit(code)


def _load_emitted(path: str) -> dict[str, Any]:
    _OWNER.clear()
    _EMITTED.clear()
    runpy.run_path(path, run_name="__main__")
    if not _EMITTED:
        raise SystemExit(
            "topology script did not call emit(topology); "
            "the runner requires emit() so logical JSON can be captured"
        )
    return _EMITTED[0].to_dict()


def _elf_kind(elf: str) -> str:
    lower = elf.lower()
    if lower.startswith("base64:"):
        return "base64"
    if lower.startswith("http://") or lower.startswith("https://"):
        return "http"
    if lower.startswith("file://"):
        return "file"
    return "path"


def _local_elf_path(elf: str) -> Path:
    if _elf_kind(elf) == "file":
        parsed = urlparse(elf)
        return Path(url2pathname(unquote(parsed.path)))
    return Path(elf)


def embed_local_elfs(topo: dict[str, Any]) -> dict[str, Any]:
    """Rewrite filesystem / ``file://`` elf strings to ``base64:`` (cwd-relative)."""
    for board in topo.get("boards", []):
        elf = board.get("elf")
        if not elf:
            continue
        kind = _elf_kind(elf)
        if kind in ("base64", "http"):
            continue
        path = _local_elf_path(elf)
        try:
            data = path.read_bytes()
        except OSError as err:
            board_id = board.get("id", "?")
            print(
                f"topology_dsl: elf file not found for board {board_id}: {path}: {err}",
                file=sys.stderr,
            )
            raise SystemExit(1) from err
        board["elf"] = "base64:" + base64.b64encode(data).decode("ascii")
    return topo


def resolve_server(cli_server: str | None, env: dict[str, str] | None = None) -> str:
    values = os.environ if env is None else env
    raw = cli_server or values.get("SIMULATION_SERVER") or DEFAULT_SERVER
    return raw.rstrip("/")


def post_simulation(server: str, topology: dict[str, Any]) -> str:
    url = f"{server}/api/simulation"
    payload = json.dumps({"topology": topology}).encode("utf-8")
    request = urllib.request.Request(
        url,
        data=payload,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request) as response:
            body = json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as err:
        detail = err.read().decode("utf-8", errors="replace")
        print(
            f"topology_dsl: POST {url} failed: HTTP {err.code}: {detail}",
            file=sys.stderr,
        )
        raise SystemExit(1) from err
    except urllib.error.URLError as err:
        print(f"topology_dsl: POST {url} failed: {err.reason}", file=sys.stderr)
        raise SystemExit(1) from err
    except json.JSONDecodeError as err:
        print(f"topology_dsl: invalid create response: {err}", file=sys.stderr)
        raise SystemExit(1) from err
    sim_id = body.get("id")
    if not sim_id:
        print("topology_dsl: create response missing id", file=sys.stderr)
        raise SystemExit(1)
    return str(sim_id)


def _require_script(path: str) -> str:
    script = Path(path)
    if not path or path.startswith("-"):
        _usage(2)
    if script.suffix != ".py":
        print("topology_dsl: create/run require a .py topology script", file=sys.stderr)
        raise SystemExit(2)
    if not script.is_file():
        print(f"topology_dsl: script not found: {path}", file=sys.stderr)
        raise SystemExit(2)
    return path


def _cmd_run(argv: list[str]) -> None:
    if len(argv) != 1 or argv[0] in ("-h", "--help"):
        _usage(0 if argv and argv[0] in ("-h", "--help") else 2)
    path = _require_script(argv[0])
    print(json.dumps(_load_emitted(path), indent=2, sort_keys=False))


def _parse_create(argv: list[str]) -> tuple[str, str | None, bool]:
    server: str | None = None
    open_browser = False
    script: str | None = None
    i = 0
    while i < len(argv):
        arg = argv[i]
        if arg in ("-h", "--help"):
            _usage(0)
        if arg == "--server":
            i += 1
            if i >= len(argv):
                print("topology_dsl: --server requires a URL", file=sys.stderr)
                _usage(2)
            server = argv[i]
        elif arg == "--open":
            open_browser = True
        elif arg.startswith("-"):
            print(f"topology_dsl: unknown argument {arg}", file=sys.stderr)
            _usage(2)
        elif script is not None:
            print("topology_dsl: unexpected extra argument", file=sys.stderr)
            _usage(2)
        else:
            script = arg
        i += 1
    if script is None:
        _usage(2)
    return script, server, open_browser


def _cmd_create(argv: list[str]) -> None:
    path, cli_server, open_browser = _parse_create(argv)
    path = _require_script(path)
    topology = embed_local_elfs(_load_emitted(path))
    server = resolve_server(cli_server)
    sim_id = post_simulation(server, topology)
    page = f"{server}/{sim_id}"
    print(page)
    if open_browser:
        webbrowser.open(page)


def main(argv: list[str] | None = None) -> None:
    argv = list(sys.argv[1:] if argv is None else argv)
    if not argv or argv[0] in ("-h", "--help"):
        _usage(0 if argv and argv[0] in ("-h", "--help") else 2)
    cmd, rest = argv[0], argv[1:]
    if cmd == "run":
        _cmd_run(rest)
        return
    if cmd == "create":
        _cmd_create(rest)
        return
    print(f"topology_dsl: unknown command {cmd}", file=sys.stderr)
    _usage(2)


if __name__ == "__main__":
    main()
