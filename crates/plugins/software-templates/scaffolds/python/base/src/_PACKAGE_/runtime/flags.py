"""This service's feature flags and runtime configuration, read from DOC.

Everything that applies to the service is read in one call and kept here; the call carries an ETag,
so a poll that finds nothing new costs a 304 and nothing else. Every read names the value to fall
back to, so a service whose flags cannot be reached keeps running on its own defaults.
"""

from __future__ import annotations

import logging
import threading
from typing import Any, TypeVar

import httpx

from .config import Config

logger = logging.getLogger(__name__)
T = TypeVar("T")


class Flags:
    """The flags and settings DOC holds for this service, kept up to date in the background."""

    def __init__(self, config: Config) -> None:
        self._config = config
        self._client = httpx.Client(timeout=5.0)
        self._lock = threading.Lock()
        self._values: dict[str, Any] = {}
        self._version = ""
        self._refresh = config.flags_poll
        self._stop = threading.Event()

    @classmethod
    def start(cls, config: Config) -> "Flags":
        """Reads them once, then keeps them up to date until the process ends."""
        flags = cls(config)
        flags.read()
        thread = threading.Thread(target=flags._poll, name="doc-flags", daemon=True)
        thread.start()
        return flags

    def stop(self) -> None:
        self._stop.set()
        self._client.close()

    def _poll(self) -> None:
        while not self._stop.wait(self._refresh):
            self.read()

    def read(self) -> None:
        """One read. A 304 means nothing has changed, and costs nothing else."""
        headers = {"if-none-match": f'"{self._version}"'} if self._version else {}
        if self._config.flags_token:
            headers["authorization"] = f"Bearer {self._config.flags_token}"
        try:
            answer = self._client.get(
                self._config.flags_url,
                params={"service": self._config.service, "environment": self._config.environment},
                headers=headers,
            )
            if answer.status_code == 304:
                return
            answer.raise_for_status()
            read = answer.json()
        except Exception as problem:  # noqa: BLE001 - the service keeps running on its defaults
            logger.warning("the flags could not be read: %s", problem)
            return

        with self._lock:
            self._values = dict(read.get("flags", {})) | dict(read.get("config", {}))
            self._version = read.get("version", "")
            self._refresh = float(read.get("refresh_seconds") or self._config.flags_poll)

    def _value(self, key: str) -> Any:
        with self._lock:
            return self._values.get(key)

    def boolean(self, key: str, fallback: bool) -> bool:
        """A switch: on, off, or the fallback when the platform holds nothing for this service."""
        value = self._value(key)
        return value if isinstance(value, bool) else fallback

    def string(self, key: str, fallback: str) -> str:
        """A value read while the service runs, such as a message or a mode."""
        value = self._value(key)
        return value if isinstance(value, str) else fallback

    def number(self, key: str, fallback: float) -> float:
        """A number read while the service runs, such as a limit or a timeout."""
        value = self._value(key)
        return float(value) if isinstance(value, (int, float)) and not isinstance(value, bool) else fallback

    def json(self, key: str, fallback: T) -> T | Any:
        """A structured value, or the fallback."""
        value = self._value(key)
        return fallback if value is None else value
