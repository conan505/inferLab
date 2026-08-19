"""Task endpoints.

Creation is project-scoped (``/projects/{project_id}/tasks``) because a task
cannot exist without a project; update addresses the task directly, since by
then it has its own identity.
"""

from uuid import UUID

from fastapi import APIRouter, Depends, Response, status

from app.api.deps import get_current_user, get_tasks_service
from app.api.responses import error_responses
from app.api.schemas import CreateTaskRequest, TaskResponse, UpdateTaskRequest
from app.db.models import User
from app.services.tasks import TasksService

project_tasks_router = APIRouter(
    prefix="/projects/{project_id}/tasks",
    tags=["tasks"],
    dependencies=[Depends(get_current_user)],
)

tasks_router = APIRouter(
    prefix="/tasks",
    tags=["tasks"],
    dependencies=[Depends(get_current_user)],
)


@project_tasks_router.post(
    "",
    status_code=status.HTTP_201_CREATED,
    response_model=TaskResponse,
    summary="Create a task in a project the caller owns",
    responses=error_responses(400, 401, 403, 404, 415, 422),
)
def create_task(
    project_id: UUID,
    payload: CreateTaskRequest,
    response: Response,
    current_user: User = Depends(get_current_user),
    tasks: TasksService = Depends(get_tasks_service),
) -> TaskResponse:
    task = tasks.create(
        project_id=str(project_id),
        user_id=current_user.id,
        # exclude_unset keeps "field omitted" distinct from "field sent as null".
        data=payload.model_dump(exclude_unset=True),
    )
    response.headers["Location"] = f"/tasks/{task.id}"
    return TaskResponse.model_validate(task)


@tasks_router.patch(
    "/{task_id}",
    response_model=TaskResponse,
    summary="Partially update a task in a project the caller owns",
    responses=error_responses(400, 401, 403, 404, 415, 422),
)
def update_task(
    task_id: UUID,
    payload: UpdateTaskRequest,
    current_user: User = Depends(get_current_user),
    tasks: TasksService = Depends(get_tasks_service),
) -> TaskResponse:
    task = tasks.update(
        task_id=str(task_id),
        user_id=current_user.id,
        changes=payload.model_dump(exclude_unset=True),
    )
    return TaskResponse.model_validate(task)
