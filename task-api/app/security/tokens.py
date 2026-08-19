"""Stateless HS256 access tokens."""

from dataclasses import dataclass
from datetime import datetime, timedelta, timezone

import jwt

from app.errors import UnauthorizedError

ALGORITHM = "HS256"


@dataclass(frozen=True)
class IssuedToken:
    token: str
    expires_in: int  # seconds


class TokenService:
    """Signs and verifies access tokens.

    ``algorithms`` is pinned on decode: without it a token whose header claims
    ``alg: none`` — or an asymmetric algorithm verified against the secret as a
    public key — would be accepted. The only thing trusted from a token is the
    subject (the user id); authorisation is re-derived from the database on
    every request, so a still-valid token can never carry stale permissions.
    """

    def __init__(self, *, secret: str, expires_minutes: int = 15, issuer: str = "task-api") -> None:
        if not secret:
            raise ValueError("TokenService requires a secret")
        self._secret = secret
        self._lifetime = timedelta(minutes=expires_minutes)
        self._issuer = issuer

    def issue(self, *, user_id: str, email: str) -> IssuedToken:
        issued_at = datetime.now(timezone.utc)
        expires_at = issued_at + self._lifetime
        payload = {
            "sub": user_id,
            "email": email,
            "iss": self._issuer,
            "iat": issued_at,
            "exp": expires_at,
        }
        token = jwt.encode(payload, self._secret, algorithm=ALGORITHM)
        return IssuedToken(token=token, expires_in=int(self._lifetime.total_seconds()))

    def verify(self, token: str) -> dict:
        try:
            return jwt.decode(
                token,
                self._secret,
                algorithms=[ALGORITHM],
                issuer=self._issuer,
                options={"require": ["exp", "iat", "sub", "iss"]},
            )
        except jwt.ExpiredSignatureError:
            raise UnauthorizedError("Access token has expired", code="TOKEN_EXPIRED") from None
        except jwt.InvalidTokenError:
            raise UnauthorizedError("Access token is invalid", code="INVALID_TOKEN") from None
