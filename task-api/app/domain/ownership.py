"""The authorisation rule for the whole service.

Every project — and every task inside it — is reachable only by the project's
owner. Kept as pure functions over plain rows so the rule is trivially
unit-testable and impossible to apply in one code path but forget in another.
"""

from typing import Optional

from app.errors import ForbiddenError, NotFoundError

_PROJECT_FORBIDDEN = "Only the project owner may perform this action"
_TASK_FORBIDDEN = (
    "Only the owner of the project this task belongs to may perform this action"
)


def assert_project_owned_by(project: Optional[object], user_id: str) -> object:
    """Raise 404 if the project does not exist, 403 if it belongs to someone else."""
    if project is None:
        raise NotFoundError("Project not found", code="PROJECT_NOT_FOUND")
    if project.owner_id != user_id:
        # Deliberately 403, not 404 — see the "403 vs 404" note in the README.
        raise ForbiddenError(_PROJECT_FORBIDDEN, code="NOT_PROJECT_OWNER")
    return project


def assert_task_owned_by(
    task: Optional[object], project_owner_id: Optional[str], user_id: str
) -> object:
    """Same rule for a task, given the owner of the project it belongs to."""
    if task is None:
        raise NotFoundError("Task not found", code="TASK_NOT_FOUND")
    if project_owner_id != user_id:
        raise ForbiddenError(_TASK_FORBIDDEN, code="NOT_PROJECT_OWNER")
    return task
