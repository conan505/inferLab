"""SQLAlchemy models.

The schema mirrors the domain model one-to-one. Constraints that protect an
invariant are declared here rather than left to the application, so a future
code path cannot write a row the rest of the system is unable to read.
"""

import uuid
from datetime import datetime, timezone
from typing import Optional

from sqlalchemy import Enum, ForeignKey, Index, String, text
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column, relationship

from app.db.types import UtcDateTime
from app.domain.task_status import DEFAULT_TASK_STATUS, TaskStatus


class Base(DeclarativeBase):
    pass


def new_id() -> str:
    """Opaque, unguessable primary key.

    UUIDs rather than auto-increment integers: ids appear in URLs, and
    sequential ids would let a caller enumerate how many projects exist and
    probe for ones they do not own.
    """
    return str(uuid.uuid4())


def utcnow() -> datetime:
    return datetime.now(timezone.utc)


class User(Base):
    __tablename__ = "users"

    id: Mapped[str] = mapped_column(String(36), primary_key=True, default=new_id)
    # Uniqueness is enforced by the database, not by the "does this email
    # already exist?" check in the service — that check is only a fast path.
    email: Mapped[str] = mapped_column(String(254), nullable=False, unique=True, index=True)
    hashed_password: Mapped[str] = mapped_column(String(255), nullable=False)
    created_at: Mapped[datetime] = mapped_column(UtcDateTime, nullable=False, default=utcnow)


class Project(Base):
    __tablename__ = "projects"

    id: Mapped[str] = mapped_column(String(36), primary_key=True, default=new_id)
    name: Mapped[str] = mapped_column(String(200), nullable=False)
    # Deleting a user removes their projects and, transitively, those projects' tasks.
    owner_id: Mapped[str] = mapped_column(
        String(36), ForeignKey("users.id", ondelete="CASCADE"), nullable=False
    )
    created_at: Mapped[datetime] = mapped_column(UtcDateTime, nullable=False, default=utcnow)

    # Declared for readability. The cascade is performed by the database
    # (ON DELETE CASCADE); passive_deletes stops the ORM from loading every task
    # into memory just to delete it.
    tasks: Mapped[list["Task"]] = relationship(
        back_populates="project", cascade="all, delete", passive_deletes=True
    )

    __table_args__ = (
        # Every project lookup is "the projects owned by this user".
        Index("ix_projects_owner_id", "owner_id"),
    )


class Task(Base):
    __tablename__ = "tasks"

    id: Mapped[str] = mapped_column(String(36), primary_key=True, default=new_id)
    project_id: Mapped[str] = mapped_column(
        String(36), ForeignKey("projects.id", ondelete="CASCADE"), nullable=False
    )
    title: Mapped[str] = mapped_column(String(200), nullable=False)
    description: Mapped[Optional[str]] = mapped_column(String(5000), nullable=True)
    # native_enum=False + create_constraint=True renders a VARCHAR column with a
    # CHECK constraint, so the three legal values are enforced by the database
    # as well as by the request schema.
    status: Mapped[TaskStatus] = mapped_column(
        Enum(
            TaskStatus,
            name="task_status",
            native_enum=False,
            create_constraint=True,
            validate_strings=True,
            values_callable=lambda enum_cls: [member.value for member in enum_cls],
        ),
        nullable=False,
        default=DEFAULT_TASK_STATUS,
        server_default=text("'todo'"),
    )
    # A task outlives the person assigned to it: unassign rather than cascade.
    assignee_id: Mapped[Optional[str]] = mapped_column(
        String(36), ForeignKey("users.id", ondelete="SET NULL"), nullable=True
    )
    due_date: Mapped[Optional[datetime]] = mapped_column(UtcDateTime, nullable=True)
    created_at: Mapped[datetime] = mapped_column(UtcDateTime, nullable=False, default=utcnow)
    updated_at: Mapped[datetime] = mapped_column(
        UtcDateTime, nullable=False, default=utcnow, onupdate=utcnow
    )

    project: Mapped[Project] = relationship(back_populates="tasks")

    __table_args__ = (
        # Serves both "list a project's tasks" and the delete guard's
        # "does this project have an in_progress task?" probe.
        Index("ix_tasks_project_id_status", "project_id", "status"),
        # Partial index: "tasks assigned to me" without paying for unassigned rows.
        Index(
            "ix_tasks_assignee_id",
            "assignee_id",
            sqlite_where=text("assignee_id IS NOT NULL"),
            postgresql_where=text("assignee_id IS NOT NULL"),
        ),
    )
