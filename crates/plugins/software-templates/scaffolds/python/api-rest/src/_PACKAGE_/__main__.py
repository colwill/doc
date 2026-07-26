"""{{ values.name }} — {{ values.description }}"""

from __future__ import annotations

import uvicorn

from .api import build
from .runtime import Config, Flags, start_telemetry


def main() -> None:
    config = Config.load()
    shutdown = start_telemetry(config)
    flags = Flags.start(config)
    try:
        uvicorn.run(
            build(config, flags),
            host=config.address,
            port=config.port,
            log_config=None,
            access_log=False,
        )
    finally:
        flags.stop()
        shutdown()


if __name__ == "__main__":
    main()
