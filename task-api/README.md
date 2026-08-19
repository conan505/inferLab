# Task Management API

A REST service where authenticated users own projects and manage tasks inside
them. Built with FastAPI, SQLAlchemy 2.0 and SQLite.

---

## Quickstart

```bash
python3 -m venv .venv && source .venv/bin/activate
pip install -e ".[dev]"          # or: pip install -r requirements.txt

cp .env.example .env
python -c "import secrets; print(secrets.token_urlsafe(48))"   # paste into JWT_SECRET

uvicorn app.asgi:app --reload
```

The service refuses to start without a `JWT_SECRET` of at least 32 characters —
a missing secret is a deployment bug, not something to paper over with a default.

Interactive docs: <http://localhost:8000/docs>. Liveness: `GET /health`.

```bash
pytest            # 154 tests, ~1s
pytest tests/unit
pytest tests/integration
```

---

## API surface

Every endpoint except the two under `/auth` requires `Authorization: Bearer <token>`.

| Method | Path | Purpose | Success |
|---|---|---|---|
| POST | `/auth/register` | Create an account, receive a token | `201` |
| POST | `/auth/login` | Exchange credentials for a token | `200` |
| POST | `/projects` | Create a project owned by the caller | `201` |
| POST | `/projects/{project_id}/tasks` | Create a task in a project you own | `201` |
| PATCH | `/tasks/{task_id}` | Partially update a task in a project you own | `200` |
| DELETE | `/projects/{project_id}` | Delete a project you own | `204` |

> The brief writes the nested route as `/project/:id/tasks`. It is implemented as
> `/projects/{project_id}/tasks` so the collection name is consistent across the API.

Read endpoints (`GET /projects`, `GET /projects/{id}/tasks`) are intentionally
out of scope — the brief names six endpoints, and adding more would have meant
pagination and filtering decisions that were not asked for.

### Worked example

```bash
TOKEN=$(curl -s -X POST localhost:8000/auth/register \
  -H 'Content-Type: application/json' \
  -d '{"email":"ada@example.com","password":"correct-horse-battery-staple"}' \
  | python -c 'import sys,json; print(json.load(sys.stdin)["access_token"])')

PROJECT=$(curl -s -X POST localhost:8000/projects \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"name":"Apollo"}' | python -c 'import sys,json; print(json.load(sys.stdin)["id"])')

curl -s -X POST localhost:8000/projects/$PROJECT/tasks \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"title":"Ship it","due_date":"2026-12-01"}'
```

---

## Architecture

```
app/api/routes/     HTTP: paths, status codes, headers
app/api/schemas.py  the wire contract (Pydantic) — separate from the ORM models
app/services/       business rules; knows nothing about HTTP
app/repositories/   all SQL; knows nothing about business rules
app/domain/         pure rules shared by the above (status enum, ownership)
app/db/             engine, session, ORM models
app/main.py         composition root — the only place that wires the above together
```

Dependencies are passed in, never imported as module-level singletons. That is
what lets each test build its own app around its own in-memory database, and
what keeps `app/services/` importable without FastAPI.

Two rules are pulled out into `app/domain/` because they must hold everywhere
and are easiest to trust when they are pure functions over plain data:
`assert_project_owned_by` / `assert_task_owned_by`, and the `TaskStatus` enum.

---

## Schema design rationale

```
users ──< projects ──< tasks
              ▲            │
              └── assignee ┘
```

**UUID primary keys, not autoincrement integers.** Ids appear in URLs.
Sequential integers would let a caller count how many projects exist and probe
for ids they do not own; the ownership check would still reject them, but the
existence of the resource is itself information.

**The status enum is defined once** (`app/domain/task_status.py`) and reused
three times: as the Pydantic field type (which produces the `400` for a bad
value), as the ORM column type, and — via `native_enum=False,
create_constraint=True` — as a database `CHECK (status IN ('todo',
'in_progress', 'done'))`. Validation at the edge is what users see; the CHECK
constraint is what stops a future bug in some other code path writing a status
the rest of the system cannot read.

**Foreign keys carry deliberate delete behaviour.** `projects.owner_id` and
`tasks.project_id` cascade: a project has no meaning without its owner, and a
task has none without its project. `tasks.assignee_id` is `ON DELETE SET NULL`
— work outlives the person assigned to it. Because SQLite disables foreign keys
per connection by default, `PRAGMA foreign_keys=ON` is set in a connect-event
listener; without it every rule above would be silently inert.

