"""Hierarchical Python DSL for logical multi-board topologies.

Define a concrete ``Topology`` (``build`` with ``_board`` / ``_connect`` / ...) and a ``@simulation`` function that
returns ``SimulationConfig``. ``python -m topology_dsl run|create <script.py>``
loads the script, requires exactly one ``@simulation`` function, and turns its
config into flat simulation JSON.

``create`` embeds local ELF files as ``base64:``, POSTs ``/api/simulation`` on a
running simulation-frontend (start ``cluster-backend`` first), and prints the
simulation URL.
"""

from __future__ import annotations

import base64
import copy
import json
import os
import runpy
import sys
import urllib.error
import urllib.request
import webbrowser
from abc import ABC, abstractmethod
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, Optional
from urllib.parse import unquote, urlparse
from urllib.request import url2pathname

Payload = Literal["uart", "digital", "analog", "pulse", "scalar"]
Direction = Literal["in", "out"]


@dataclass(frozen=True)
class EndpointLoc:
    """One hop in a directed-edge ``route`` (and ``from`` / ``to``)."""

    path: str
    port: str

    def to_dict(self) -> dict[str, str]:
        return {"path": self.path, "port": self.port}


@dataclass
class PortRef:
    """A visible port: leaf board endpoint or exported topology boundary."""

    chain: list[EndpointLoc]
    direction: Direction
    payload: Payload

    @property
    def leaf(self) -> EndpointLoc:
        return self.chain[0]


@dataclass
class Board:
    id: str
    # Hierarchical location in the topology tree (may differ from ``id`` later).
    path: str
    kind: str
    # One firmware source: filesystem path, file://, http(s)://, or
    # base64:<payload>. These are alternatives and are not combined.
    elf: Optional[str] = None
    _endpoints: dict[str, PortRef] = field(default_factory=dict, repr=False)

    def endpoint(self, name: str, *, direction: Direction, payload: Payload) -> PortRef:
        if not name:
            raise ValueError(f"board {self.id!r}: endpoint name must not be empty")
        if name in self._endpoints:
            raise ValueError(f"board {self.id!r}: duplicate endpoint {name!r}")
        ref = PortRef(
            chain=[EndpointLoc(path=self.path, port=name)],
            direction=direction,
            payload=payload,
        )
        self._endpoints[name] = ref
        return ref

    def port(self, name: str) -> PortRef:
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
                    "name": ep.leaf.port,
                    "direction": ep.direction,
                    "payload": ep.payload,
                }
                for ep in self._endpoints.values()
            ],
        }
        if self.elf is not None:
            out["elf"] = self.elf
        return out


