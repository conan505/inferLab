"""Custom column types."""

from datetime import datetime, timezone

from sqlalchemy import DateTime
from sqlalchemy.types import TypeDecorator


class UtcDateTime(TypeDecorator):
    """A timestamp that is always timezone-aware UTC in Python.

    SQLite has no native timestamp type and discards timezone information, so a
    naive ``datetime`` read back from the database would silently be interpreted
    in local time. This type normalises on the way in, rejects naive values
    outright, and re-attaches UTC on the way out — so application code never has
    to reason about which side of the boundary a value came from.
    """

    impl = DateTime
    cache_ok = True

    def process_bind_param(self, value, dialect):
        if value is None:
            return None
        if not isinstance(value, datetime):
            raise TypeError(f"expected datetime, got {type(value).__name__}")
        if value.tzinfo is None:
            raise ValueError("naive datetimes are not accepted; use timezone-aware UTC")
        return value.astimezone(timezone.utc).replace(tzinfo=None)

    def process_result_value(self, value, dialect):
        if value is None:
            return None
        if value.tzinfo is None:
            return value.replace(tzinfo=timezone.utc)
        return value.astimezone(timezone.utc)
