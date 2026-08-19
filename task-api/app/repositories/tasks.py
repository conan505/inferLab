"""All persistence for the ``tasks`` table."""

from datetime import datetime
from typing import Any, Mapping, Optional, Tuple

from sqlalchemy import select
from sqlalchemy.orm import Session

from app.db.models import Project, Task, utcnow

#: Fields a PATCH is allowed to touch. ``project_id`` is deliberately absent:
#: re-parenting a task is a different operation with its own authorisation
#: question (you would have to own both projects), so it is out of scope.
UPDATABLE_FIELDS = frozenset({"title", "description", "status", "assignee_id", "due_date"})


class TasksRepository:
    def __init__(self, session: Session) -> None:
        self._session = session

    def create(self, **fields: Any) -> Task:
        task = Task(**fields)
        self._session.add(task)
        self._session.flush()
        return task

    def find_by_id(self, task_id: str) -> Optional[Task]:
        return self._session.get(Task, task_id)

    def find_with_project_owner(self, task_id: str) -> Optional[Tuple[Task, str]]:
        """One round trip answering both "does this task exist?" and "who owns
        the project it lives in?" — which is what every task authorisation needs."""
        row = self._session.execute(
            select(Task, Project.owner_id)
            .join(Project, Task.project_id == Project.id)
            .where(Task.id == task_id)
        ).one_or_none()
        return (row[0], row[1]) if row is not None else None

    def apply_changes(
        self, task: Task, changes: Mapping[str, Any], *, now: Optional[datetime] = None
    ) -> Task:
        """Apply a partial update.

        Attribute names come from :data:`UPDATABLE_FIELDS`, never from request
        input, so a client cannot reach a column the API does not expose.
        """
        for field, value in changes.items():
            if field not in UPDATABLE_FIELDS:
                raise ValueError(f"field {field!r} is not updatable")
            setattr(task, field, value)

        # Set explicitly rather than relying on the column's ``onupdate``: a
        # PATCH that happens to write identical values emits no UPDATE at all,
        # and we still want the write to be recorded.
        task.updated_at = now or utcnow()
        self._session.flush()
        return task
