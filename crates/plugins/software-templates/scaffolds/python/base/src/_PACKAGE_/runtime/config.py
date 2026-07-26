"""What the service reads from its environment at start-up. Anything that should change without a
deployment belongs in `Flags` instead, which is read while it runs."""

from __future__ import annotations

import os
from dataclasses import dataclass


@dataclass(frozen=True)
class Config:
    service: str
    environment: str
    address: str
    port: int
    flags_url: str
    flags_token: str | None
    flags_poll: float

    @classmethod
    def load(cls) -> "Config":
        """The environment, falling back to what DOC knew when this service was created."""
        return cls(
            service=text("OTEL_SERVICE_NAME", "{{ values.name }}"),
            environment=text("DOC_ENVIRONMENT", "{{ telemetry.environment }}"),
            address=text("ADDRESS", "0.0.0.0"),
            port=number("PORT", 8080),
            flags_url=text("DOC_FLAGS_URL", "{{ flags.url }}"),
            flags_token=os.environ.get("DOC_FLAGS_TOKEN") or None,
            flags_poll=float(number("DOC_FLAGS_POLL_SECONDS", 30)),
        )


def text(name: str, fallback: str) -> str:
    return os.environ.get(name) or fallback


def number(name: str, fallback: int) -> int:
    try:
        return int(os.environ[name])
    except (KeyError, ValueError):
        return fallback
