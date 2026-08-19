"""Request schema behaviour that the endpoints depend on."""

import pytest
from pydantic import ValidationError as PydanticValidationError

from app.api.schemas import (
    CreateProjectRequest,
    CreateTaskRequest,
    LoginRequest,
    RegisterRequest,
    UpdateTaskRequest,
)
from app.security.passwords import MAX_PASSWORD_BYTES


def test_register_normalises_nothing_but_validates_the_email():
    assert RegisterRequest(email="a@example.com", password="a" * 12).email == "a@example.com"


@pytest.mark.parametrize("email", ["not-an-email", "@example.com", "a@", ""])
def test_register_rejects_invalid_emails(email):
    with pytest.raises(PydanticValidationError):
        RegisterRequest(email=email, password="a" * 12)


def test_register_rejects_a_short_password():
    with pytest.raises(PydanticValidationError):
        RegisterRequest(email="a@example.com", password="short")


def test_register_rejects_a_password_bcrypt_cannot_hash():
    with pytest.raises(PydanticValidationError):
        RegisterRequest(email="a@example.com", password="x" * (MAX_PASSWORD_BYTES + 1))


def test_login_does_not_impose_the_registration_length_rule():
    assert LoginRequest(email="a@example.com", password="x").password == "x"


@pytest.mark.parametrize(
    "model, payload",
    [
        (RegisterRequest, {"email": "a@example.com", "password": "a" * 12, "admin": True}),
        (CreateProjectRequest, {"name": "Apollo", "owner_id": "someone-else"}),
        (CreateTaskRequest, {"title": "x", "project_id": "elsewhere"}),
        (UpdateTaskRequest, {"status": "done", "created_at": "2020-01-01T00:00:00Z"}),
    ],
)
def test_unknown_fields_are_rejected(model, payload):
    """A typo'd or injected field must fail loudly rather than be ignored —
    this is also what stops a client setting owner_id or project_id directly."""
    with pytest.raises(PydanticValidationError):
        model(**payload)


@pytest.mark.parametrize("name", ["", "   "])
def test_project_name_cannot_be_blank(name):
    with pytest.raises(PydanticValidationError):
        CreateProjectRequest(name=name)


@pytest.mark.parametrize("title", ["", "   "])
def test_task_title_cannot_be_blank(title):
    with pytest.raises(PydanticValidationError):
        CreateTaskRequest(title=title)


def test_empty_patch_is_rejected():
    with pytest.raises(PydanticValidationError):
        UpdateTaskRequest()


def test_patch_distinguishes_omitted_from_explicit_null():
    omitted = UpdateTaskRequest(status="done").model_dump(exclude_unset=True)
    explicit = UpdateTaskRequest(status="done", assignee_id=None).model_dump(exclude_unset=True)

    assert "assignee_id" not in omitted
    assert explicit["assignee_id"] is None


@pytest.mark.parametrize(
    "value, expected_iso",
    [
        ("2026-03-04", "2026-03-04T00:00:00+00:00"),
        ("2026-03-04T05:06:07Z", "2026-03-04T05:06:07+00:00"),
        # Naive input is interpreted as UTC.
        ("2026-03-04T05:06:07", "2026-03-04T05:06:07+00:00"),
        # An offset is honoured, not ignored.
        ("2026-03-04T05:06:07+02:00", "2026-03-04T05:06:07+02:00"),
    ],
)
def test_due_date_accepts_dates_and_datetimes(value, expected_iso):
    parsed = CreateTaskRequest(title="x", due_date=value).due_date
    assert parsed.isoformat() == expected_iso
    assert parsed.tzinfo is not None


@pytest.mark.parametrize("value", ["yesterday", "2026-13-45", 12345.6789])
def test_due_date_rejects_nonsense(value):
    with pytest.raises(PydanticValidationError):
        CreateTaskRequest(title="x", due_date=value)


def test_create_task_defaults_to_todo_when_status_is_omitted():
    assert CreateTaskRequest(title="x").status.value == "todo"


@pytest.mark.parametrize("field", ["title", "status"])
def test_columns_that_are_not_nullable_reject_an_explicit_null(field):
    """Omitting a field means "leave it alone"; sending null means "make it
    null", which these two columns do not permit."""
    with pytest.raises(PydanticValidationError):
        UpdateTaskRequest(**{field: None})


def test_create_rejects_a_null_status():
    with pytest.raises(PydanticValidationError):
        CreateTaskRequest(title="x", status=None)


@pytest.mark.parametrize("field", ["description", "assignee_id", "due_date"])
def test_nullable_columns_can_be_cleared_with_an_explicit_null(field):
    patch = UpdateTaskRequest(**{field: None})
    dumped = patch.model_dump(exclude_unset=True)
    assert dumped == {field: None}
