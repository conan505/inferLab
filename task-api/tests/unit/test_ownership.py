"""The ownership rule, tested in isolation from HTTP and the database."""

from types import SimpleNamespace

import pytest

from app.domain.ownership import assert_project_owned_by, assert_task_owned_by
from app.errors import ForbiddenError, NotFoundError

OWNER = "user-1"
OTHER = "user-2"


def a_project(owner_id=OWNER):
    return SimpleNamespace(id="project-1", owner_id=owner_id)


def test_owner_may_act_on_their_project():
    project = a_project()
    assert assert_project_owned_by(project, OWNER) is project


def test_missing_project_is_not_found():
    with pytest.raises(NotFoundError) as raised:
        assert_project_owned_by(None, OWNER)
    assert raised.value.status_code == 404
    assert raised.value.code == "PROJECT_NOT_FOUND"


def test_someone_elses_project_is_forbidden():
    with pytest.raises(ForbiddenError) as raised:
        assert_project_owned_by(a_project(), OTHER)
    assert raised.value.status_code == 403
    assert raised.value.code == "NOT_PROJECT_OWNER"


def test_owner_may_act_on_a_task_in_their_project():
    task = SimpleNamespace(id="task-1")
    assert assert_task_owned_by(task, OWNER, OWNER) is task


def test_missing_task_is_not_found():
    with pytest.raises(NotFoundError) as raised:
        assert_task_owned_by(None, None, OWNER)
    assert raised.value.code == "TASK_NOT_FOUND"


def test_task_in_someone_elses_project_is_forbidden():
    with pytest.raises(ForbiddenError) as raised:
        assert_task_owned_by(SimpleNamespace(id="task-1"), OWNER, OTHER)
    assert raised.value.code == "NOT_PROJECT_OWNER"
