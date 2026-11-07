"""Hierarchy / SimulationConfig tests for topology_dsl."""

from __future__ import annotations

import unittest

from topology_dsl import SimulationConfig, Topology


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


class _SysTopo(Topology):
    def __init__(self, name: str, *, left_elf: str | None = None) -> None:
        super().__init__(name)
        self._left_elf = left_elf

    def build(self) -> None:
        left = self.add_child(PairTopo(name="left", elf_a=self._left_elf))
        right = self.board("d", kind="rl78")
        right.endpoint("rx", direction="in", payload="uart")
        right.endpoint("tx", direction="out", payload="uart")
        self.connect(left.port("tx"), right.port("rx"))


def nested_config(*, left_elf: str | None = None) -> SimulationConfig:
    return SimulationConfig(
        margin_ns=1000,
        headroom_threshold_ns=200,
        topology=_SysTopo(name="sys", left_elf=left_elf),
    )


class HierarchyToDictTests(unittest.TestCase):
    def test_full_ids_and_route(self) -> None:
        out = nested_config(left_elf="https://example.test/a.elf").to_dict()
        ids = {b["id"] for b in out["boards"]}
        self.assertEqual(ids, {"sys/left/a", "sys/left/b", "sys/d"})
        self.assertEqual(out["margin_ns"], 1000)
        self.assertEqual(out["headroom_threshold_ns"], 200)

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
                leaf = child._boards["a"].port("tx")
                d = self.board("d")
                d.endpoint("rx", direction="in", payload="uart")
                self.connect(leaf, d.port("rx"))

        with self.assertRaises(RuntimeError):
            SimulationConfig(
                margin_ns=1,
                headroom_threshold_ns=1,
                topology=Bad(name="sys"),
            ).to_dict()

    def test_rejects_mutate_child_after_add_child(self) -> None:
        caught: list[BaseException] = []

        class Parent(Topology):
            def build(self) -> None:
                child = self.add_child(PairTopo(name="left"))
                try:
                    child.board("extra")
                except RuntimeError as err:
                    caught.append(err)

        SimulationConfig(
            margin_ns=1,
            headroom_threshold_ns=1,
            topology=Parent(name="sys"),
        ).to_dict()
        self.assertEqual(len(caught), 1)

    def test_rejects_board_child_name_collision(self) -> None:
        class Collision(Topology):
            def build(self) -> None:
                self.board("left")
                self.add_child(PairTopo(name="left"))

        with self.assertRaises(ValueError):
            SimulationConfig(
                margin_ns=1,
                headroom_threshold_ns=1,
                topology=Collision(name="sys"),
            ).to_dict()

    def test_factory_can_rerun(self) -> None:
        first = nested_config(left_elf="https://example.test/a.elf").to_dict()
        second = nested_config(left_elf="https://example.test/a.elf").to_dict()
        self.assertEqual(first["boards"], second["boards"])
        self.assertEqual(first["edges"], second["edges"])

    def test_to_dict_is_idempotent(self) -> None:
        cfg = nested_config(left_elf="https://example.test/a.elf")
        first = cfg.to_dict()
        second = cfg.to_dict()
        self.assertEqual(first, second)
        first["boards"][0]["elf"] = "mutated"
        self.assertNotEqual(second["boards"][0].get("elf"), "mutated")

    def test_rejects_duplicate_connect(self) -> None:
        class Dup(Topology):
            def build(self) -> None:
                a = self.board("a")
                b = self.board("b")
                a.endpoint("tx", direction="out", payload="uart")
                b.endpoint("rx", direction="in", payload="uart")
                self.connect(a.port("tx"), b.port("rx"))
                self.connect(a.port("tx"), b.port("rx"))

        with self.assertRaises(ValueError):
            SimulationConfig(
                margin_ns=1,
                headroom_threshold_ns=1,
                topology=Dup(name="demo"),
            ).to_dict()


if __name__ == "__main__":
    unittest.main()
