"""Registration and login — the only unauthenticated routes in the service."""

from fastapi import APIRouter, Depends, Response, status

from app.api.deps import enforce_auth_rate_limit, get_auth_service
from app.api.responses import error_responses
from app.api.schemas import (
    LoginRequest,
    RegisterRequest,
    SessionResponse,
    UserResponse,
)
from app.services.auth import AuthenticatedSession, AuthService

router = APIRouter(
    prefix="/auth",
    tags=["auth"],
    # These endpoints are reachable without credentials, so they are the ones
    # that need brute-force protection.
    dependencies=[Depends(enforce_auth_rate_limit)],
)


def _to_session_response(session: AuthenticatedSession) -> SessionResponse:
    return SessionResponse(
        user=UserResponse.model_validate(session.user),
        access_token=session.access_token,
        expires_in=session.expires_in,
    )


@router.post(
    "/register",
    status_code=status.HTTP_201_CREATED,
    response_model=SessionResponse,
    summary="Create an account",
    responses=error_responses(400, 409, 415, 429),
)
def register(
    payload: RegisterRequest,
    response: Response,
    auth_service: AuthService = Depends(get_auth_service),
) -> SessionResponse:
    session = auth_service.register(email=payload.email, password=payload.password)
    # Registration also returns a token: the alternative is forcing every client
    # to immediately POST /auth/login with credentials it just sent.
    response.headers["Location"] = f"/users/{session.user.id}"
    return _to_session_response(session)


@router.post(
    "/login",
    response_model=SessionResponse,
    summary="Exchange credentials for an access token",
    responses=error_responses(400, 401, 415, 429),
)
def login(
    payload: LoginRequest,
    auth_service: AuthService = Depends(get_auth_service),
) -> SessionResponse:
    return _to_session_response(
        auth_service.login(email=payload.email, password=payload.password)
    )
