"""Virtual-time duration used by wait() and UART windows."""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class Duration:
    """A span of virtual time.

    Construct with ``Duration.ns`` / ``us`` / ``ms`` / ``sec``. The stored
    count is ``nanoseconds``.
    """

    nanoseconds: int

    def __post_init__(self) -> None:
        if self.nanoseconds < 0:
            raise ValueError("Duration cannot be negative")

    @classmethod
    def ns(cls, n: int) -> Duration:
        return cls(int(n))

    @classmethod
    def us(cls, n: int | float) -> Duration:
        return cls(int(n * 1_000))

    @classmethod
    def ms(cls, n: int | float) -> Duration:
        return cls(int(n * 1_000_000))

    @classmethod
    def sec(cls, n: int | float) -> Duration:
        return cls(int(n * 1_000_000_000))
