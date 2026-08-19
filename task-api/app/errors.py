"""One error taxonomy for the whole service.

Services raise these; the exception handlers registered in :mod:`app.main` are
the only place that knows how to turn them into HTTP responses. Business logic
therefore never imports ``fastapi`` and stays directly unit-testable.
"""

from typing import Any, Optional


class AppError(Exception):
    """Base class for errors that are safe to show the caller."""

    status_code: int = 500
    code: str = "INTERNAL_ERROR"
    default_message: str = "An unexpected error occurred"

    def __init__(
        self,
        message: Optional[str] = None,
        *,
        code: Optional[str] = None,
        details: Any = None,
    ) -> None:
        self.message = message or self.default_message
        if code is not None:
            self.code = code
        self.details = details
        super().__init__(self.message)


class ValidationError(AppError):
    status_code = 400
    code = "VALIDATION_ERROR"
    default_message = "Request validation failed"


class UnauthorizedError(AppError):
    status_code = 401
    code = "UNAUTHORIZED"
    default_message = "Authentication required"


class ForbiddenError(AppError):
    status_code = 403
    code = "FORBIDDEN"
    default_message = "You do not have access to this resource"


class NotFoundError(AppError):
    status_code = 404
    code = "NOT_FOUND"
    default_message = "Resource not found"


class ConflictError(AppError):
    status_code = 409
    code = "CONFLICT"
    default_message = "Request conflicts with the current state of the resource"


class UnprocessableEntityError(AppError):
    """Well-formed request that cannot be acted on (e.g. it references a row
    that does not exist)."""

    status_code = 422
    code = "UNPROCESSABLE_ENTITY"
    default_message = "Request could not be processed"


class RateLimitedError(AppError):
    status_code = 429
    code = "TOO_MANY_REQUESTS"
    default_message = "Too many requests; please retry later"
