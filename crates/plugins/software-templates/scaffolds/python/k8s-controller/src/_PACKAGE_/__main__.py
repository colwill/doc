"""{{ values.name }} — {{ values.description }}

The controller itself is in `controller.py`; kopf finds its handlers by importing it.
"""

from __future__ import annotations

import logging

import kopf

from . import controller  # noqa: F401 - importing it is what registers the handlers
from .runtime import Config, Flags, start_telemetry


def main() -> None:
    config = Config.load()
    shutdown = start_telemetry(config)
    controller.PLATFORM.config = config
    controller.PLATFORM.flags = Flags.start(config)
    logging.getLogger(__name__).info(
        "watching {{ scaffold.kind }}s in {{ scaffold.group }} as %s", config.service
    )
    try:
        kopf.run(clusterwide=True, liveness_endpoint="http://0.0.0.0:8080/healthz")
    finally:
        if controller.PLATFORM.flags is not None:
            controller.PLATFORM.flags.stop()
        shutdown()


if __name__ == "__main__":
    main()
