"""The end-to-end flow the specification calls for.

register -> login -> create project -> create task -> update status ->
attempt (and fail) to delete a project with an in-progress task.
"""

from fastapi.testclient import TestClient

EMAIL = "flow@example.com"
PASSWORD = "correct-horse-battery-staple"


def test_full_task_lifecycle(client: TestClient):
    # --- register ----------------------------------------------------------
    registered = client.post("/auth/register", json={"email": EMAIL, "password": PASSWORD})
    assert registered.status_code == 201, registered.text
    registration = registered.json()
    user_id = registration["user"]["id"]
    assert registration["token_type"] == "Bearer"
    assert registration["expires_in"] > 0
    # The password, in any form, never leaves the service.
    assert "hashed_password" not in registration["user"]
    assert PASSWORD not in registered.text

    # --- login -------------------------------------------------------------
    logged_in = client.post("/auth/login", json={"email": EMAIL, "password": PASSWORD})
    assert logged_in.status_code == 200, logged_in.text
    token = logged_in.json()["access_token"]
    assert logged_in.json()["user"]["id"] == user_id
    auth = {"Authorization": f"Bearer {token}"}

    # --- create a project --------------------------------------------------
    created_project = client.post("/projects", json={"name": "Apollo"}, headers=auth)
    assert created_project.status_code == 201, created_project.text
    project = created_project.json()
    project_id = project["id"]
    # Ownership is assigned from the token, never from the request body.
    assert project["owner_id"] == user_id
    assert created_project.headers["Location"] == f"/projects/{project_id}"

    # --- create a task -----------------------------------------------------
    created_task = client.post(
        f"/projects/{project_id}/tasks",
        json={
            "title": "Wire up the API",
            "description": "Ship the six endpoints",
            "due_date": "2026-12-01",
            "assignee_id": user_id,
        },
        headers=auth,
    )
    assert created_task.status_code == 201, created_task.text
    task = created_task.json()
    task_id = task["id"]
    assert task["project_id"] == project_id
    assert task["status"] == "todo"  # the documented default
    assert task["assignee_id"] == user_id
    assert task["due_date"] == "2026-12-01T00:00:00Z"
    assert task["created_at"] == task["updated_at"]

    # --- move it to in_progress -------------------------------------------
    updated = client.patch(f"/tasks/{task_id}", json={"status": "in_progress"}, headers=auth)
    assert updated.status_code == 200, updated.text
    updated_task = updated.json()
    assert updated_task["status"] == "in_progress"
    # A partial update leaves everything it did not mention alone.
    assert updated_task["title"] == "Wire up the API"
    assert updated_task["description"] == "Ship the six endpoints"
    assert updated_task["updated_at"] >= updated_task["created_at"]

    # --- deleting the project is now a conflict ----------------------------
    blocked = client.delete(f"/projects/{project_id}", headers=auth)
    assert blocked.status_code == 409, blocked.text
    error = blocked.json()["error"]
    assert error["code"] == "PROJECT_HAS_BLOCKING_TASKS"
    assert error["details"]["blocking_task_count"] == 1
    assert "request_id" in blocked.json()

    # --- and the project and its task really are still there ---------------
    still_there = client.patch(f"/tasks/{task_id}", json={"title": "Still here"}, headers=auth)
    assert still_there.status_code == 200
    assert still_there.json()["status"] == "in_progress"

    # --- once nothing is in progress, the delete succeeds ------------------
    finished = client.patch(f"/tasks/{task_id}", json={"status": "done"}, headers=auth)
    assert finished.status_code == 200
    assert finished.json()["status"] == "done"

    deleted = client.delete(f"/projects/{project_id}", headers=auth)
    assert deleted.status_code == 204
    assert deleted.content == b""

    # --- the project is gone, and so is its task ---------------------------
    assert client.delete(f"/projects/{project_id}", headers=auth).status_code == 404
    orphaned = client.patch(f"/tasks/{task_id}", json={"status": "todo"}, headers=auth)
    assert orphaned.status_code == 404
    assert orphaned.json()["error"]["code"] == "TASK_NOT_FOUND"
