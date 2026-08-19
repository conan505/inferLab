"""Cross-user isolation: a project, and everything in it, belongs to its owner."""

from fastapi.testclient import TestClient

MISSING_ID = "00000000-0000-4000-8000-000000000000"


def make_project(client: TestClient, user, name="Apollo") -> str:
    response = client.post("/projects", json={"name": name}, headers=user.headers)
    assert response.status_code == 201, response.text
    return response.json()["id"]


def make_task(client: TestClient, user, project_id: str, **overrides) -> dict:
    payload = {"title": "a task", **overrides}
    response = client.post(
        f"/projects/{project_id}/tasks", json=payload, headers=user.headers
    )
    assert response.status_code == 201, response.text
    return response.json()


def test_a_stranger_cannot_create_a_task_in_your_project(client, alice, bob):
    project_id = make_project(client, alice)

    response = client.post(
        f"/projects/{project_id}/tasks", json={"title": "sneaky"}, headers=bob.headers
    )

    assert response.status_code == 403
    assert response.json()["error"]["code"] == "NOT_PROJECT_OWNER"


def test_a_stranger_cannot_update_your_task(client, alice, bob):
    project_id = make_project(client, alice)
    task = make_task(client, alice, project_id)

    response = client.patch(
        f"/tasks/{task['id']}", json={"status": "done"}, headers=bob.headers
    )

    assert response.status_code == 403
    assert response.json()["error"]["code"] == "NOT_PROJECT_OWNER"
    # And the task is untouched.
    unchanged = client.patch(
        f"/tasks/{task['id']}", json={"status": "todo"}, headers=alice.headers
    )
    assert unchanged.json()["status"] == "todo"


def test_a_stranger_cannot_delete_your_project(client, alice, bob):
    project_id = make_project(client, alice)

    response = client.delete(f"/projects/{project_id}", headers=bob.headers)

    assert response.status_code == 403
    assert response.json()["error"]["code"] == "NOT_PROJECT_OWNER"
    # The project survives: its owner can still use it.
    assert (
        client.post(
            f"/projects/{project_id}/tasks", json={"title": "still mine"}, headers=alice.headers
        ).status_code
        == 201
    )


def test_ownership_is_checked_before_the_in_progress_rule(client, alice, bob):
    """A stranger gets 403, not 409 — the conflict would reveal that the project
    exists and has work in flight."""
    project_id = make_project(client, alice)
    make_task(client, alice, project_id, status="in_progress")

    response = client.delete(f"/projects/{project_id}", headers=bob.headers)

    assert response.status_code == 403


def test_owner_id_in_the_request_body_is_rejected_not_honoured(client, alice, bob):
    response = client.post(
        "/projects", json={"name": "Apollo", "owner_id": bob.id}, headers=alice.headers
    )
    assert response.status_code == 400
    assert response.json()["error"]["code"] == "VALIDATION_ERROR"


def test_a_task_may_be_assigned_to_another_user(client, alice, bob):
    """Assignment is not authorisation: Bob can be assigned work in Alice's
    project, but that does not let him touch it."""
    project_id = make_project(client, alice)
    task = make_task(client, alice, project_id, assignee_id=bob.id)

    assert task["assignee_id"] == bob.id
    assert (
        client.patch(
            f"/tasks/{task['id']}", json={"status": "done"}, headers=bob.headers
        ).status_code
        == 403
    )


def test_projects_of_different_users_are_independent(client, alice, bob):
    alice_project = make_project(client, alice, "Alice's")
    bob_project = make_project(client, bob, "Bob's")

    assert client.delete(f"/projects/{bob_project}", headers=bob.headers).status_code == 204
    # Deleting Bob's project left Alice's alone.
    assert client.delete(f"/projects/{alice_project}", headers=alice.headers).status_code == 204


def test_a_project_that_does_not_exist_is_404_for_everyone(client, alice):
    response = client.delete(f"/projects/{MISSING_ID}", headers=alice.headers)
    assert response.status_code == 404
    assert response.json()["error"]["code"] == "PROJECT_NOT_FOUND"


def test_emails_are_case_insensitive(client, register_user):
    register_user("Mixed.Case@Example.COM")

    # The same address in a different case is the same account.
    duplicate = client.post(
        "/auth/register",
        json={"email": "mixed.case@example.com", "password": "correct-horse-battery-staple"},
    )
    assert duplicate.status_code == 409

    login = client.post(
        "/auth/login",
        json={"email": "MIXED.CASE@EXAMPLE.COM", "password": "correct-horse-battery-staple"},
    )
    assert login.status_code == 200
