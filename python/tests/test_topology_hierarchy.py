"""Hierarchy / SimulationConfig tests for topology_dsl."""

from __future__ import annotations

import unittest

from topology_dsl import EndpointLoc, PortRef, SimulationConfig, Topology


class PairTopo(Topology):
    def __init__(self, name: str, *, elf_a: str | None = None, elf_b: str | None = None) -> None:
        super().__init__(name)
        self._elf_a = elf_a
        self._elf_b = elf_b

    def build(self) -> None:
        a = self._board("a", kind="rl78", elf=self._elf_a)
        b = self._board("b", kind="rl78", elf=self._elf_b)
        a.endpoint("tx", direction="out", payload="uart")
        a.endpoint("rx", direction="in", payload="uart")
        b.endpoint("tx", direction="out", payload="uart")
        b.endpoint("rx", direction="in", payload="uart")
        self._connect(a.port("tx"), b.port("rx"))
        self._export("tx", a.port("tx"))
        self._export("rx", b.port("rx"))


class _SysTopo(Topology):
    def __init__(self, name: str, *, left_elf: str | None = None) -> None:
        super().__init__(name)
        self._left_elf = left_elf

    def build(self) -> None:
        left = self._add_child(PairTopo(name="left", elf_a=self._left_elf))
        right = self._board("d", kind="rl78")
        right.endpoint("rx", direction="in", payload="uart")
        right.endpoint("tx", direction="out", payload="uart")
        self._connect(left.port("tx"), right.port("rx"))


def nested_config(*, left_elf: str | None = None) -> SimulationConfig:
    return SimulationConfig(
        margin_ns=1000,
        topology=_SysTopo(name="sys", left_elf=left_elf),
    )


class HierarchyToDictTests(unittest.TestCase):
    def test_full_ids_and_route(self) -> None:
        out = nested_config(left_elf="https://example.test/a.elf").to_dict()
        ids = {b["id"] for b in out["boards"]}
        self.assertEqual(ids, {"sys/left/a", "sys/left/b", "sys/d"})
        self.assertEqual(out["margin_ns"], 1000)

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
                child = self._add_child(PairTopo(name="left"))
                # Bypass export on purpose: leaf port is not visible to parent.
                leaf = PortRef(
                    chain=[EndpointLoc(path=f"{child.path}/a", port="tx")],
                    direction="out",
                    payload="uart",
                )
                d = self._board("d")
                d.endpoint("rx", direction="in", payload="uart")
                self._connect(leaf, d.port("rx"))

        with self.assertRaises(RuntimeError):
            SimulationConfig(
                margin_ns=1,
                topology=Bad(name="sys"),
            ).to_dict()

    def test_rejects_mutate_child_after_add_child(self) -> None:
        caught: list[BaseException] = []

        class Parent(Topology):
            def build(self) -> None:
                child = self._add_child(PairTopo(name="left"))
                try:
                    child._board("extra")
                except RuntimeError as err:
                    caught.append(err)

        SimulationConfig(
            margin_ns=1,
            topology=Parent(name="sys"),
        ).to_dict()
        self.assertEqual(len(caught), 1)

    def test_rejects_board_child_name_collision(self) -> None:
        class Collision(Topology):
            def build(self) -> None:
                self._board("left")
                self._add_child(PairTopo(name="left"))

        with self.assertRaises(ValueError):
            SimulationConfig(
                margin_ns=1,
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
                a = self._board("a")
                b = self._board("b")
                a.endpoint("tx", direction="out", payload="uart")
                b.endpoint("rx", direction="in", payload="uart")
                self._connect(a.port("tx"), b.port("rx"))
                self._connect(a.port("tx"), b.port("rx"))

        with self.assertRaises(ValueError):
            SimulationConfig(
                margin_ns=1,
                topology=Dup(name="demo"),
            ).to_dict()

    def test_rejects_duplicate_leaf_edge_across_hierarchy(self) -> None:
        class Parent(Topology):
            def build(self) -> None:
                left = self._add_child(PairTopo(name="left"))
                # Same leaf pair as PairTopo's internal a.tx -> b.rx, via exports.
                self._connect(left.port("tx"), left.port("rx"))

        with self.assertRaises(ValueError) as ctx:
            SimulationConfig(
                margin_ns=1,
                topology=Parent(name="sys"),
            ).to_dict()
        self.assertIn("duplicate edge", str(ctx.exception))

    def test_endpoint_and_visibility_use_path_not_id(self) -> None:
        seen: dict[str, str] = {}

        class T(Topology):
            def build(self) -> None:
                a = self._board("a")
                b = self._board("b")
                a.endpoint("tx", direction="out", payload="uart")
                b.endpoint("rx", direction="in", payload="uart")
                # Path references must not depend on id staying equal to path.
                a.id = "uuid-a"
                b.id = "uuid-b"
                seen["a_path"] = a.path
                seen["a_leaf"] = a.port("tx").leaf.path
                seen["a_id"] = a.id
                self._connect(a.port("tx"), b.port("rx"))

        T(name="demo").run_build()
        self.assertEqual(seen["a_path"], "demo/a")
        self.assertEqual(seen["a_leaf"], "demo/a")
        self.assertEqual(seen["a_id"], "uuid-a")


if __name__ == "__main__":
    unittest.main()
