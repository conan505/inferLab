"""Task creation and partial update."""

from typing import Any, Mapping, Optional

from app.db.models import Task, utcnow
from app.domain.ownership import assert_task_owned_by
from app.domain.task_status import DEFAULT_TASK_STATUS
from app.errors import UnprocessableEntityError
from app.repositories.tasks import TasksRepository
from app.repositories.users import UsersRepository
from app.services.projects import ProjectsService


class TasksService:
    def __init__(
        self,
        *,
        tasks: TasksRepository,
        users: UsersRepository,
        projects_service: ProjectsService,
    ) -> None:
        self._tasks = tasks
        self._users = users
        self._projects = projects_service

    def _assert_assignee_exists(self, assignee_id: Optional[str]) -> None:
        """The domain has no project-membership concept, so any registered user
        is a valid assignee. The check exists so a mistyped id fails as a clear
        422 instead of an opaque foreign-key error at flush time."""
        if assignee_id is None:
            return
        if not self._users.exists(assignee_id):
            raise UnprocessableEntityError(
                "The assignee does not correspond to a registered user",
                code="ASSIGNEE_NOT_FOUND",
            )

    def create(self, *, project_id: str, user_id: str, data: Mapping[str, Any]) -> Task:
        # Creating a task requires ownership of the containing project.
        self._projects.get_owned(project_id=project_id, user_id=user_id)
        self._assert_assignee_exists(data.get("assignee_id"))

        # One timestamp for both columns: a freshly created task must report
        # created_at == updated_at, which two separate column defaults would not
        # guarantee.
        created_at = utcnow()
        return self._tasks.create(
            project_id=project_id,
            title=data["title"].strip(),
            description=data.get("description"),
            status=data.get("status") or DEFAULT_TASK_STATUS,
            assignee_id=data.get("assignee_id"),
            due_date=data.get("due_date"),
            created_at=created_at,
            updated_at=created_at,
        )

    def update(self, *, task_id: str, user_id: str, changes: Mapping[str, Any]) -> Task:
        found = self._tasks.find_with_project_owner(task_id)
        task, project_owner_id = found if found is not None else (None, None)
        assert_task_owned_by(task, project_owner_id, user_id)

        if "assignee_id" in changes:
            self._assert_assignee_exists(changes["assignee_id"])
        if "title" in changes:
            changes = {**changes, "title": changes["title"].strip()}

        return self._tasks.apply_changes(task, changes)
