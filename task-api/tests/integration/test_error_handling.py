"""HTTP status codes, error bodies and edge cases."""

from datetime import datetime, timedelta, timezone

import jwt
import pytest
from fastapi.testclient import TestClient

from tests.conftest import DEFAULT_PASSWORD, TEST_SECRET

MISSING_ID = "00000000-0000-4000-8000-000000000000"


def assert_error_envelope(response, *, status: int, code: str):
    assert response.status_code == status, response.text
    body = response.json()
    assert set(body) >= {"error", "request_id"}
    assert body["error"]["code"] == code
    assert isinstance(body["error"]["message"], str) and body["error"]["message"]
    return body


# --------------------------------------------------------------------------- #
# Authentication
# --------------------------------------------------------------------------- #

PROTECTED_REQUESTS = [
    ("post", "/projects", {"name": "Apollo"}),
    ("post", f"/projects/{MISSING_ID}/tasks", {"title": "x"}),
    ("patch", f"/tasks/{MISSING_ID}", {"status": "done"}),
    ("delete", f"/projects/{MISSING_ID}", None),
]


@pytest.mark.parametrize("method, path, payload", PROTECTED_REQUESTS)
def test_every_endpoint_except_auth_requires_a_token(client, method, path, payload):
    response = getattr(client, method)(path, json=payload) if payload else getattr(client, method)(path)

    assert_error_envelope(response, status=401, code="MISSING_TOKEN")
    # RFC 9110: a 401 must say how to authenticate.
    assert response.headers["WWW-Authenticate"] == "Bearer"


@pytest.mark.parametrize(
    "header",
    ["Bearer", "Token abc", "abc", "Basic dXNlcjpwYXNz"],
)
def test_a_malformed_authorization_header_is_rejected(client, header):
    response = client.post(
        "/projects", json={"name": "Apollo"}, headers={"Authorization": header}
    )
    assert_error_envelope(response, status=401, code="MALFORMED_AUTH_HEADER")


@pytest.mark.parametrize("token", ["garbage", "a.b.c", "", "null", "a b"])
def test_an_invalid_token_is_rejected(client, token):
    response = client.post(
        "/projects", json={"name": "Apollo"}, headers={"Authorization": f"Bearer {token}"}
    )
    assert response.status_code == 401
    assert response.json()["error"]["code"] in {"INVALID_TOKEN", "MALFORMED_AUTH_HEADER"}


def test_an_expired_token_is_rejected_with_its_own_code(client, alice):
    expired = jwt.encode(
        {
            "sub": alice.id,
            "email": alice.email,
            "iss": "task-api",
            "iat": datetime.now(timezone.utc) - timedelta(hours=2),
            "exp": datetime.now(timezone.utc) - timedelta(hours=1),
        },
        TEST_SECRET,
        algorithm="HS256",
    )

    response = client.post(
        "/projects", json={"name": "Apollo"}, headers={"Authorization": f"Bearer {expired}"}
    )
    assert_error_envelope(response, status=401, code="TOKEN_EXPIRED")


def test_a_token_for_an_unknown_user_is_rejected(client):
    """Identity is re-resolved from the database on every request, so a
    correctly signed token for a deleted account is worthless."""
    orphaned = jwt.encode(
        {
            "sub": MISSING_ID,
            "email": "ghost@example.com",
            "iss": "task-api",
            "iat": datetime.now(timezone.utc),
            "exp": datetime.now(timezone.utc) + timedelta(minutes=15),
        },
        TEST_SECRET,
        algorithm="HS256",
    )

    response = client.post(
        "/projects", json={"name": "Apollo"}, headers={"Authorization": f"Bearer {orphaned}"}
    )
    assert_error_envelope(response, status=401, code="INVALID_TOKEN")


# --------------------------------------------------------------------------- #
# Registration and login
# --------------------------------------------------------------------------- #


def test_registering_the_same_email_twice_is_a_conflict(client, alice):
    response = client.post(
        "/auth/register", json={"email": alice.email, "password": DEFAULT_PASSWORD}
    )
    assert_error_envelope(response, status=409, code="EMAIL_ALREADY_REGISTERED")


def test_wrong_password_and_unknown_email_are_indistinguishable(client, alice):
    wrong_password = client.post(
        "/auth/login", json={"email": alice.email, "password": "not-the-password"}
    )
    unknown_email = client.post(
        "/auth/login", json={"email": "nobody@example.com", "password": DEFAULT_PASSWORD}
    )

    assert wrong_password.status_code == unknown_email.status_code == 401
    # Identical code and message: the response must not reveal which accounts exist.
    assert wrong_password.json()["error"] == unknown_email.json()["error"]
    assert wrong_password.json()["error"]["code"] == "INVALID_CREDENTIALS"


