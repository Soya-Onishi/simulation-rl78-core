"""Minimal SSE parser for httpx streaming responses."""

from __future__ import annotations

from collections.abc import Iterator

import httpx


def iter_sse(response: httpx.Response) -> Iterator[tuple[str, str]]:
    event = "message"
    data: list[str] = []
    for raw in response.iter_lines():
        line = raw.decode("utf-8") if isinstance(raw, bytes) else raw
        if line == "":
            if data:
                yield event, "\n".join(data)
            event = "message"
            data = []
            continue
        if line.startswith(":"):
            continue
        if line.startswith("event:"):
            event = line[6:].strip()
            continue
        if line.startswith("data:"):
            data.append(line[5:].lstrip())
