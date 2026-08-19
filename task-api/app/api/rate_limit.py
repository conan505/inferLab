"""A minimal fixed-window rate limiter.

Applied to the unauthenticated auth routes so password guessing and
registration spam are not free.

Deliberately in-process: it is honest protection for a single instance with no
external dependency. Behind more than one replica it must be replaced with a
shared store (Redis) — see the README.
"""

import threading
import time
from typing import Callable, Dict, Tuple

from app.errors import RateLimitedError

_MAX_TRACKED_CLIENTS = 10_000


class FixedWindowRateLimiter:
    def __init__(
        self,
        *,
        max_requests: int,
        window_seconds: int,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self._max = max_requests
        self._window = window_seconds
        self._clock = clock
        self._windows: Dict[str, Tuple[int, float]] = {}
        # FastAPI runs sync endpoints on a threadpool, so this is shared state.
        self._lock = threading.Lock()

    def check(self, key: str) -> None:
        """Count one request against ``key``; raise once the budget is spent."""
        now = self._clock()
        with self._lock:
            if len(self._windows) > _MAX_TRACKED_CLIENTS:
                self._evict_expired(now)

            count, resets_at = self._windows.get(key, (0, 0.0))
            if resets_at <= now:
                count, resets_at = 0, now + self._window

            count += 1
            self._windows[key] = (count, resets_at)
            over_budget = count > self._max
            retry_after = max(1, int(resets_at - now))

        if over_budget:
            raise RateLimitedError(
                "Too many requests; please retry later",
                details={"retry_after_seconds": retry_after},
            )

    def _evict_expired(self, now: float) -> None:
        expired = [key for key, (_, resets_at) in self._windows.items() if resets_at <= now]
        for key in expired:
            del self._windows[key]