**Indexes follow the actual queries**, not every column:
`projects(owner_id)` for "the projects owned by this user";
`tasks(project_id, status)` which serves both listing a project's tasks and the
deletion guard's "does this project have an `in_progress` task?" probe; and a
partial index on `tasks(assignee_id) WHERE assignee_id IS NOT NULL`, so
unassigned tasks cost nothing to index.

**Timestamps are timezone-aware UTC on both sides of the boundary.** SQLite has
no native timestamp type and discards offsets, so a custom `UtcDateTime` type
normalises on write, rejects naive datetimes outright, and re-attaches UTC on
read. Responses serialise as ISO-8601 with a `Z` suffix.

**Request/response models are separate from ORM models.** A column cannot leak
into a response unless it is declared in `app/api/schemas.py` — which is the
structural reason `hashed_password` can never be serialised, rather than a rule
someone has to remember.

---

## Authentication and authorisation

**Passwords** are hashed with bcrypt at cost factor 12 (configurable; the test
suite drops to 4). bcrypt refuses input over 72 bytes, so the request schema
enforces the same limit — an over-long password is a clean `400` rather than a
`500` from the hashing library.

**Login does not leak which accounts exist.** An unknown email and a wrong
password return an identical code and message, and the unknown-email path still
performs a bcrypt comparison against a decoy hash so the two take comparable
time. There is an integration test asserting the two response bodies are equal.

**Tokens** are stateless HS256 JWTs carrying `sub`, `email`, `iss`, `iat` and
`exp`, with a 15-minute default lifetime. On verification the algorithm list is
pinned and `exp`/`iat`/`sub`/`iss` are required — without pinning, a token whose
header claims `alg: none` would be accepted, which is the classic JWT bypass and
is covered by a test.

**Identity is re-resolved from the database on every request.** The only thing
trusted from a token is the subject; the user row is then loaded fresh. This
costs one indexed lookup per request and buys the ability to revoke: a deleted
account's still-unexpired token stops working immediately.

**Authorisation is ownership, checked in the service layer.** Every project
access goes through `ProjectsService.get_owned`, and task access resolves the
task's project owner in the same query that fetches the task. Authentication is
attached at the *router*, not per route, so adding an endpoint cannot
accidentally leave it public.

---

## Error model

Every error — from a handler, a dependency or middleware — has the same shape:

```json
{
  "error": {
    "code": "PROJECT_HAS_BLOCKING_TASKS",
    "message": "Project cannot be deleted while 1 of its task(s) have status: in_progress",
    "details": { "blocking_statuses": ["in_progress"], "blocking_task_count": 1 }
  },
  "request_id": "9f1c…"
}
```

`code` is the stable, machine-readable part; `message` is for humans; `details`
is present only when there is structured context. `request_id` is generated per
request (or taken from an inbound `X-Request-Id`), echoed as a response header,
and included in every error body — so a user-reported failure maps to a log line.

| Status | Codes | When |
|---|---|---|
| 400 | `VALIDATION_ERROR`, `MALFORMED_JSON` | Schema violation, unknown field, unparseable body, malformed id in path |
| 401 | `MISSING_TOKEN`, `MALFORMED_AUTH_HEADER`, `INVALID_TOKEN`, `TOKEN_EXPIRED`, `INVALID_CREDENTIALS` | Sent with `WWW-Authenticate: Bearer` |
| 403 | `NOT_PROJECT_OWNER` | Authenticated, but not the owner |
| 404 | `PROJECT_NOT_FOUND`, `TASK_NOT_FOUND`, `ROUTE_NOT_FOUND` | |
| 409 | `EMAIL_ALREADY_REGISTERED`, `PROJECT_HAS_BLOCKING_TASKS` | |
| 415 | `UNSUPPORTED_MEDIA_TYPE` | Body sent as something other than JSON |
| 422 | `ASSIGNEE_NOT_FOUND` | Well-formed, but references a row that does not exist |
| 429 | `TOO_MANY_REQUESTS` | Auth rate limit; sent with `Retry-After` |
| 500 | `INTERNAL_ERROR` | Logged in full, opaque to the client |

Unhandled exceptions are logged with a stack trace and reported as a bare 500.
No SQL, file path or stack trace crosses the boundary.

---

## Key tradeoffs

**1. The deletion guard is one SQL statement, not a check followed by a delete.**
This is the decision the rest of the design bends around. The obvious
implementation —

```python
if repo.count_in_progress(project_id) > 0:   # ← another request can slip in here
    raise ConflictError(...)
repo.delete(project_id)
```

— has a window in which a concurrent `PATCH` moves a task to `in_progress`
between the check and the delete. The task would then be cascaded away by
exactly the delete the rule exists to prevent. Instead the guard lives in the
statement's `WHERE` clause:

