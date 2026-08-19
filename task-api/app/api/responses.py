"""Shared construction of the error envelope.

Every error the service emits — from an exception handler or from middleware —
goes through here, so clients see exactly one error shape.
"""

from typing import Any, Mapping, Optional

from fastapi.responses import JSONResponse


def build_error_payload(
    *, code: str, message: str, request_id: str, details: Any = None
) -> dict:
    error: dict = {"code": code, "message": message}
    if details is not None:
        error["details"] = details
    return {"error": error, "request_id": request_id}


def json_error_response(
    *,
    status_code: int,
    code: str,
    message: str,
    request_id: str,
    details: Any = None,
    headers: Optional[Mapping[str, str]] = None,
) -> JSONResponse:
    response_headers = dict(headers or {})
    # RFC 9110: a 401 must tell the client how to authenticate.
    if status_code == 401:
        response_headers.setdefault("WWW-Authenticate", "Bearer")
    return JSONResponse(
        status_code=status_code,
        content=build_error_payload(
            code=code, message=message, request_id=request_id, details=details
        ),
        headers=response_headers,
    )


_ERROR_DESCRIPTIONS = {
    400: "Malformed or invalid request",
    401: "Missing, malformed, invalid or expired access token",
    403: "Authenticated, but not the owner of the resource",
    404: "Resource does not exist",
    409: "Conflicts with the current state of the resource",
    415: "Request body is not application/json",
    422: "Well-formed but references something that does not exist",
    429: "Rate limit exceeded",
}


def error_responses(*status_codes: int) -> dict:
    """Declare error shapes on a route so they appear in the OpenAPI document."""
    from app.api.schemas import ErrorResponse

    return {
        status_code: {
            "model": ErrorResponse,
            "description": _ERROR_DESCRIPTIONS.get(status_code, "Error"),
        }
        for status_code in status_codes
    }
