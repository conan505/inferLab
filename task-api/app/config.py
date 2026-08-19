"""Application configuration.

Environment is parsed and validated exactly once, at import of ``get_settings``.
A misconfigured deployment then fails loudly on startup rather than at the first
request that happens to need the value.
"""

from functools import lru_cache
from typing import Literal

from pydantic import Field, field_validator
from pydantic_settings import BaseSettings, SettingsConfigDict


class Settings(BaseSettings):
    model_config = SettingsConfigDict(
        env_file=".env",
        env_file_encoding="utf-8",
        extra="ignore",
        case_sensitive=False,
    )

    environment: Literal["development", "test", "production"] = "development"
    port: int = Field(default=8000, ge=0, le=65535)

    # No default: refusing to boot without an explicit secret is the point.
    jwt_secret: str = Field(min_length=32)
    jwt_expires_minutes: int = Field(default=15, ge=1)
    jwt_issuer: str = "task-api"

    # A SQLAlchemy URL rather than a file path, so moving to PostgreSQL is a
    # configuration change rather than a code change.
    database_url: str = "sqlite:///./data/task-api.db"

    # bcrypt work factor. 12 is the current sensible default for production;
    # the test suite drops it to the minimum to stay fast.
    bcrypt_rounds: int = Field(default=12, ge=4, le=16)

    auth_rate_limit_max: int = Field(default=20, ge=1)
    auth_rate_limit_window_seconds: int = Field(default=60, ge=1)

    trust_proxy: bool = False

    @field_validator("jwt_secret")
    @classmethod
    def reject_placeholder_secret(cls, value: str) -> str:
        if value.startswith("replace-me"):
            raise ValueError("JWT_SECRET is still the placeholder from .env.example")
        return value


@lru_cache(maxsize=1)
def get_settings() -> Settings:
    """Cached accessor used as a FastAPI dependency and by the entrypoint."""
    return Settings()
