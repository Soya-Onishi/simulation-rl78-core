"""Errors raised by the system-test harness."""

from __future__ import annotations


class SystemTestError(Exception):
    """Base error for the harness."""


class ApiError(SystemTestError):
    def __init__(self, status: int, code: str, message: str) -> None:
        super().__init__(f"{code}: {message} (HTTP {status})")
        self.status = status
        self.code = code
        self.message = message


class TruncatedError(SystemTestError):
    """Probe log overflowed; samples are missing."""


class WaitError(SystemTestError):
    """wait() ended for a reason other than until_ns."""


class ProbeKindError(SystemTestError):
    """A probe was read with the wrong kind helper."""
