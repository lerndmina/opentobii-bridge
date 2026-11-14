"""Smoothing for head pose.

`tobiid`'s head pose comes straight from its estimator, one per IR image, with
a few tenths of a degree of frame-to-frame noise. A game scales head rotation
up several times, so that noise shows as a shaking view.

The filter is the One Euro filter (Casiez, Roussel and Vogel, 2012): a low-pass
whose cutoff rises with the speed of the signal. A still head is filtered hard,
a turning head is followed with little lag.
"""

from __future__ import annotations

import math
from dataclasses import dataclass
from typing import Sequence


def _alpha(cutoff_hz: float, dt: float) -> float:
    tau = 1.0 / (2.0 * math.pi * cutoff_hz)
    return 1.0 / (1.0 + tau / dt)


class OneEuro:
    """One value. `min_cutoff` (Hz) sets how hard a still signal is filtered:
    lower is steadier and slower. `beta` sets how fast the cutoff opens with
    speed, in Hz per unit per second: higher follows fast moves more closely."""

    def __init__(self, min_cutoff: float, beta: float, derivative_cutoff: float = 1.0) -> None:
        if min_cutoff <= 0 or derivative_cutoff <= 0 or beta < 0:
            raise ValueError("cutoffs must be positive and beta not negative")
        self.min_cutoff = min_cutoff
        self.beta = beta
        self.derivative_cutoff = derivative_cutoff
        self.value: float | None = None
        self.speed = 0.0
        self.time = 0.0

    def reset(self) -> None:
        self.value = None
        self.speed = 0.0

    def __call__(self, value: float, seconds: float) -> float:
        if not math.isfinite(value):
            return value if self.value is None else self.value
        dt = seconds - self.time
        if self.value is None or dt <= 0 or dt > 1.0:
            # First sample, a clock that went back, or a gap long enough that
            # the old state says nothing about now: start again from here.
            self.value = value
            self.speed = 0.0
            self.time = seconds
            return value
        speed = (value - self.value) / dt
        self.speed += _alpha(self.derivative_cutoff, dt) * (speed - self.speed)
        cutoff = self.min_cutoff + self.beta * abs(self.speed)
        self.value += _alpha(cutoff, dt) * (value - self.value)
        self.time = seconds
        return self.value


@dataclass(frozen=True)
class Settings:
    """Rotation is filtered in degrees and position in mm, so the betas read
    as Hz per degree per second and Hz per mm per second."""

    rotation_min_cutoff: float = 0.8
    rotation_beta: float = 0.1
    position_min_cutoff: float = 0.8
    position_beta: float = 0.02


class PoseFilter:
    def __init__(self, settings: Settings = Settings()) -> None:
        self.position = [
            OneEuro(settings.position_min_cutoff, settings.position_beta) for _ in range(3)
        ]
        self.rotation = [
            OneEuro(settings.rotation_min_cutoff, settings.rotation_beta) for _ in range(3)
        ]

    def reset(self) -> None:
        for f in (*self.position, *self.rotation):
            f.reset()

    def __call__(self, micros: int, position_mm: Sequence[float],
                 rotation_rad: Sequence[float]) -> tuple[tuple[float, ...], tuple[float, ...]]:
        seconds = micros / 1e6
        position = tuple(f(v, seconds) for f, v in zip(self.position, position_mm))
        rotation = tuple(
            math.radians(f(math.degrees(v), seconds)) for f, v in zip(self.rotation, rotation_rad)
        )
        return position, rotation
