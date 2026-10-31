"""Client-side UART byte stream assembled from probe samples."""

from __future__ import annotations

from dataclasses import dataclass, field


@dataclass
class _Word:
    virtual_time_ns: int
    byte: int
    consumed: bool = False


@dataclass
class UartStream:
    """Unread UART words with optional virtual-time windowing."""

    _words: list[_Word] = field(default_factory=list)

    def push(self, virtual_time_ns: int, value: int) -> None:
        self._words.append(_Word(virtual_time_ns, value & 0xFF))

    def readline(self, from_ns: int | None = None) -> bytes | None:
        visible = self._visible(from_ns)
        line: list[_Word] = []
        for word in visible:
            line.append(word)
            if word.byte == 0x0A:
                for item in line:
                    item.consumed = True
                raw = bytes(item.byte for item in line[:-1])
                if raw.endswith(b"\r"):
                    raw = raw[:-1]
                return raw
        return None

    def read(self, count: int, from_ns: int | None = None) -> bytes | None:
        if count <= 0:
            return b""
        visible = self._visible(from_ns)
        if len(visible) < count:
            return None
        out = bytes(word.byte for word in visible[:count])
        for word in visible[:count]:
            word.consumed = True
        return out

    def _visible(self, from_ns: int | None) -> list[_Word]:
        out: list[_Word] = []
        for word in self._words:
            if word.consumed:
                continue
            if from_ns is not None and word.virtual_time_ns < from_ns:
                continue
            out.append(word)
        return out
