"""The auth endpoints are rate limited end to end.

Built with its own settings rather than the shared `client` fixture, which
deliberately raises the limit out of the way.
"""

import pytest
from fastapi.testclient import TestClient

from app.config import Settings
from app.db.session import create_engine_from_url, init_database
from app.main import create_app
from tests.conftest import DEFAULT_PASSWORD, TEST_SECRET

BUDGET = 3


@pytest.fixture
def throttled_client():
    settings = Settings(
        _env_file=None,
        environment="test",
        jwt_secret=TEST_SECRET,
        database_url="sqlite:///:memory:",
        bcrypt_rounds=4,
        auth_rate_limit_max=BUDGET,
        auth_rate_limit_window_seconds=60,
    )
    engine = create_engine_from_url(settings.database_url)
    init_database(engine)
    with TestClient(create_app(settings=settings, engine=engine)) as client:
        yield client
    engine.dispose()


def test_repeated_login_attempts_are_throttled(throttled_client):
    payload = {"email": "nobody@example.com", "password": DEFAULT_PASSWORD}

    for _ in range(BUDGET):
        assert throttled_client.post("/auth/login", json=payload).status_code == 401

    throttled = throttled_client.post("/auth/login", json=payload)

    assert throttled.status_code == 429
    assert throttled.json()["error"]["code"] == "TOO_MANY_REQUESTS"
    assert int(throttled.headers["Retry-After"]) > 0


def test_the_limit_covers_registration_too(throttled_client):
    for index in range(BUDGET):
        response = throttled_client.post(
            "/auth/register",
            json={"email": f"user{index}@example.com", "password": DEFAULT_PASSWORD},
        )
        assert response.status_code == 201

    assert (
        throttled_client.post(
            "/auth/register",
            json={"email": "one-too-many@example.com", "password": DEFAULT_PASSWORD},
        ).status_code
        == 429
    )


def test_authenticated_endpoints_are_not_throttled(throttled_client):
    registered = throttled_client.post(
        "/auth/register", json={"email": "busy@example.com", "password": DEFAULT_PASSWORD}
    )
    headers = {"Authorization": f"Bearer {registered.json()['access_token']}"}

    # Well past the auth budget; the limiter only guards the unauthenticated routes.
    for index in range(BUDGET * 3):
        response = throttled_client.post(
            "/projects", json={"name": f"Project {index}"}, headers=headers
        )
        assert response.status_code == 201
