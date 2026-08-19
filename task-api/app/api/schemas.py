"""Request and response models.

These are the API's contract and are deliberately separate from the ORM models:
a column can be renamed, added or hidden without changing what clients see, and
no field can leak into a response unless it is declared here. That is why
``hashed_password`` cannot accidentally be serialised.
"""

from datetime import date, datetime, timezone
from typing import Annotated, Any, Optional

from pydantic import (
    AfterValidator,
    BaseModel,
    BeforeValidator,
    ConfigDict,
    EmailStr,
    Field,
    PlainSerializer,
    field_validator,
    model_validator,
)

from app.domain.task_status import DEFAULT_TASK_STATUS, TaskStatus
from app.security.passwords import MAX_PASSWORD_BYTES

MIN_PASSWORD_LENGTH = 12


def _to_iso_utc(value: datetime) -> str:
    return value.astimezone(timezone.utc).isoformat().replace("+00:00", "Z")


#: Every timestamp leaves the API as ISO-8601 UTC with a ``Z`` suffix.
UtcTimestamp = Annotated[datetime, PlainSerializer(_to_iso_utc, return_type=str)]


def _as_utc(value: datetime) -> datetime:
    """A timestamp sent without an offset is interpreted as UTC."""
    return value.replace(tzinfo=timezone.utc) if value.tzinfo is None else value


def _require_iso_string(value: Any) -> Any:
    """Reject input that is not an ISO-8601 string.

    Pydantic's lax mode would otherwise read a bare number as a Unix timestamp,
    so ``"due_date": 0`` would quietly become 1970 instead of failing.
    """
    if isinstance(value, (str, datetime, date)):
        return value
    raise ValueError("must be an ISO-8601 date or date-time string")


#: A due date as the API documents it: an ISO-8601 string, normalised to UTC.
DueDate = Annotated[datetime, BeforeValidator(_require_iso_string), AfterValidator(_as_utc)]


# --------------------------------------------------------------------------- #
# Errors (declared so they appear in the generated OpenAPI document)
# --------------------------------------------------------------------------- #


class ErrorDetail(BaseModel):
    code: str = Field(description="Stable, machine-readable error identifier")
    message: str
    details: Optional[Any] = None


class ErrorResponse(BaseModel):
    error: ErrorDetail
    request_id: str


# --------------------------------------------------------------------------- #
# Auth
# --------------------------------------------------------------------------- #


class RegisterRequest(BaseModel):
    # extra="forbid" makes a typo'd field name a loud 400 rather than a silently
    # ignored one.
    model_config = ConfigDict(extra="forbid")

    email: EmailStr = Field(max_length=254)
    password: str = Field(min_length=MIN_PASSWORD_LENGTH)

    @field_validator("password")
    @classmethod
    def reject_over_long_password(cls, value: str) -> str:
        # bcrypt refuses input beyond 72 bytes; rejecting it here turns what
        # would be a 500 into a clear validation error.
        if len(value.encode("utf-8")) > MAX_PASSWORD_BYTES:
            raise ValueError(f"must be at most {MAX_PASSWORD_BYTES} bytes")
        return value


class LoginRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")

    email: EmailStr = Field(max_length=254)
    # Login only presents a credential; length rules belong to registration.
    password: str = Field(min_length=1)


class UserResponse(BaseModel):
    model_config = ConfigDict(from_attributes=True)

    id: str
    email: str
    created_at: UtcTimestamp


class SessionResponse(BaseModel):
    user: UserResponse
    access_token: str
    token_type: str = "Bearer"
    expires_in: int = Field(description="Access token lifetime in seconds")


# --------------------------------------------------------------------------- #
# Projects
# --------------------------------------------------------------------------- #


class CreateProjectRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")

    name: str = Field(min_length=1, max_length=200)

    @field_validator("name")
    @classmethod
    def reject_blank_name(cls, value: str) -> str:
        if not value.strip():
            raise ValueError("must not be blank")
        return value


class ProjectResponse(BaseModel):
    model_config = ConfigDict(from_attributes=True)

    id: str
    name: str
    owner_id: str
    created_at: UtcTimestamp


# --------------------------------------------------------------------------- #
# Tasks
# --------------------------------------------------------------------------- #


class CreateTaskRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")

    title: str = Field(min_length=1, max_length=200)
    description: Optional[str] = Field(default=None, max_length=5000)
    # Typing the field as the domain enum is what produces the 400 for an
    # invalid status; the database CHECK constraint is the second line of defence.
    # Not Optional: the status column is NOT NULL, so an explicit null is a
    # client error rather than a request for the default.
    status: TaskStatus = DEFAULT_TASK_STATUS
    assignee_id: Optional[str] = None
    due_date: Optional[DueDate] = None

    @field_validator("title")
    @classmethod
    def reject_blank_title(cls, value: str) -> str:
        if not value.strip():
            raise ValueError("must not be blank")
        return value


class UpdateTaskRequest(BaseModel):
    """Partial update.

    Every field is optional, but an empty patch is a client bug rather than a
    no-op worth a 200. ``project_id`` is absent by design: tasks are not
    re-parented through this endpoint.
    """

    model_config = ConfigDict(extra="forbid")

    # `title` and `status` map to NOT NULL columns, so they are annotated
    # non-Optional: a patch may omit them, but may not set them to null. The
    # None default is never observed, because handlers dump with
    # exclude_unset=True and omitted fields therefore never reach the service.
    title: str = Field(default=None, min_length=1, max_length=200)
    status: TaskStatus = None
    # These columns are nullable, so an explicit null means "clear it".
    description: Optional[str] = Field(default=None, max_length=5000)
    assignee_id: Optional[str] = None
    due_date: Optional[DueDate] = None

    @field_validator("title")
    @classmethod
    def reject_blank_title(cls, value: str) -> str:
        if not value.strip():
            raise ValueError("must not be blank")
        return value

    @model_validator(mode="after")
    def require_at_least_one_field(self) -> "UpdateTaskRequest":
        if not self.model_fields_set:
            raise ValueError("at least one field must be provided")
        return self


class TaskResponse(BaseModel):
    model_config = ConfigDict(from_attributes=True)

    id: str
    project_id: str
    title: str
    description: Optional[str]
    status: TaskStatus
    assignee_id: Optional[str]
    due_date: Optional[UtcTimestamp]
    created_at: UtcTimestamp
    updated_at: UtcTimestamp
