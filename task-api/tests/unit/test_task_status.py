"""The status enum is the contract between the API, the service and the schema."""

import pytest
from pydantic import ValidationError as PydanticValidationError

from app.api.schemas import CreateTaskRequest, UpdateTaskRequest
from app.domain.task_status import (
    DEFAULT_TASK_STATUS,
    DELETE_BLOCKING_STATUSES,
    TaskStatus,
    is_valid_task_status,
)


def test_exactly_three_statuses_are_defined():
    assert [status.value for status in TaskStatus] == ["todo", "in_progress", "done"]


def test_default_status_is_todo():
    assert DEFAULT_TASK_STATUS is TaskStatus.TODO


@pytest.mark.parametrize("value", ["todo", "in_progress", "done"])
def test_defined_values_are_valid(value):
    assert is_valid_task_status(value)


@pytest.mark.parametrize(
    "value",
    ["TODO", "in progress", "in-progress", "blocked", "", None, 1, ["todo"]],
)
def test_everything_else_is_invalid(value):
    assert not is_valid_task_status(value)


def test_only_in_progress_blocks_project_deletion():
    assert DELETE_BLOCKING_STATUSES == (TaskStatus.IN_PROGRESS,)


@pytest.mark.parametrize("value", ["todo", "in_progress", "done"])
def test_request_schema_accepts_each_defined_status(value):
    assert CreateTaskRequest(title="write tests", status=value).status.value == value


@pytest.mark.parametrize("value", ["archived", "TODO", "in-progress", 3])
def test_request_schema_rejects_undefined_status(value):
    with pytest.raises(PydanticValidationError):
        CreateTaskRequest(title="write tests", status=value)

    with pytest.raises(PydanticValidationError):
        UpdateTaskRequest(status=value)
