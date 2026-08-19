"""Task creation and update rules, against a real in-memory database."""

import pytest

from app.db.models import Project, Task, User
from app.domain.task_status import TaskStatus
from app.errors import ForbiddenError, NotFoundError, UnprocessableEntityError
from app.repositories.projects import ProjectsRepository
from app.repositories.tasks import TasksRepository
from app.repositories.users import UsersRepository
from app.services.projects import ProjectsService
from app.services.tasks import TasksService

MISSING_ID = "00000000-0000-4000-8000-000000000000"


@pytest.fixture
def session(session_factory):
    with session_factory() as session:
        yield session


@pytest.fixture
def users(session):
    def _create(email: str) -> User:
        user = User(email=email, hashed_password="x")
        session.add(user)
        session.flush()
        return user

    return _create


@pytest.fixture
def owner(users):
    return users("owner@example.com")


@pytest.fixture
def stranger(users):
    return users("stranger@example.com")


@pytest.fixture
def project(session, owner):
    project = Project(name="Apollo", owner_id=owner.id)
    session.add(project)
    session.flush()
    return project


@pytest.fixture
def service(session):
    projects_repository = ProjectsRepository(session)
    return TasksService(
        tasks=TasksRepository(session),
        users=UsersRepository(session),
        projects_service=ProjectsService(projects=projects_repository),
    )


def test_create_defaults_to_todo(service, project, owner):
    task = service.create(
        project_id=project.id, user_id=owner.id, data={"title": "  write tests  "}
    )
    assert task.status is TaskStatus.TODO
    assert task.title == "write tests"
    assert task.project_id == project.id
    assert task.assignee_id is None
    assert task.created_at == task.updated_at


def test_create_honours_an_explicit_status(service, project, owner):
    task = service.create(
        project_id=project.id,
        user_id=owner.id,
        data={"title": "ship it", "status": TaskStatus.IN_PROGRESS},
    )
    assert task.status is TaskStatus.IN_PROGRESS


def test_cannot_create_a_task_in_someone_elses_project(service, project, stranger):
    with pytest.raises(ForbiddenError) as raised:
        service.create(
            project_id=project.id, user_id=stranger.id, data={"title": "sneaky"}
        )
    assert raised.value.code == "NOT_PROJECT_OWNER"


def test_cannot_create_a_task_in_a_project_that_does_not_exist(service, owner):
    with pytest.raises(NotFoundError):
        service.create(project_id=MISSING_ID, user_id=owner.id, data={"title": "ghost"})


def test_assignee_must_be_a_registered_user(service, project, owner):
    with pytest.raises(UnprocessableEntityError) as raised:
        service.create(
            project_id=project.id,
            user_id=owner.id,
            data={"title": "assign me", "assignee_id": MISSING_ID},
        )
    assert raised.value.status_code == 422
    assert raised.value.code == "ASSIGNEE_NOT_FOUND"


def test_any_registered_user_may_be_assigned(service, project, owner, stranger):
    task = service.create(
        project_id=project.id,
        user_id=owner.id,
        data={"title": "collaborate", "assignee_id": stranger.id},
    )
    assert task.assignee_id == stranger.id


def test_update_changes_status_and_bumps_updated_at(service, project, owner):
    task = service.create(project_id=project.id, user_id=owner.id, data={"title": "x"})
    created_at, first_updated_at = task.created_at, task.updated_at

    updated = service.update(
        task_id=task.id, user_id=owner.id, changes={"status": TaskStatus.DONE}
    )

    assert updated.status is TaskStatus.DONE
    assert updated.created_at == created_at
    assert updated.updated_at >= first_updated_at


def test_update_only_touches_the_fields_provided(service, project, owner):
    task = service.create(
        project_id=project.id,
        user_id=owner.id,
        data={"title": "original", "description": "keep me"},
    )

    updated = service.update(
        task_id=task.id, user_id=owner.id, changes={"status": TaskStatus.IN_PROGRESS}
    )

    assert updated.title == "original"
    assert updated.description == "keep me"


def test_update_can_clear_a_nullable_field(service, project, owner, stranger):
    task = service.create(
        project_id=project.id,
        user_id=owner.id,
        data={"title": "x", "assignee_id": stranger.id},
    )

    updated = service.update(task_id=task.id, user_id=owner.id, changes={"assignee_id": None})

    assert updated.assignee_id is None


def test_cannot_update_a_task_in_someone_elses_project(service, project, owner, stranger):
    task = service.create(project_id=project.id, user_id=owner.id, data={"title": "x"})

    with pytest.raises(ForbiddenError) as raised:
        service.update(task_id=task.id, user_id=stranger.id, changes={"status": TaskStatus.DONE})
    assert raised.value.code == "NOT_PROJECT_OWNER"


def test_updating_a_task_that_does_not_exist_is_not_found(service, owner):
    with pytest.raises(NotFoundError) as raised:
        service.update(task_id=MISSING_ID, user_id=owner.id, changes={"status": TaskStatus.DONE})
    assert raised.value.code == "TASK_NOT_FOUND"


def test_repository_refuses_to_write_a_field_the_api_does_not_expose(session, project, owner):
    """Defence in depth: even if a schema were loosened by mistake, the
    repository will not write an arbitrary column."""
    task = Task(project_id=project.id, title="x")
    session.add(task)
    session.flush()

    with pytest.raises(ValueError):
        TasksRepository(session).apply_changes(task, {"project_id": MISSING_ID})
