"""Shared fixtures.

Every test gets its own in-memory database and its own application instance, so
tests are order-independent and leave nothing behind.
"""

from typing import Iterator

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import Engine

from app.config import Settings
from app.db.session import create_engine_from_url, create_session_factory, init_database
from app.main import create_app

TEST_SECRET = "test-secret-that-is-long-enough-to-pass-validation"
DEFAULT_PASSWORD = "correct-horse-battery-staple"


@pytest.fixture
def settings() -> Settings:
    # _env_file=None so a developer's local .env can never change test outcomes.
    return Settings(
        _env_file=None,
        environment="test",
        jwt_secret=TEST_SECRET,
        database_url="sqlite:///:memory:",
        # Minimum work factor: the suite hashes a lot of passwords and the
        # cost of bcrypt is not what is under test.
        bcrypt_rounds=4,
        # The limiter has its own unit test; here it must not interfere.
        auth_rate_limit_max=100_000,
    )


@pytest.fixture
def engine(settings: Settings) -> Iterator[Engine]:
    engine = create_engine_from_url(settings.database_url)
    init_database(engine)
    yield engine
    engine.dispose()


@pytest.fixture
def session_factory(engine: Engine):
    return create_session_factory(engine)


@pytest.fixture
def client(settings: Settings, engine: Engine) -> Iterator[TestClient]:
    with TestClient(create_app(settings=settings, engine=engine)) as test_client:
        yield test_client


class ApiUser:
    """A registered user plus the headers needed to act as them."""

    def __init__(self, user_id: str, email: str, token: str) -> None:
        self.id = user_id
        self.email = email
        self.token = token
        self.headers = {"Authorization": f"Bearer {token}"}


@pytest.fixture
def register_user(client: TestClient):
    def _register(email: str, password: str = DEFAULT_PASSWORD) -> ApiUser:
        response = client.post(
            "/auth/register", json={"email": email, "password": password}
        )
        assert response.status_code == 201, response.text
        body = response.json()
        return ApiUser(body["user"]["id"], body["user"]["email"], body["access_token"])

    return _register


@pytest.fixture
def alice(register_user) -> ApiUser:
    return register_user("alice@example.com")


@pytest.fixture
def bob(register_user) -> ApiUser:
    return register_user("bob@example.com")
