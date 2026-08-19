"""FastAPI dependencies — the wiring between HTTP and the service layer.

Repositories and services are constructed per request around that request's
database session, so a handler can never accidentally share a transaction with
another request.
"""

from typing import Iterator, Optional

from fastapi import Depends, Request
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from sqlalchemy.orm import Session

from app.config import Settings
from app.db.models import User
from app.db.session import session_scope
from app.errors import UnauthorizedError
from app.repositories.projects import ProjectsRepository
from app.repositories.tasks import TasksRepository
from app.repositories.users import UsersRepository
from app.security.tokens import TokenService
from app.services.auth import AuthService
from app.services.projects import ProjectsService
from app.services.tasks import TasksService

#: auto_error=False so the service produces its own structured 401 body instead
#: of Starlette's bare ``{"detail": ...}``.
bearer_scheme = HTTPBearer(auto_error=False, description="JWT access token")


def get_settings(request: Request) -> Settings:
    return request.app.state.settings


def get_token_service(request: Request) -> TokenService:
    # Built once at startup: it holds the signing secret and is stateless.
    return request.app.state.token_service


def get_session(request: Request) -> Iterator[Session]:
    """Transaction per request: commit on success, roll back on any exception."""
    yield from session_scope(request.app.state.session_factory)


def get_users_repository(session: Session = Depends(get_session)) -> UsersRepository:
    return UsersRepository(session)


def get_projects_repository(session: Session = Depends(get_session)) -> ProjectsRepository:
    return ProjectsRepository(session)


def get_tasks_repository(session: Session = Depends(get_session)) -> TasksRepository:
    return TasksRepository(session)


def get_auth_service(
    request: Request,
    users: UsersRepository = Depends(get_users_repository),
    token_service: TokenService = Depends(get_token_service),
) -> AuthService:
    return AuthService(
        users=users,
        # Shared instance: constructing a PasswordHasher computes a decoy hash,
        # which at production work factors costs a few hundred milliseconds.
        password_hasher=request.app.state.password_hasher,
        token_service=token_service,
    )


def get_projects_service(
    projects: ProjectsRepository = Depends(get_projects_repository),
) -> ProjectsService:
    return ProjectsService(projects=projects)


def get_tasks_service(
    tasks: TasksRepository = Depends(get_tasks_repository),
    users: UsersRepository = Depends(get_users_repository),
    projects_service: ProjectsService = Depends(get_projects_service),
) -> TasksService:
    return TasksService(tasks=tasks, users=users, projects_service=projects_service)


def get_current_user(
    request: Request,
    credentials: Optional[HTTPAuthorizationCredentials] = Depends(bearer_scheme),
    token_service: TokenService = Depends(get_token_service),
    users: UsersRepository = Depends(get_users_repository),
) -> User:
    """Resolve the bearer token to a live user.

    The extra lookup costs one indexed query per request and buys revocation:
    a deleted account's still-unexpired token stops working immediately rather
    than remaining valid until it expires.
    """
    if request.headers.get("authorization") is None:
        raise UnauthorizedError("Authorization header is missing", code="MISSING_TOKEN")
    if credentials is None:
        raise UnauthorizedError(
            'Authorization header must be of the form "Bearer <token>"',
            code="MALFORMED_AUTH_HEADER",
        )

    # Raises UnauthorizedError with TOKEN_EXPIRED or INVALID_TOKEN.
    payload = token_service.verify(credentials.credentials)

    user = users.find_by_id(payload["sub"])
    if user is None:
        raise UnauthorizedError(
            "The account for this token no longer exists", code="INVALID_TOKEN"
        )
    return user


def client_ip(request: Request, settings: Settings) -> str:
    if settings.trust_proxy:
        forwarded = request.headers.get("x-forwarded-for")
        if forwarded:
            return forwarded.split(",")[0].strip()
    return request.client.host if request.client else "unknown"


def enforce_auth_rate_limit(
    request: Request, settings: Settings = Depends(get_settings)
) -> None:
    request.app.state.auth_rate_limiter.check(client_ip(request, settings))
