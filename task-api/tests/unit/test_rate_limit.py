"""The fixed-window limiter, driven by a fake clock so the test is instant."""

import pytest

from app.api.rate_limit import FixedWindowRateLimiter
from app.errors import RateLimitedError


class FakeClock:
    def __init__(self) -> None:
        self.now = 0.0

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


@pytest.fixture
def clock():
    return FakeClock()


@pytest.fixture
def limiter(clock):
    return FixedWindowRateLimiter(max_requests=3, window_seconds=60, clock=clock)


def test_requests_within_the_budget_are_allowed(limiter):
    for _ in range(3):
        limiter.check("1.2.3.4")


def test_the_request_after_the_budget_is_rejected(limiter):
    for _ in range(3):
        limiter.check("1.2.3.4")

    with pytest.raises(RateLimitedError) as raised:
        limiter.check("1.2.3.4")

    assert raised.value.status_code == 429
    assert raised.value.details["retry_after_seconds"] == 60


def test_the_budget_is_per_client(limiter):
    for _ in range(3):
        limiter.check("1.2.3.4")

    # A different client is unaffected.
    limiter.check("5.6.7.8")


def test_the_budget_refreshes_once_the_window_passes(limiter, clock):
    for _ in range(3):
        limiter.check("1.2.3.4")
    with pytest.raises(RateLimitedError):
        limiter.check("1.2.3.4")

    clock.advance(61)

    limiter.check("1.2.3.4")
