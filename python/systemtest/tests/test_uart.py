from systemtest.uart import UartStream


def test_readline_strips_lf_and_crlf() -> None:
    stream = UartStream()
    for vt, ch in enumerate(b"ok\n"):
        stream.push(vt, ch)
    assert stream.readline() == b"ok"
    assert stream.readline() is None

    stream = UartStream()
    for vt, ch in enumerate(b"ok\r\n"):
        stream.push(vt, ch)
    assert stream.readline() == b"ok"


def test_empty_line_is_empty_bytes() -> None:
    stream = UartStream()
    stream.push(1, ord("\n"))
    assert stream.readline() == b""


def test_read_all_or_nothing() -> None:
    stream = UartStream()
    stream.push(1, ord("a"))
    stream.push(2, ord("b"))
    assert stream.read(3) is None
    assert stream.read(2) == b"ab"
    assert stream.read(1) is None


def test_since_does_not_consume_older_unread() -> None:
    stream = UartStream()
    stream.push(1, ord("a"))
    stream.push(10, ord("b"))
    stream.push(11, ord("\n"))
    assert stream.readline(from_ns=10) == b"b"
    assert stream.read(1) == b"a"