class Topology(ABC):
    """Reusable hierarchical topology definition."""

    def __init__(self, name: str) -> None:
        if not name:
            raise ValueError("topology name must not be empty")
        if "/" in name:
            raise ValueError(f"topology name must not contain '/': {name!r}")
        self.name = name
        self._parent: Topology | None = None
        self._children: dict[str, Topology] = {}
        self._boards: dict[str, Board] = {}
        self._exports: dict[str, PortRef] = {}
        self._edges: list[dict[str, Any]] = []
        self._built = False
        self._building = False

    @property
    def path(self) -> str:
        if self._parent is None:
            return self.name
        return f"{self._parent.path}/{self.name}"

    @property
    def is_built(self) -> bool:
        return self._built

    @property
    def is_root(self) -> bool:
        return self._parent is None

    @abstractmethod
    def build(self) -> None:
        """Register boards/plants, children, exports, and connections."""

    def _board(
        self, local_name: str, *, kind: str = "rl78", elf: Optional[str] = None
    ) -> Board:
        self._require_building()
        if not local_name:
            raise ValueError("board local name must not be empty")
        if "/" in local_name:
            raise ValueError(f"board local name must not contain '/': {local_name!r}")
        if local_name in self._boards:
            raise ValueError(f"duplicate board local name {local_name!r}")
        if local_name in self._children:
            raise ValueError(
                f"board local name {local_name!r} conflicts with a child topology"
            )
        board_path = f"{self.path}/{local_name}"
        # Today the wire-facing board id equals the hierarchical path; keep the
        # fields separate so path comparisons never depend on id remaining so.
        board = Board(id=board_path, path=board_path, kind=kind, elf=elf)
        self._boards[local_name] = board
        return board

    def _plant(self, local_name: str, *, kind: str = "plant") -> Board:
        """Register a plant node (no ELF). JSON is still a ``boards[]`` entry."""
        return self._board(local_name, kind=kind)

    def _add_child(self, child: Topology) -> Topology:
        self._require_building()
        if not isinstance(child, Topology):
            raise TypeError("_add_child expects a Topology instance")
        if not child.is_root:
            raise ValueError(f"topology {child.name!r} already has a parent")
        if child.name in self._children:
            raise ValueError(f"duplicate child topology name {child.name!r}")
        if child.name in self._boards:
            raise ValueError(
                f"child topology name {child.name!r} conflicts with a board"
            )
        if child.is_built:
            raise ValueError(f"topology {child.name!r} is already built")
        child._attach_parent(self)
        self._children[child.name] = child
        child.run_build()
        return child

    def _export(self, name: str, endpoint: PortRef) -> PortRef:
        self._require_building()
        if not name:
            raise ValueError("export name must not be empty")
        if name in self._exports:
            raise ValueError(f"topology {self.path!r}: duplicate export {name!r}")
        if not self._is_visible(endpoint):
            raise RuntimeError(
                f"topology {self.path!r}: export target is not visible at this level"
            )
        exported = PortRef(
            chain=[*endpoint.chain, EndpointLoc(path=self.path, port=name)],
            direction=endpoint.direction,
            payload=endpoint.payload,
        )
        self._exports[name] = exported
        return exported

    def port(self, name: str) -> PortRef:
        try:
            return self._exports[name]
        except KeyError as exc:
            raise KeyError(f"topology {self.path!r}: unknown export {name!r}") from exc

    def _connect(
        self,
        src: PortRef,
        dst: PortRef,
        *,
        uart_ring_len: Optional[int] = None,
    ) -> None:
        self._require_building()
        if not self._is_visible(src) or not self._is_visible(dst):
            raise RuntimeError(
                f"topology {self.path!r}: connect endpoints must be visible at this level"
            )
        if src.direction != "out":
            raise ValueError(f"connect source must be out, got {src.direction!r}")
        if dst.direction != "in":
            raise ValueError(f"connect sink must be in, got {dst.direction!r}")
        if src.payload != dst.payload:
            raise ValueError("connect payload mismatch between endpoints")
        if src.leaf.path == dst.leaf.path:
            raise ValueError("connect must link two different boards")
        leaf_key = (
            src.leaf.path,
            src.leaf.port,
            dst.leaf.path,
            dst.leaf.port,
        )
        for existing in self._edges:
            if (
                existing["from"]["path"],
                existing["from"]["port"],
                existing["to"]["path"],
                existing["to"]["port"],
            ) == leaf_key:
                raise ValueError(
                    "duplicate connect between "
                    f"{src.leaf.path!r}:{src.leaf.port!r} and "
                    f"{dst.leaf.path!r}:{dst.leaf.port!r}"
                )
        route_locs = [*src.chain, *reversed(dst.chain)]
        edge: dict[str, Any] = {
            "from": src.leaf.to_dict(),
            "to": dst.leaf.to_dict(),
            "route": [loc.to_dict() for loc in route_locs],
            "payload": src.payload,
        }
        if uart_ring_len is not None:
            edge["uart_ring_len"] = uart_ring_len
        self._edges.append(edge)

    def run_build(self) -> None:
        if self.is_built:
            raise RuntimeError(f"topology {self.path!r} is already built")
        self._building = True
        try:
            self.build()
            self._built = True
        finally:
            self._building = False

    def _attach_parent(self, parent: Topology) -> None:
        self._parent = parent

    def _require_building(self) -> None:
        if not self._building:
            raise RuntimeError(
                f"topology {self.name!r}: "
                "_board/_connect/_export/_add_child only valid during build()"
            )

    def _is_visible(self, ref: PortRef) -> bool:
        last = ref.chain[-1]
        if len(ref.chain) == 1:
            local = last.path.rsplit("/", 1)[-1]
            board = self._boards.get(local)
            if (
                board is not None
                and board.path == last.path
                and last.port in board._endpoints
            ):
                return True
        for child in self._children.values():
            if last.path == child.path and last.port in child._exports:
                return True
        return False

    def collect_boards(self) -> list[Board]:
        boards = list(self._boards.values())
        for child in self._children.values():
            boards.extend(child.collect_boards())
        return boards

    def collect_edges(self) -> list[dict[str, Any]]:
        edges = list(self._edges)
        for child in self._children.values():
            edges.extend(child.collect_edges())
        return edges