```sql
DELETE FROM projects
 WHERE id = ?
   AND NOT EXISTS (SELECT 1 FROM tasks
                    WHERE tasks.project_id = projects.id
                      AND tasks.status IN ('in_progress'))
```

`rowcount == 0` means blocked, and the service then counts the blocking tasks
purely to build the message. There is no window at all. The repository owns
atomicity; the service owns what the outcome *means*.

**2. `403` for a resource you do not own, not `404`.** Returning `404` would
hide whether the project exists. That is a real (small) information leak
accepted in exchange for an API that can be debugged — "you are not the owner"
and "it does not exist" are different problems for a client. Ownership is
checked *before* the in-progress rule, so a stranger gets `403` rather than
learning from a `409` that the project exists and has work in flight.

**3. Schema violations are `400`, not FastAPI's default `422`.** `422` is
reserved here for requests that are well-formed but reference something that
does not exist (`ASSIGNEE_NOT_FOUND`). This is a deliberate override of the
framework default, applied in one exception handler.

**4. PATCH distinguishes "omitted" from "explicitly null."** Handlers dump with
`exclude_unset=True`, so `{"status": "done"}` touches only `status`. `title` and
`status` map to `NOT NULL` columns and are therefore annotated non-Optional — a
patch may omit them but may not null them — while `description`, `assignee_id`
and `due_date` accept an explicit `null` meaning "clear this". `project_id` is
not patchable at all: re-parenting a task needs its own authorisation rule (you
would have to own both projects), so it is out of scope rather than half-done.

**5. SQLite, reached through a SQLAlchemy URL.** Zero setup, real transactions,
real constraints, and the test suite runs against an in-memory instance in about
a second. Because configuration is a `DATABASE_URL` and all SQL is confined to
`app/repositories/`, moving to PostgreSQL is a config change plus a driver — the
partial index and the conditional `DELETE` are both portable. What SQLite does
*not* give: a single writer at a time, and no `SELECT … FOR UPDATE`. Neither
matters here because the one contended operation is already a single statement.

**6. `create_all()` instead of migrations.** Honest for a service with one
additive schema and no production history. Anything long-lived needs Alembic;
the models are already structured for it.

**7. Rate limiting is in-process.** A fixed-window limiter on the two
unauthenticated routes, with no external dependency. It is genuine protection
for a single instance and *not* correct behind multiple replicas, where it needs
a shared store such as Redis.

**8. Registration returns a token.** The alternative is forcing every client to
immediately `POST /auth/login` with credentials it just sent. Access tokens are
short-lived and there is no refresh-token flow — with a 15-minute lifetime that
is a real usability cost, and the right next step rather than something to
improvise.

---

## Testing

154 tests, roughly one second, no network or external services.

**Unit** (`tests/unit/`) — the business rules in isolation:

- `test_task_status.py` — the enum has exactly three values; every other value is
  rejected by both the domain helper and the request schemas.
- `test_ownership.py` — missing → `404`, someone else's → `403`, owner → allowed.
- `test_projects_service.py` — the no-delete-in-progress rule against a real
  in-memory database: empty projects delete, `todo`/`done` tasks cascade, one
  `in_progress` task blocks, deletion succeeds once it moves on, and a non-owner
  of a busy project gets `403` rather than `409`. These run through the real
  repositories on purpose — the rule is partly expressed in SQL, so a fake
  repository would test the mock rather than the rule.
- `test_tasks_service.py` — ownership on create and update, assignee validation,
  partial updates leaving other fields alone.
- `test_security.py` — salting, expiry, wrong secret, wrong issuer, missing
  `exp`, and the `alg: none` bypass.
- `test_schemas.py`, `test_rate_limit.py`.

**Integration** (`tests/integration/`) — through the real ASGI app:

- `test_full_flow.py` — the flow the brief asks for: register → login → create
  project → create task → move it to `in_progress` → `DELETE` the project and
  get `409`, then assert the project and task survived, move the task to `done`,
  delete successfully, and confirm the task was cascaded away.
- `test_authorization.py` — a second user cannot create tasks in, update tasks
  in, or delete another user's project, and their failed attempts change nothing.
- `test_error_handling.py` — every status code in the table above.
- `test_rate_limit.py` — throttling end to end, including `Retry-After`.

---

## Known gaps

Named rather than hidden: no refresh tokens or logout (short-lived access tokens
only); no read endpoints; no pagination; no Alembic migrations; the rate limiter
is per-process; there is no structured request log, only a per-request id ready
for one; and `TRUST_PROXY` must stay `false` unless the service genuinely runs
behind a proxy, since otherwise a client could spoof `X-Forwarded-For` and evade
the limiter.
