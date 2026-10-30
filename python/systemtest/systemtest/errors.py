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

    def is_catalog_not_ready(self) -> bool:
        """Probe catalog is 503 ``not_ready``; action catalog is 409 ``conflict``."""
        if self.code == "viewer_busy":
            return False
        if self.status == 503 and self.code == "not_ready":
            return True
        return self.status == 409 and "catalog is not ready" in self.message.lower()


class TruncatedError(SystemTestError):
    """Probe log overflowed; samples are missing."""


class WaitError(SystemTestError):
    """wait() ended for a reason other than until_ns."""


class ProbeKindError(SystemTestError):
    """A probe was read with the wrong kind helper."""
