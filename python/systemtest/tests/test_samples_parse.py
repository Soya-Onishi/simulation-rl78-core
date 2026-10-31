from systemtest.errors import TruncatedError
from systemtest.simulation import Simulation


def test_truncated_payload_raises() -> None:
    sim = Simulation.__new__(Simulation)
    try:
        sim._raise_truncated({"truncated": True, "samples": []})
    except TruncatedError:
        return
    raise AssertionError("expected TruncatedError")


def test_safe_testname_sanitizes_brackets() -> None:
    from systemtest.simulation import safe_testname

    assert safe_testname("test_foo[case-0]") == "test_foo_case-0_"