@pytest.mark.parametrize(
    "payload",
    [
        {"email": "not-an-email", "password": DEFAULT_PASSWORD},
        {"email": "a@example.com", "password": "short"},
        {"email": "a@example.com"},
        {"password": DEFAULT_PASSWORD},
        {},
    ],
)
def test_invalid_registration_payloads_are_rejected(client, payload):
    response = client.post("/auth/register", json=payload)
    assert_error_envelope(response, status=400, code="VALIDATION_ERROR")


# --------------------------------------------------------------------------- #
# Request bodies
# --------------------------------------------------------------------------- #


def test_malformed_json_is_reported_as_such(client):
    response = client.post(
        "/auth/login",
        content='{"email": "a@example.com", ',
        headers={"Content-Type": "application/json"},
    )
    assert_error_envelope(response, status=400, code="MALFORMED_JSON")


def test_a_non_json_body_is_unsupported_media_type(client):
    response = client.post(
        "/auth/login",
        content="email=a@example.com&password=x",
        headers={"Content-Type": "application/x-www-form-urlencoded"},
    )
    assert_error_envelope(response, status=415, code="UNSUPPORTED_MEDIA_TYPE")


@pytest.mark.parametrize("status", ["archived", "TODO", "in-progress", "", None, 5])
def test_an_undefined_task_status_is_rejected(client, alice, status):
    project_id = client.post(
        "/projects", json={"name": "Apollo"}, headers=alice.headers
    ).json()["id"]

    response = client.post(
        f"/projects/{project_id}/tasks",
        json={"title": "x", "status": status},
        headers=alice.headers,
    )

    body = assert_error_envelope(response, status=400, code="VALIDATION_ERROR")
    assert any(detail["field"] == "status" for detail in body["error"]["details"])


def test_an_empty_patch_is_rejected(client, alice):
    project_id = client.post(
        "/projects", json={"name": "Apollo"}, headers=alice.headers
    ).json()["id"]
    task_id = client.post(
        f"/projects/{project_id}/tasks", json={"title": "x"}, headers=alice.headers
    ).json()["id"]

    response = client.patch(f"/tasks/{task_id}", json={}, headers=alice.headers)
    assert_error_envelope(response, status=400, code="VALIDATION_ERROR")


def test_assigning_to_an_unknown_user_is_unprocessable(client, alice):
    project_id = client.post(
        "/projects", json={"name": "Apollo"}, headers=alice.headers
    ).json()["id"]

    response = client.post(
        f"/projects/{project_id}/tasks",
        json={"title": "x", "assignee_id": MISSING_ID},
        headers=alice.headers,
    )
    assert_error_envelope(response, status=422, code="ASSIGNEE_NOT_FOUND")


def test_a_malformed_id_in_the_path_is_a_bad_request(client, alice):
    response = client.delete("/projects/not-a-uuid", headers=alice.headers)
    body = assert_error_envelope(response, status=400, code="VALIDATION_ERROR")
    assert body["error"]["details"][0]["source"] == "path"


def test_a_task_that_does_not_exist_is_not_found(client, alice):
    response = client.patch(
        f"/tasks/{MISSING_ID}", json={"status": "done"}, headers=alice.headers
    )
    assert_error_envelope(response, status=404, code="TASK_NOT_FOUND")


# --------------------------------------------------------------------------- #
# Routing and transport
# --------------------------------------------------------------------------- #


def test_an_unknown_route_returns_a_structured_404(client):
    assert_error_envelope(client.get("/nope"), status=404, code="ROUTE_NOT_FOUND")


def test_an_unsupported_method_returns_a_structured_405(client, alice):
    assert_error_envelope(
        client.get("/projects", headers=alice.headers), status=405, code="METHOD_NOT_ALLOWED"
    )


def test_every_response_carries_a_request_id_and_hardening_headers(client):
    response = client.get("/health")

    assert response.status_code == 200
    assert response.headers["X-Request-Id"]
    assert response.headers["X-Content-Type-Options"] == "nosniff"
    assert response.headers["X-Frame-Options"] == "DENY"


def test_a_supplied_request_id_is_echoed_back_for_correlation(client):
    response = client.get("/health", headers={"X-Request-Id": "trace-me-123"})
    assert response.headers["X-Request-Id"] == "trace-me-123"


def test_error_bodies_carry_the_same_request_id(client):
    response = client.get("/nope", headers={"X-Request-Id": "trace-me-456"})
    assert response.json()["request_id"] == "trace-me-456"
