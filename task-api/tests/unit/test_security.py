"""Password hashing and token handling."""

from datetime import datetime, timedelta, timezone

import jwt
import pytest

from app.errors import UnauthorizedError
from app.security.passwords import MAX_PASSWORD_BYTES, PasswordHasher
from app.security.tokens import TokenService

SECRET = "a-secret-that-is-comfortably-longer-than-32-chars"


@pytest.fixture(scope="module")
def hasher():
    return PasswordHasher(rounds=4)


def test_hash_is_not_the_plaintext(hasher):
    hashed = hasher.hash("correct-horse-battery")
    assert hashed != "correct-horse-battery"
    assert hashed.startswith("$2")


def test_same_password_hashes_differently_each_time(hasher):
    """Per-password salt: two users with the same password must not share a hash."""
    assert hasher.hash("same-password-here") != hasher.hash("same-password-here")


def test_verify_accepts_the_right_password_and_rejects_others(hasher):
    hashed = hasher.hash("correct-horse-battery")
    assert hasher.verify("correct-horse-battery", hashed)
    assert not hasher.verify("Correct-horse-battery", hashed)
    assert not hasher.verify("", hashed)


def test_verify_of_an_unparseable_hash_fails_rather_than_raising(hasher):
    assert not hasher.verify("anything", "not-a-bcrypt-hash")


def test_decoy_verification_always_fails(hasher):
    assert not hasher.verify_decoy("anything at all")


def test_over_long_password_is_rejected_rather_than_truncated(hasher):
    with pytest.raises(ValueError):
        hasher.hash("x" * (MAX_PASSWORD_BYTES + 1))


@pytest.fixture
def tokens():
    return TokenService(secret=SECRET, expires_minutes=15, issuer="task-api")


def test_round_trip(tokens):
    issued = tokens.issue(user_id="user-1", email="a@example.com")
    payload = tokens.verify(issued.token)
    assert payload["sub"] == "user-1"
    assert issued.expires_in == 15 * 60


def test_a_token_signed_with_another_secret_is_rejected(tokens):
    foreign = TokenService(secret="a-completely-different-secret-value-here")
    issued = foreign.issue(user_id="user-1", email="a@example.com")

    with pytest.raises(UnauthorizedError) as raised:
        tokens.verify(issued.token)
    assert raised.value.code == "INVALID_TOKEN"


def test_an_expired_token_is_reported_as_expired(tokens):
    expired = jwt.encode(
        {
            "sub": "user-1",
            "iss": "task-api",
            "iat": datetime.now(timezone.utc) - timedelta(hours=2),
            "exp": datetime.now(timezone.utc) - timedelta(hours=1),
        },
        SECRET,
        algorithm="HS256",
    )

    with pytest.raises(UnauthorizedError) as raised:
        tokens.verify(expired)
    assert raised.value.code == "TOKEN_EXPIRED"
    assert raised.value.status_code == 401


def test_an_unsigned_token_is_rejected(tokens):
    """`alg: none` is the classic JWT bypass; pinning algorithms on decode
    is what stops it."""
    unsigned = jwt.encode(
        {"sub": "user-1", "iss": "task-api", "iat": 0, "exp": 9_999_999_999},
        key="",
        algorithm="none",
    )

    with pytest.raises(UnauthorizedError):
        tokens.verify(unsigned)


def test_a_token_from_another_issuer_is_rejected(tokens):
    other_issuer = TokenService(secret=SECRET, issuer="some-other-service")
    issued = other_issuer.issue(user_id="user-1", email="a@example.com")

    with pytest.raises(UnauthorizedError):
        tokens.verify(issued.token)


def test_a_token_without_an_expiry_is_rejected(tokens):
    forever = jwt.encode({"sub": "user-1", "iss": "task-api", "iat": 0}, SECRET, algorithm="HS256")

    with pytest.raises(UnauthorizedError):
        tokens.verify(forever)


@pytest.mark.parametrize("garbage", ["", "not.a.token", "a.b.c", "Bearer"])
def test_garbage_is_rejected(tokens, garbage):
    with pytest.raises(UnauthorizedError):
        tokens.verify(garbage)
