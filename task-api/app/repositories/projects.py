"""All persistence for the ``projects`` table."""

from typing import Optional, Sequence

from sqlalchemy import delete, func, literal, select
from sqlalchemy.orm import Session

from app.db.models import Project, Task
from app.domain.task_status import TaskStatus


class ProjectsRepository:
    def __init__(self, session: Session) -> None:
        self._session = session

    def create(self, *, name: str, owner_id: str) -> Project:
        project = Project(name=name, owner_id=owner_id)
        self._session.add(project)
        self._session.flush()
        return project

    def find_by_id(self, project_id: str) -> Optional[Project]:
        return self._session.get(Project, project_id)

    def count_tasks_with_status(
        self, project_id: str, statuses: Sequence[TaskStatus]
    ) -> int:
        """How many of the project's tasks are in one of ``statuses``.

        Used only to build the 409 message after a blocked delete — never to
        decide whether the delete is allowed.
        """
        return (
            self._session.scalar(
                select(func.count())
                .select_from(Task)
                .where(Task.project_id == project_id, Task.status.in_(statuses))
            )
            or 0
        )

    def delete_unless_tasks_have_status(
        self, project_id: str, statuses: Sequence[TaskStatus]
    ) -> bool:
        """Delete the project unless it has a task in one of ``statuses``.

        The guard and the delete are expressed as a *single* conditional
        statement rather than a SELECT followed by a DELETE. That closes the
        race where a concurrent PATCH moves a task to ``in_progress`` between
        the check and the delete — which would otherwise cascade that task away
        precisely when the rule says the project must be kept.

        Tasks in the non-blocking statuses are removed by ON DELETE CASCADE.

        :returns: ``True`` if the project was deleted, ``False`` if blocked.
        """
        has_blocking_task = (
            select(literal(1))
            .where(Task.project_id == Project.id, Task.status.in_(statuses))
            .exists()
        )
        result = self._session.execute(
            delete(Project).where(Project.id == project_id, ~has_blocking_task),
            # "fetch" makes SQLAlchemy emit DELETE ... RETURNING id — still one
            # statement, so the guard stays atomic — and evict the deleted row
            # from the identity map, so nothing later in the request can read a
            # project that no longer exists.
            execution_options={"synchronize_session": "fetch"},
        )
        return bool(result.rowcount)
