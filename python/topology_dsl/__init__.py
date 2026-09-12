"""Thin Python DSL for logical multi-board topologies.

Scripts are executed by ``cluster-server`` via ``python -m topology_dsl run``.
The resulting JSON is validated by Rust (``sim_cluster::topology``) and passed
to ``cluster-arbiter``. This is a runtime handoff, not a checked-in product
config.
"""

from __future__ import annotations

import json
import runpy
import sys
from dataclasses import dataclass, field
from typing import Any, Literal, Optional

Payload = Literal["uart", "digital"]
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


def _run_script(path: str) -> None:
    _OWNER.clear()
    _EMITTED.clear()
    runpy.run_path(path, run_name="__main__")
    if not _EMITTED:
        raise SystemExit(
            "topology script did not call emit(topology); "
            "cluster-server requires emit() so logical JSON can be captured"
        )
    print(json.dumps(_EMITTED[0].to_dict(), indent=2, sort_keys=False))


def main(argv: list[str] | None = None) -> None:
    argv = list(sys.argv[1:] if argv is None else argv)
    if not argv or argv[0] in ("-h", "--help"):
        print(
            "Usage: python -m topology_dsl run <script.py>\n"
            "Executed by cluster-server; prints logical topology JSON to stdout.",
            file=sys.stderr,
        )
        raise SystemExit(0 if argv and argv[0] in ("-h", "--help") else 2)
    if argv[0] != "run" or len(argv) != 2:
        print("Usage: python -m topology_dsl run <script.py>", file=sys.stderr)
        raise SystemExit(2)
    _run_script(argv[1])


if __name__ == "__main__":
    main()
