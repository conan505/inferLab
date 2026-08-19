"""All persistence for the ``users`` table."""

from typing import Optional

from sqlalchemy import select
from sqlalchemy.orm import Session

from app.db.models import User


class UsersRepository:
    def __init__(self, session: Session) -> None:
        self._session = session

    def create(self, *, email: str, hashed_password: str) -> User:
        user = User(email=email, hashed_password=hashed_password)
        self._session.add(user)
        # Flush (not commit) so the row — and any unique-constraint violation —
        # materialises now, while the request transaction is still open.
        self._session.flush()
        return user

    def find_by_email(self, email: str) -> Optional[User]:
        return self._session.scalars(select(User).where(User.email == email)).one_or_none()

    def find_by_id(self, user_id: str) -> Optional[User]:
        return self._session.get(User, user_id)

    def exists(self, user_id: str) -> bool:
        return (
            self._session.scalar(select(User.id).where(User.id == user_id).limit(1))
            is not None
        )
