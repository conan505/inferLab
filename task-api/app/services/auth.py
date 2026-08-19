"""Registration and login."""

from dataclasses import dataclass

from sqlalchemy.exc import IntegrityError

from app.db.models import User
from app.errors import ConflictError, UnauthorizedError
from app.repositories.users import UsersRepository
from app.security.passwords import PasswordHasher
from app.security.tokens import TokenService

_DUPLICATE_EMAIL = "That email address is already registered"
#: One message for both "no such account" and "wrong password", so the response
#: never reveals whether an address is registered.
_BAD_CREDENTIALS = "Invalid email or password"


def normalise_email(email: str) -> str:
    """Emails are compared case-insensitively, so they are stored normalised."""
    return email.strip().lower()


@dataclass(frozen=True)
class AuthenticatedSession:
    user: User
    access_token: str
    expires_in: int


class AuthService:
    def __init__(
        self,
        *,
        users: UsersRepository,
        password_hasher: PasswordHasher,
        token_service: TokenService,
    ) -> None:
        self._users = users
        self._hasher = password_hasher
        self._tokens = token_service

    def _issue_session(self, user: User) -> AuthenticatedSession:
        issued = self._tokens.issue(user_id=user.id, email=user.email)
        return AuthenticatedSession(
            user=user, access_token=issued.token, expires_in=issued.expires_in
        )

    def register(self, *, email: str, password: str) -> AuthenticatedSession:
        normalised = normalise_email(email)

        # Fast path for the common case; the unique index below is what actually
        # makes this correct when two registrations race.
        if self._users.find_by_email(normalised) is not None:
            raise ConflictError(_DUPLICATE_EMAIL, code="EMAIL_ALREADY_REGISTERED")

        try:
            user = self._users.create(
                email=normalised, hashed_password=self._hasher.hash(password)
            )
        except IntegrityError:
            raise ConflictError(_DUPLICATE_EMAIL, code="EMAIL_ALREADY_REGISTERED") from None

        return self._issue_session(user)

    def login(self, *, email: str, password: str) -> AuthenticatedSession:
        user = self._users.find_by_email(normalise_email(email))

        if user is None:
            # Same latency as a real verification, so timing does not leak
            # whether the account exists.
            self._hasher.verify_decoy(password)
            raise UnauthorizedError(_BAD_CREDENTIALS, code="INVALID_CREDENTIALS")

        if not self._hasher.verify(password, user.hashed_password):
            raise UnauthorizedError(_BAD_CREDENTIALS, code="INVALID_CREDENTIALS")

        return self._issue_session(user)
