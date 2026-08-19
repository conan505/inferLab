"""Cross-cutting request handling."""

import uuid

from starlette.middleware.base import BaseHTTPMiddleware
from starlette.requests import Request

from app.api.responses import json_error_response

_METHODS_WITH_BODY = {"POST", "PUT", "PATCH"}

#: The few parts of a `helmet`-style header set that actually apply to a JSON
#: API, inlined rather than pulling in a dependency.
_SECURITY_HEADERS = {
    "X-Content-Type-Options": "nosniff",
    "X-Frame-Options": "DENY",
    "Referrer-Policy": "no-referrer",
}


class RequestContextMiddleware(BaseHTTPMiddleware):
    """Give every request a correlation id and add hardening headers.

    The id is echoed in ``X-Request-Id`` and included in every error body, so a
    user-reported failure can be traced to a specific log line.
    """

    async def dispatch(self, request: Request, call_next):
        request_id = request.headers.get("x-request-id") or str(uuid.uuid4())
        request.state.request_id = request_id

        response = await call_next(request)
        response.headers["X-Request-Id"] = request_id
        for header, value in _SECURITY_HEADERS.items():
            response.headers.setdefault(header, value)
        return response


class RequireJsonBodyMiddleware(BaseHTTPMiddleware):
    """Reject a request that carries a body in something other than JSON.

    Without this, the body is parsed as JSON regardless of what the client
    declared, and a form-encoded POST surfaces as a confusing "field required"
    validation error instead of an accurate 415.
    """

    async def dispatch(self, request: Request, call_next):
        if request.method in _METHODS_WITH_BODY and self._has_body(request):
            content_type = request.headers.get("content-type", "")
            media_type = content_type.split(";", 1)[0].strip().lower()
            if media_type != "application/json":
                return json_error_response(
                    status_code=415,
                    code="UNSUPPORTED_MEDIA_TYPE",
                    message="Request body must be sent as application/json",
                    request_id=getattr(request.state, "request_id", "unknown"),
                )
        return await call_next(request)

    @staticmethod
    def _has_body(request: Request) -> bool:
        if "transfer-encoding" in request.headers:
            return True
        content_length = request.headers.get("content-length")
        return content_length is not None and content_length != "0"
