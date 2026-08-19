"""Composition root.

The one place where concrete implementations are chosen and wired together.
Everything below this module receives its collaborators as arguments, which is
what makes the layers independently testable and the storage choice swappable.
"""

import logging
from typing import Any, List, Optional

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from sqlalchemy import Engine, text
from starlette.exceptions import HTTPException as StarletteHTTPException

from app.api.middleware import RequestContextMiddleware, RequireJsonBodyMiddleware
from app.api.rate_limit import FixedWindowRateLimiter
from app.api.responses import json_error_response
from app.api.routes import auth as auth_routes
from app.api.routes import projects as project_routes
from app.api.routes import tasks as task_routes
from app.config import Settings, get_settings
from app.db.session import create_engine_from_url, create_session_factory, init_database
from app.errors import AppError, RateLimitedError
from app.security.passwords import PasswordHasher
from app.security.tokens import TokenService

logger = logging.getLogger("task-api")

DESCRIPTION = """
A task management API. Users own projects; tasks live inside projects; only a
project's owner may read or change anything inside it.

Every endpoint except `POST /auth/register` and `POST /auth/login` requires an
`Authorization: Bearer <token>` header.
"""

#: Starlette raises bare HTTPExceptions for routing failures; give them the same
#: envelope as everything else.
_HTTP_ERROR_CODES = {
    404: "ROUTE_NOT_FOUND",
    405: "METHOD_NOT_ALLOWED",
    413: "PAYLOAD_TOO_LARGE",
}


def _request_id(request: Request) -> str:
    return getattr(request.state, "request_id", "unknown")


def _format_validation_errors(raw_errors: List[dict]) -> List[dict]:
    """Flatten pydantic's error list into a stable, JSON-safe shape.

    Pydantic's own format (including `ctx`, which can hold exception objects) is
    an implementation detail that must not leak into the API contract.
    """
    formatted = []
    for error in raw_errors:
        location = error.get("loc", ())
        source = str(location[0]) if location else "body"
        field = ".".join(str(part) for part in location[1:]) or f"({source})"
        formatted.append(
            {
                "source": source,
                "field": field,
                "code": error.get("type", "invalid"),
                "message": error.get("msg", "invalid value"),
            }
        )
    return formatted


def _register_exception_handlers(app: FastAPI) -> None:
    @app.exception_handler(AppError)
    async def handle_app_error(request: Request, exc: AppError):
        headers = {}
        if isinstance(exc, RateLimitedError) and isinstance(exc.details, dict):
            headers["Retry-After"] = str(exc.details.get("retry_after_seconds", 60))
        return json_error_response(
            status_code=exc.status_code,
            code=exc.code,
            message=exc.message,
            request_id=_request_id(request),
            details=exc.details,
            headers=headers,
        )

    @app.exception_handler(RequestValidationError)
    async def handle_validation_error(request: Request, exc: RequestValidationError):
        raw_errors = exc.errors()
        # A body that is not parseable JSON at all is a different failure from a
        # body whose fields are wrong, and deserves its own code.
        if any(error.get("type") == "json_invalid" for error in raw_errors):
            return json_error_response(
                status_code=400,
                code="MALFORMED_JSON",
                message="Request body is not valid JSON",
                request_id=_request_id(request),
            )
        # FastAPI's default for validation failures is 422. This service reserves
        # 422 for requests that are well-formed but reference something that does
        # not exist, and reports schema violations as 400.
        return json_error_response(
            status_code=400,
            code="VALIDATION_ERROR",
            message="Request validation failed",
            request_id=_request_id(request),
            details=_format_validation_errors(raw_errors),
        )

    @app.exception_handler(StarletteHTTPException)
    async def handle_http_exception(request: Request, exc: StarletteHTTPException):
        return json_error_response(
            status_code=exc.status_code,
            code=_HTTP_ERROR_CODES.get(exc.status_code, "HTTP_ERROR"),
            message=str(exc.detail),
            request_id=_request_id(request),
            headers=getattr(exc, "headers", None),
        )

    @app.exception_handler(Exception)
    async def handle_unexpected_error(request: Request, exc: Exception):
        # Anything reaching here is a bug. Log it in full, tell the client
        # nothing: stack traces, SQL and file paths must not cross the boundary.
        request_id = _request_id(request)
        logger.exception(
            "Unhandled error on %s %s (request_id=%s)",
            request.method,
            request.url.path,
            request_id,
        )
        return json_error_response(
            status_code=500,
            code="INTERNAL_ERROR",
            message="An unexpected error occurred",
            request_id=request_id,
            headers={"X-Request-Id": request_id},
        )


def create_app(
    *, settings: Optional[Settings] = None, engine: Optional[Engine] = None
) -> FastAPI:
    settings = settings or get_settings()
    engine = engine or create_engine_from_url(settings.database_url)
    init_database(engine)

    app = FastAPI(
        title="Task Management API",
        version="1.0.0",
        description=DESCRIPTION,
    )

    # Process-wide singletons. PasswordHasher in particular computes a decoy
    # hash on construction, which costs real time at production work factors.
    app.state.settings = settings
    app.state.session_factory = create_session_factory(engine)
    app.state.password_hasher = PasswordHasher(rounds=settings.bcrypt_rounds)
    app.state.token_service = TokenService(
        secret=settings.jwt_secret,
        expires_minutes=settings.jwt_expires_minutes,
        issuer=settings.jwt_issuer,
    )
    app.state.auth_rate_limiter = FixedWindowRateLimiter(
        max_requests=settings.auth_rate_limit_max,
        window_seconds=settings.auth_rate_limit_window_seconds,
    )

    # Middleware runs outermost-first, so the request id is assigned before the
    # content-type guard can need it for an error body.
    app.add_middleware(RequireJsonBodyMiddleware)
    app.add_middleware(RequestContextMiddleware)

    _register_exception_handlers(app)

    app.include_router(auth_routes.router)
    app.include_router(project_routes.router)
    app.include_router(task_routes.project_tasks_router)
    app.include_router(task_routes.tasks_router)

    @app.get("/health", tags=["ops"], summary="Liveness and database check")
    def health() -> Any:
        with app.state.session_factory() as session:
            session.execute(text("SELECT 1"))
        return {"status": "ok"}

    return app
