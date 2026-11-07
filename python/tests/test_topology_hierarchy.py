"""Hierarchy / Root.emit tests for topology_dsl."""

from __future__ import annotations

import unittest

from topology_dsl import Root, Topology


class PairTopo(Topology):
    def __init__(self, name: str, *, elf_a: str | None = None, elf_b: str | None = None) -> None:
        super().__init__(name)
        self._elf_a = elf_a
        self._elf_b = elf_b

    def build(self) -> None:
        a = self.board("a", kind="rl78", elf=self._elf_a)
        b = self.board("b", kind="rl78", elf=self._elf_b)
        a.endpoint("tx", direction="out", payload="uart")
        a.endpoint("rx", direction="in", payload="uart")
        b.endpoint("tx", direction="out", payload="uart")
        b.endpoint("rx", direction="in", payload="uart")
        self.connect(a.port("tx"), b.port("rx"))
        self.export("tx", a.port("tx"))
        self.export("rx", b.port("rx"))


class NestedRoot(Root):
    def __init__(self, *, left_elf: str | None = None) -> None:
        super().__init__()
        self._left_elf = left_elf

    def configure(self) -> None:
        self.margin_ns = 1000
        self.headroom_threshold_ns = 200
        self.topology = _SysTopo(name="sys", left_elf=self._left_elf)


class _SysTopo(Topology):
    def __init__(self, name: str, *, left_elf: str | None = None) -> None:
        super().__init__(name)
        self._left_elf = left_elf

    def build(self) -> None:
        left = self.add_child(PairTopo(name="left", elf_a=self._left_elf))
        right = self.board("d", kind="rl78")
        right.endpoint("rx", direction="in", payload="uart")
        right.endpoint("tx", direction="out", payload="uart")
        # Connect left's exported tx (leaf a) into local board d.
        self.connect(left.port("tx"), right.port("rx"))


class FlatRoot(Root):
    def configure(self) -> None:
        self.margin_ns = 1
        self.headroom_threshold_ns = 1
        self.topology = _FlatTopo(name="demo")


class _FlatTopo(Topology):
    def build(self) -> None:
        a = self.board("a", elf="fw.bin")
        a.endpoint("tx", direction="out", payload="uart")


class HierarchyEmitTests(unittest.TestCase):
    def test_full_ids_and_route(self) -> None:
        out = NestedRoot(left_elf="https://example.test/a.elf").emit()
        ids = {b["id"] for b in out["boards"]}
        self.assertEqual(ids, {"sys/left/a", "sys/left/b", "sys/d"})
        self.assertEqual(out["margin_ns"], 1000)
        self.assertEqual(out["headroom_threshold_ns"], 200)

        # Edges: left internal a->b, plus sys-level left.tx -> d.rx
        self.assertEqual(len(out["edges"]), 2)
        cross = next(
            e
            for e in out["edges"]
            if e["from"]["path"] == "sys/left/a" and e["to"]["path"] == "sys/d"
        )
        self.assertEqual(cross["from"], {"path": "sys/left/a", "port": "tx"})
        self.assertEqual(cross["to"], {"path": "sys/d", "port": "rx"})
        self.assertEqual(
            cross["route"],
            [
                {"path": "sys/left/a", "port": "tx"},
                {"path": "sys/left", "port": "tx"},
                {"path": "sys/d", "port": "rx"},
            ],
        )
        self.assertEqual(cross["route"][0], cross["from"])
        self.assertEqual(cross["route"][-1], cross["to"])

    def test_rejects_deep_connect(self) -> None:
        class Bad(Topology):
            def build(self) -> None:
                child = self.add_child(PairTopo(name="left"))
                # Reach into child leaf — not visible at this level.
                leaf = child._boards["a"].port("tx")
                d = self.board("d")
                d.endpoint("rx", direction="in", payload="uart")
                self.connect(leaf, d.port("rx"))

        class BadRoot(Root):
            def configure(self) -> None:
                self.margin_ns = 1
                self.headroom_threshold_ns = 1
                self.topology = Bad(name="sys")

        with self.assertRaises(RuntimeError):
            BadRoot().emit()

    def test_rejects_mutate_child_after_add_child(self) -> None:
        caught: list[BaseException] = []

        class Parent(Topology):
            def build(self) -> None:
                child = self.add_child(PairTopo(name="left"))
                try:
                    child.board("extra")
                except RuntimeError as err:
                    caught.append(err)

        class R(Root):
            def configure(self) -> None:
                self.margin_ns = 1
                self.headroom_threshold_ns = 1
                self.topology = Parent(name="sys")

        R().emit()
        self.assertEqual(len(caught), 1)

    def test_rejects_board_child_name_collision(self) -> None:
        class Collision(Topology):
            def build(self) -> None:
                self.board("left")
                self.add_child(PairTopo(name="left"))

        class R(Root):
            def configure(self) -> None:
                self.margin_ns = 1
                self.headroom_threshold_ns = 1
                self.topology = Collision(name="sys")

        with self.assertRaises(ValueError):
            R().emit()

    def test_emit_can_rerun_on_same_root(self) -> None:
        root = NestedRoot(left_elf="https://example.test/a.elf")
        first = root.emit()
        second = root.emit()
        self.assertEqual(first["boards"], second["boards"])
        self.assertEqual(first["edges"], second["edges"])


if __name__ == "__main__":
    unittest.main()
