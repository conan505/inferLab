"""Engine and session construction.

The engine is built by a factory and injected, rather than being created as an
import-time module global. That is what lets the test suite run each test
against its own isolated in-memory database without touching global state.
"""

from pathlib import Path
from typing import Iterator

from sqlalchemy import Engine, create_engine, event
from sqlalchemy.engine import make_url
from sqlalchemy.orm import Session, sessionmaker
from sqlalchemy.pool import StaticPool

from app.db.models import Base


def _install_sqlite_pragmas(engine: Engine, *, in_memory: bool) -> None:
    @event.listens_for(engine, "connect")
    def _set_pragmas(dbapi_connection, _connection_record):  # pragma: no cover - trivial
        cursor = dbapi_connection.cursor()
        # SQLite disables foreign keys per connection by default. Without this,
        # every FOREIGN KEY and ON DELETE rule in the schema is inert — which is
        # the single easiest way to ship a "SQLite has no constraints" bug.
        cursor.execute("PRAGMA foreign_keys=ON")
        # Wait for a busy writer instead of failing the request immediately.
        cursor.execute("PRAGMA busy_timeout=5000")
        if not in_memory:
            # Readers no longer block behind a writer.
            cursor.execute("PRAGMA journal_mode=WAL")
        cursor.close()


def create_engine_from_url(url: str, *, echo: bool = False) -> Engine:
    sa_url = make_url(url)
    is_sqlite = sa_url.get_backend_name() == "sqlite"
    in_memory = is_sqlite and sa_url.database in (None, "", ":memory:")

    kwargs = {"echo": echo, "future": True}
    if is_sqlite:
        # Pooled connections are handed between the threadpool workers that run
        # FastAPI's synchronous endpoints (never concurrently on one connection).
        kwargs["connect_args"] = {"check_same_thread": False}
        if in_memory:
            # Keep one connection for the whole process, otherwise each session
            # would get a fresh — and therefore empty — in-memory database.
            kwargs["poolclass"] = StaticPool
        else:
            Path(sa_url.database).expanduser().resolve().parent.mkdir(
                parents=True, exist_ok=True
            )

    engine = create_engine(url, **kwargs)
    if is_sqlite:
        _install_sqlite_pragmas(engine, in_memory=in_memory)
    return engine


def create_session_factory(engine: Engine) -> sessionmaker:
    # expire_on_commit=False keeps ORM objects readable after the request's
    # transaction commits, so a handler can still serialise what it just wrote.
    return sessionmaker(bind=engine, autoflush=False, expire_on_commit=False, future=True)


def init_database(engine: Engine) -> None:
    """Create the schema if it does not exist.

    Adequate for a service with a single, additive schema; the migration story
    for a longer-lived deployment is discussed in the README.
    """
    Base.metadata.create_all(engine)


def session_scope(session_factory: sessionmaker) -> Iterator[Session]:
    """Transaction-per-request: commit on success, roll back on any exception.

    Handlers never call ``commit`` themselves, so a request either applies
    completely or not at all.
    """
    session = session_factory()
    try:
        yield session
        session.commit()
    except Exception:
        session.rollback()
        raise
    finally:
        session.close()
