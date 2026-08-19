"""Project lifecycle, including the deletion guard."""

from app.db.models import Project
from app.domain.ownership import assert_project_owned_by
from app.domain.task_status import DELETE_BLOCKING_STATUSES
from app.errors import ConflictError
from app.repositories.projects import ProjectsRepository


class ProjectsService:
    def __init__(self, *, projects: ProjectsRepository) -> None:
        self._projects = projects

    def create(self, *, owner_id: str, name: str) -> Project:
        return self._projects.create(name=name.strip(), owner_id=owner_id)

    def get_owned(self, *, project_id: str, user_id: str) -> Project:
        """Load a project and enforce ownership in one step.

        Every caller that needs a project goes through here, which is why the
        ownership rule cannot be skipped by adding a new endpoint.
        """
        return assert_project_owned_by(self._projects.find_by_id(project_id), user_id)

    def delete(self, *, project_id: str, user_id: str) -> None:
        """Business rule: a project with ``in_progress`` tasks cannot be deleted.

        Ownership is checked first so a non-owner gets 403/404 rather than
        learning, via a 409, that the project exists and is busy.
        """
        self.get_owned(project_id=project_id, user_id=user_id)

        deleted = self._projects.delete_unless_tasks_have_status(
            project_id, DELETE_BLOCKING_STATUSES
        )
        if deleted:
            return

        blocking_count = self._projects.count_tasks_with_status(
            project_id, DELETE_BLOCKING_STATUSES
        )
        blocking_statuses = ", ".join(status.value for status in DELETE_BLOCKING_STATUSES)
        raise ConflictError(
            f"Project cannot be deleted while {blocking_count} of its task(s) "
            f"have status: {blocking_statuses}",
            code="PROJECT_HAS_BLOCKING_TASKS",
            details={"blocking_statuses": [s.value for s in DELETE_BLOCKING_STATUSES],
                     "blocking_task_count": blocking_count},
        )
