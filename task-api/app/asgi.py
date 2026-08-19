"""Uvicorn entrypoint: ``uvicorn app.asgi:app``."""

import logging

from app.config import get_settings
from app.main import create_app

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(name)s %(message)s",
)

app = create_app(settings=get_settings())
