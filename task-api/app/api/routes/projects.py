"""Project endpoints."""

from uuid import UUID

from fastapi import APIRouter, Depends, Response, status

from app.api.deps import get_current_user, get_projects_service
from app.api.responses import error_responses
from app.api.schemas import CreateProjectRequest, ProjectResponse
from app.db.models import User
from app.services.projects import ProjectsService

router = APIRouter(
    prefix="/projects",
    tags=["projects"],
    # Authentication is attached at the router, not per route, so adding an
    # endpoint here cannot accidentally leave it public. FastAPI caches the
    # dependency within a request, so the handlers below that also declare
    # `current_user` do not pay for it twice.
    dependencies=[Depends(get_current_user)],
)


@router.post(
    "",
    status_code=status.HTTP_201_CREATED,
    response_model=ProjectResponse,
    summary="Create a project owned by the caller",
    responses=error_responses(400, 401, 415),
)
def create_project(
    payload: CreateProjectRequest,
    response: Response,
    current_user: User = Depends(get_current_user),
    projects: ProjectsService = Depends(get_projects_service),
) -> ProjectResponse:
    project = projects.create(owner_id=current_user.id, name=payload.name)
    response.headers["Location"] = f"/projects/{project.id}"
    return ProjectResponse.model_validate(project)


@router.delete(
    "/{project_id}",
    status_code=status.HTTP_204_NO_CONTENT,
    summary="Delete a project the caller owns",
    responses=error_responses(400, 401, 403, 404, 409),
)
def delete_project(
    project_id: UUID,
    current_user: User = Depends(get_current_user),
    projects: ProjectsService = Depends(get_projects_service),
) -> Response:
    projects.delete(project_id=str(project_id), user_id=current_user.id)
    return Response(status_code=status.HTTP_204_NO_CONTENT)
