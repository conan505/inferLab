"""The project-deletion rule, exercised against a real (in-memory) database.

These run through the repositories rather than fakes, because the rule is
partly expressed in SQL — a fake repository would test the mock, not the rule.
"""

import pytest

from app.db.models import Project, Task, User
from app.domain.task_status import TaskStatus
from app.errors import ConflictError, ForbiddenError, NotFoundError
from app.repositories.projects import ProjectsRepository
from app.services.projects import ProjectsService

MISSING_ID = "00000000-0000-4000-8000-000000000000"


@pytest.fixture
def session(session_factory):
    with session_factory() as session:
        yield session


@pytest.fixture
def service(session):
    return ProjectsService(projects=ProjectsRepository(session))


@pytest.fixture
def owner(session):
    user = User(email="owner@example.com", hashed_password="x")
    session.add(user)
    session.flush()
    return user


@pytest.fixture
def project(session, owner):
    project = Project(name="Apollo", owner_id=owner.id)
    session.add(project)
    session.flush()
    return project


def add_task(session, project, status: TaskStatus, title="a task"):
    task = Task(project_id=project.id, title=title, status=status)
    session.add(task)
    session.flush()
    return task


def test_create_assigns_ownership_to_the_caller(service, owner):
    project = service.create(owner_id=owner.id, name="  Gemini  ")
    assert project.owner_id == owner.id
    # Names are stored trimmed.
    assert project.name == "Gemini"


def test_empty_project_is_deleted(service, session, project):
    service.delete(project_id=project.id, user_id=project.owner_id)
    assert session.get(Project, project.id) is None


@pytest.mark.parametrize("status", [TaskStatus.TODO, TaskStatus.DONE])
def test_project_with_only_non_blocking_tasks_is_deleted(service, session, project, status):
    task_id = add_task(session, project, status).id

    service.delete(project_id=project.id, user_id=project.owner_id)

    assert session.get(Project, project.id) is None
    # ON DELETE CASCADE removes the project's tasks with it. Detach first so the
    # assertion reads the database rather than the session's identity map.
    session.expunge_all()
    assert session.get(Task, task_id) is None


def test_project_with_an_in_progress_task_cannot_be_deleted(service, session, project):
    task = add_task(session, project, TaskStatus.IN_PROGRESS)

    with pytest.raises(ConflictError) as raised:
        service.delete(project_id=project.id, user_id=project.owner_id)

    assert raised.value.status_code == 409
    assert raised.value.code == "PROJECT_HAS_BLOCKING_TASKS"
    assert raised.value.details["blocking_task_count"] == 1
    # Nothing was removed.
    assert session.get(Project, project.id) is not None
    assert session.get(Task, task.id) is not None


def test_one_in_progress_task_blocks_deletion_even_among_many(service, session, project):
    add_task(session, project, TaskStatus.DONE, "shipped")
    add_task(session, project, TaskStatus.TODO, "later")
    add_task(session, project, TaskStatus.IN_PROGRESS, "in flight")

    with pytest.raises(ConflictError):
        service.delete(project_id=project.id, user_id=project.owner_id)

    assert session.get(Project, project.id) is not None


def test_deletion_succeeds_once_the_blocking_task_moves_on(service, session, project):
    task = add_task(session, project, TaskStatus.IN_PROGRESS)
    with pytest.raises(ConflictError):
        service.delete(project_id=project.id, user_id=project.owner_id)

    task.status = TaskStatus.DONE
    session.flush()

    service.delete(project_id=project.id, user_id=project.owner_id)
    assert session.get(Project, project.id) is None


def test_non_owner_cannot_delete(service, session, project):
    with pytest.raises(ForbiddenError) as raised:
        service.delete(project_id=project.id, user_id="somebody-else")

    assert raised.value.code == "NOT_PROJECT_OWNER"
    assert session.get(Project, project.id) is not None


def test_non_owner_of_a_busy_project_gets_403_not_409(service, session, project):
    """Ownership is checked first, so a stranger cannot learn from the status
    code that the project exists and is busy."""
    add_task(session, project, TaskStatus.IN_PROGRESS)

    with pytest.raises(ForbiddenError):
        service.delete(project_id=project.id, user_id="somebody-else")


def test_deleting_a_missing_project_is_not_found(service, owner):
    with pytest.raises(NotFoundError) as raised:
        service.delete(project_id=MISSING_ID, user_id=owner.id)
    assert raised.value.code == "PROJECT_NOT_FOUND"


def test_get_owned_rejects_other_peoples_projects(service, project):
    with pytest.raises(ForbiddenError):
        service.get_owned(project_id=project.id, user_id="somebody-else")
