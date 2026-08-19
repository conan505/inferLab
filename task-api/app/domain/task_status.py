"""The task status enum, defined once.

This module is the single source of truth: the ORM column, the database CHECK
constraint, the request schemas and the project-deletion guard all derive from
it, so the API and the database can never disagree about what a valid status is.
"""

from enum import Enum


class TaskStatus(str, Enum):
    TODO = "todo"
    IN_PROGRESS = "in_progress"
    DONE = "done"


DEFAULT_TASK_STATUS = TaskStatus.TODO

#: Statuses that make a project undeletable. A collection rather than a single
#: constant so the rule can grow without touching the service.
DELETE_BLOCKING_STATUSES = (TaskStatus.IN_PROGRESS,)


def is_valid_task_status(value: object) -> bool:
    return isinstance(value, str) and value in {status.value for status in TaskStatus}