def _reject_duplicate_edges(edges: list[dict[str, Any]]) -> None:
    """Reject duplicate leaf endpoints across the flattened edge list.

    Matches sim-cluster validation: ``(from.path, from.port, to.path, to.port)``.
    """
    seen: set[tuple[str, str, str, str]] = set()
    for edge in edges:
        src = edge["from"]
        dst = edge["to"]
        key = (src["path"], src["port"], dst["path"], dst["port"])
        if key in seen:
            raise ValueError(
                "duplicate edge "
                f"'{src['path']}:{src['port']}' -> '{dst['path']}:{dst['port']}'"
            )
        seen.add(key)


_SIMULATION_MARKER = "__topology_dsl_simulation__"


@dataclass
class SimulationConfig:
    """Timing settings plus the root ``Topology`` for one simulation."""

    margin_ns: int
    topology: Topology
    booting_timeout_ms: int | None = None
    _cached: dict[str, Any] | None = field(default=None, init=False, repr=False)

    def to_dict(self) -> dict[str, Any]:
        """Build the topology once and return flat simulation JSON (idempotent).

        Returns a deep copy so callers (e.g. ``embed_local_elfs``) can mutate
        the dict without corrupting the cached value.
        """
        if self._cached is not None:
            return copy.deepcopy(self._cached)
        if not self.topology.is_root:
            raise RuntimeError("SimulationConfig.topology must be a top-level Topology")
        if not self.topology.is_built:
            self.topology.run_build()
        boards = self.topology.collect_boards()
        edges = self.topology.collect_edges()
        _reject_duplicate_edges(edges)
        out: dict[str, Any] = {
            "boards": [b.to_dict() for b in boards],
            "edges": edges,
            "margin_ns": self.margin_ns,
        }
        if self.booting_timeout_ms is not None:
            out["booting_timeout_ms"] = self.booting_timeout_ms
        self._cached = out
        return copy.deepcopy(out)


def simulation(
    fn: Callable[[], SimulationConfig],
) -> Callable[[], SimulationConfig]:
    """Mark a topology factory for CLI discovery (exactly one per script)."""
    setattr(fn, _SIMULATION_MARKER, True)
    return fn


def _is_simulation_factory(obj: object) -> bool:
    return callable(obj) and bool(getattr(obj, _SIMULATION_MARKER, False))


def _simulation_factories_in_namespace(
    ns: dict[str, Any],
) -> list[Callable[[], SimulationConfig]]:
    """Collect ``@simulation`` factories defined by the loaded script itself."""
    found: list[Callable[[], SimulationConfig]] = []
    seen: set[int] = set()
    for obj in ns.values():
        if not _is_simulation_factory(obj):
            continue
        if getattr(obj, "__module__", None) != "__main__":
            continue
        key = id(obj)
        if key in seen:
            continue
        seen.add(key)
        found.append(obj)  # type: ignore[arg-type]
    return found


DEFAULT_SERVER = "http://127.0.0.1:8090"
_USAGE = (
    "Usage:\n"
    "  python -m topology_dsl run <script.py>\n"
    "  python -m topology_dsl create <script.py> [--server URL] [--open]\n"
)


def _usage(code: int) -> None:
    print(_USAGE, end="", file=sys.stderr)
    raise SystemExit(code)


def _load_config_dict(path: str) -> dict[str, Any]:
    ns = runpy.run_path(path, run_name="__main__")
    factories = _simulation_factories_in_namespace(ns)
    if len(factories) == 0:
        raise SystemExit(
            "topology script has no @simulation function; "
            "define exactly one () -> SimulationConfig factory"
        )
    if len(factories) > 1:
        names = ", ".join(getattr(fn, "__name__", repr(fn)) for fn in factories)
        raise SystemExit(
            f"topology script has {len(factories)} @simulation functions "
            f"({names}); define exactly one"
        )
    config = factories[0]()
    if not isinstance(config, SimulationConfig):
        raise SystemExit(
            "@simulation function must return SimulationConfig, "
            f"got {type(config).__name__}"
        )
    return config.to_dict()


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
    print(json.dumps(_load_config_dict(path), indent=2, sort_keys=False))


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
    topology = embed_local_elfs(_load_config_dict(path))
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
